use std::{
    cmp::Ordering,
    collections::HashMap,
    ffi::CString,
    fs::{self, File},
    io::{self, Seek, SeekFrom},
    os::unix::fs::FileExt,
    path::{Component, Path, PathBuf},
};

use crate::kexec::{self, KexecImage};

const SYS_BLOCK: &str = "/sys/block";
const DEV: &str = "/dev";
const BOOT_MOUNT_ROOT: &str = "/run/pocketboot/boot";
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_MIN_HEADER_SIZE: usize = 92;
const GPT_MIN_ENTRY_SIZE: usize = 128;
const GPT_TABLE_MAX_BYTES: u64 = 16 * 1024 * 1024;
const MBR_SIGNATURE: [u8; 2] = [0x55, 0xaa];
const MBR_PARTITION_TABLE_OFFSET: usize = 0x1be;
const MBR_PARTITION_ENTRY_SIZE: usize = 16;
const MBR_PRIMARY_PARTITIONS: usize = 4;
const GPT_PROTECTIVE_MBR_TYPE: u8 = 0xee;
const MBR_XBOOTLDR_TYPE: u8 = 0xea;
const DEFAULT_LOGICAL_BLOCK_SIZE: u64 = 512;
const MS_NOSYMFOLLOW: libc::c_ulong = 256;

const DEFAULT_BLS_DIR: &str = "loader/entries";
const GRUB_CFG_PATHS: [&str; 3] = [
    "grub2/grub.cfg",
    "EFI/fedora/grub.cfg",
    "efi/EFI/fedora/grub.cfg",
];
const GRUB_ENV_PATHS: [&str; 3] = [
    "grub2/grubenv",
    "EFI/fedora/grubenv",
    "efi/EFI/fedora/grubenv",
];
const GRUB_VARIABLE_KEYS: [&str; 6] = [
    "blsdir",
    "kernelopts",
    "next_entry",
    "saved_entry",
    "tuned_initrd",
    "tuned_params",
];
const GRUB_OPTIONAL_EMPTY_VARIABLES: [&str; 3] = ["grub_users", "tuned_initrd", "tuned_params"];

const ESP_GUID: Guid = Guid([
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
]);
const XBOOTLDR_GUID: Guid = Guid([
    0xff, 0xc2, 0x13, 0xbc, 0xe6, 0x59, 0x62, 0x42, 0xa3, 0x52, 0xb2, 0x75, 0xfd, 0x6f, 0x71, 0x72,
]);

#[derive(Clone, Debug)]
pub(crate) struct BootEntry {
    pub(crate) id: String,
    pub(crate) title: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) architecture: Option<String>,
    pub(crate) source: PathBuf,
    pub(crate) role: BootPartitionRole,
    pub(crate) disk: String,
    pub(crate) partition: String,
    pub(crate) preferred: bool,
    linux: PathBuf,
    initrds: Vec<PathBuf>,
    options: Vec<String>,
}

impl BootEntry {
    pub(crate) fn is_directly_bootable(&self) -> bool {
        !self
            .cmdline()
            .split_whitespace()
            .any(|arg| arg.starts_with('$'))
    }

    pub(crate) fn cmdline(&self) -> String {
        self.options.join(" ")
    }

    pub(crate) fn load(&self) -> io::Result<()> {
        let kernel = File::open(&self.linux).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("open kernel {}: {err}", self.linux.display()),
            )
        })?;
        let initrd = open_initrd_payload(&self.initrds)?;
        let image = KexecImage::new(kernel, initrd, None, &self.cmdline())?;
        image.load()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BootPartitionRole {
    Xbootldr,
    Esp,
}

impl BootPartitionRole {
    fn priority(self) -> u8 {
        match self {
            Self::Xbootldr => 0,
            Self::Esp => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Xbootldr => "xbootldr",
            Self::Esp => "esp",
        }
    }
}

pub(crate) fn discover() -> io::Result<Vec<BootEntry>> {
    let mut partitions = discover_boot_partitions()?;
    partitions.sort_by(compare_partition_candidates);

    let mut entries = Vec::new();
    for partition in partitions {
        match mount_partition(&partition) {
            Ok(mount) => {
                tracing::info!(
                    disk = %partition.disk,
                    partition = %partition.partition,
                    role = partition.role.label(),
                    path = %partition.dev_path.display(),
                    mount = %mount.root.display(),
                    fstype = mount.fstype,
                    "mounted boot partition candidate"
                );
                let context = BlsContext::from_boot_root(&mount.root);
                entries.extend(scan_bls_entries(&partition, &mount, &context));
            }
            Err(err) => {
                tracing::warn!(
                    disk = %partition.disk,
                    partition = %partition.partition,
                    role = partition.role.label(),
                    path = %partition.dev_path.display(),
                    error = ?err,
                    "failed to mount boot partition candidate"
                );
            }
        }
    }

    entries.sort_by(compare_boot_entries);
    Ok(entries)
}

fn open_initrd_payload(initrds: &[PathBuf]) -> io::Result<Option<File>> {
    match initrds {
        [] => Ok(None),
        [path] => File::open(path).map(Some).map_err(|err| {
            io::Error::new(err.kind(), format!("open initrd {}: {err}", path.display()))
        }),
        paths => {
            let mut payload = kexec::create_payload_memfd("bls-initrd")?;
            for path in paths {
                let mut initrd = File::open(path).map_err(|err| {
                    io::Error::new(err.kind(), format!("open initrd {}: {err}", path.display()))
                })?;
                io::copy(&mut initrd, &mut payload).map_err(|err| {
                    io::Error::new(
                        err.kind(),
                        format!("append initrd {}: {err}", path.display()),
                    )
                })?;
            }
            payload.seek(SeekFrom::Start(0))?;
            kexec::reopen_payload_readonly(payload).map(Some)
        }
    }
}

#[derive(Clone, Debug)]
struct BootPartitionCandidate {
    disk: String,
    partition: String,
    partno: u32,
    role: BootPartitionRole,
    dev_path: PathBuf,
    removable: bool,
}

#[derive(Clone, Debug)]
struct MountedPartition {
    root: PathBuf,
    fstype: &'static str,
}

#[derive(Clone, Debug, Default)]
struct BlsContext {
    variables: HashMap<String, String>,
}

impl BlsContext {
    fn from_boot_root(root: &Path) -> Self {
        let mut context = Self::default();
        for relative in GRUB_CFG_PATHS {
            context.load_assignments(root, relative, parse_grub_set_assignments);
        }
        for relative in GRUB_ENV_PATHS {
            context.load_assignments(root, relative, parse_grubenv_assignments);
        }
        context
    }

    fn load_assignments(
        &mut self,
        root: &Path,
        relative: &str,
        parse: fn(&str) -> Vec<(String, String)>,
    ) {
        let path = root.join(relative);
        let Ok(text) = fs::read_to_string(&path) else {
            return;
        };
        for (key, value) in parse(&text) {
            if !is_grub_variable_key(&key) {
                continue;
            }
            let value = self.expand(&value);
            self.variables.insert(key, value);
        }
        tracing::debug!(path = %path.display(), "loaded Fedora GRUB variables");
    }

    fn entries_dir(&self, root: &Path) -> io::Result<PathBuf> {
        let blsdir = self
            .variables
            .get("blsdir")
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_BLS_DIR);
        resolve_boot_path(root, &self.expand(blsdir))
    }

    fn preferred_entry(&self) -> Option<&str> {
        self.variables
            .get("next_entry")
            .filter(|value| !value.is_empty())
            .or_else(|| self.variables.get("saved_entry"))
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    fn expand(&self, value: &str) -> String {
        expand_grub_vars(value, &self.variables)
    }
}

fn discover_boot_partitions() -> io::Result<Vec<BootPartitionCandidate>> {
    let mut candidates = Vec::new();
    for disk in local_disk_candidates()? {
        match gpt_boot_partitions(&disk) {
            Ok(partitions) => candidates.extend(partitions),
            Err(err) => {
                tracing::debug!(disk = %disk.name, error = ?err, "disk is not usable GPT boot media");
            }
        }
        match mbr_boot_partitions(&disk) {
            Ok(partitions) => candidates.extend(partitions),
            Err(err) => {
                tracing::debug!(disk = %disk.name, error = ?err, "disk is not usable MBR boot media");
            }
        }
    }
    Ok(candidates)
}

fn local_disk_candidates() -> io::Result<Vec<DiskCandidate>> {
    let mut disks = Vec::new();
    let entries = fs::read_dir(SYS_BLOCK)?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let sysfs_path = entry.path();
        if !is_local_disk_name(&name) || is_virtual_block(&sysfs_path) {
            continue;
        }

        let dev_path = Path::new(DEV).join(&name);
        if !dev_path.exists() {
            continue;
        }

        disks.push(DiskCandidate {
            removable: read_trimmed(sysfs_path.join("removable")).as_deref() == Some("1"),
            logical_block_size: read_trimmed(sysfs_path.join("queue/logical_block_size"))
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value >= 512 && value.is_power_of_two())
                .unwrap_or(DEFAULT_LOGICAL_BLOCK_SIZE),
            sectors_512: read_trimmed(sysfs_path.join("size"))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
            name,
            sysfs_path,
            dev_path,
        });
    }

    disks.sort_by(|left, right| {
        left.removable
            .cmp(&right.removable)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(disks)
}

#[derive(Clone, Debug)]
struct DiskCandidate {
    name: String,
    sysfs_path: PathBuf,
    dev_path: PathBuf,
    logical_block_size: u64,
    sectors_512: u64,
    removable: bool,
}

fn gpt_boot_partitions(disk: &DiskCandidate) -> io::Result<Vec<BootPartitionCandidate>> {
    let total_bytes = disk
        .sectors_512
        .checked_mul(512)
        .ok_or_else(|| invalid_data(format!("disk {} byte size overflows", disk.name)))?;
    if total_bytes < disk.logical_block_size.checked_mul(2).unwrap_or(u64::MAX) {
        return Err(invalid_data(format!(
            "disk {} is too small for GPT",
            disk.name
        )));
    }

    let total_blocks = total_bytes / disk.logical_block_size;
    if total_blocks < 2 {
        return Err(invalid_data(format!(
            "disk {} has too few GPT LBAs",
            disk.name
        )));
    }

    let file = File::open(&disk.dev_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("open {}: {err}", disk.dev_path.display()),
        )
    })?;
    validate_protective_mbr(&file)?;

    let header_block = read_lba(&file, disk.logical_block_size, 1)?;
    let header = parse_gpt_header(&header_block, total_blocks)?;
    let table_bytes = (header.entry_count as u64)
        .checked_mul(header.entry_size as u64)
        .ok_or_else(|| invalid_data("GPT partition table size overflows"))?;
    if table_bytes == 0 || table_bytes > GPT_TABLE_MAX_BYTES {
        return Err(invalid_data(format!(
            "GPT partition table size {table_bytes} is unsupported"
        )));
    }

    let table_offset = header
        .entries_lba
        .checked_mul(disk.logical_block_size)
        .ok_or_else(|| invalid_data("GPT partition table offset overflows"))?;
    if table_offset.checked_add(table_bytes).unwrap_or(u64::MAX) > total_bytes {
        return Err(invalid_data("GPT partition table exceeds disk size"));
    }

    let mut table = vec![
        0;
        usize::try_from(table_bytes).map_err(|_| {
            invalid_data("GPT partition table is too large for this platform")
        })?
    ];
    read_exact_at(&file, &mut table, table_offset)?;
    let table_crc = crc32_ieee(&table);
    if table_crc != header.entry_array_crc32 {
        return Err(invalid_data("GPT partition entry array CRC mismatch"));
    }

    let mut candidates = Vec::new();
    for index in 0..header.entry_count {
        let offset = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(header.entry_size as usize))
            .ok_or_else(|| invalid_data("GPT partition entry offset overflows"))?;
        let end = offset
            .checked_add(header.entry_size as usize)
            .ok_or_else(|| invalid_data("GPT partition entry end overflows"))?;
        let raw = table
            .get(offset..end)
            .ok_or_else(|| invalid_data("GPT partition entry exceeds table"))?;
        let entry = parse_gpt_entry(raw, index + 1)?;
        if entry.is_unused() {
            continue;
        }
        if entry.first_lba < header.first_usable_lba
            || entry.last_lba > header.last_usable_lba
            || entry.last_lba >= total_blocks
            || entry.last_lba < entry.first_lba
        {
            tracing::debug!(disk = %disk.name, partno = entry.partno, "skipping invalid GPT partition entry");
            continue;
        }

        let Some(role) = boot_role_for_guid(entry.type_guid) else {
            continue;
        };
        let Some(partition) = partition_name_for_partno(&disk.sysfs_path, entry.partno)? else {
            tracing::debug!(
                disk = %disk.name,
                partno = entry.partno,
                role = role.label(),
                "kernel partition device is missing for GPT boot partition"
            );
            continue;
        };

        candidates.push(BootPartitionCandidate {
            dev_path: Path::new(DEV).join(&partition),
            disk: disk.name.clone(),
            partition,
            partno: entry.partno,
            role,
            removable: disk.removable,
        });
    }

    Ok(candidates)
}

fn mbr_boot_partitions(disk: &DiskCandidate) -> io::Result<Vec<BootPartitionCandidate>> {
    let file = File::open(&disk.dev_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("open {}: {err}", disk.dev_path.display()),
        )
    })?;
    let mut mbr = [0; 512];
    read_exact_at(&file, &mut mbr, 0)?;
    let entries = parse_mbr_boot_entries(&mbr, disk.sectors_512)?;

    let mut candidates = Vec::new();
    for entry in entries {
        let Some(partition) = partition_name_for_partno(&disk.sysfs_path, entry.partno)? else {
            tracing::debug!(
                disk = %disk.name,
                partno = entry.partno,
                role = BootPartitionRole::Xbootldr.label(),
                "kernel partition device is missing for MBR boot partition"
            );
            continue;
        };

        candidates.push(BootPartitionCandidate {
            dev_path: Path::new(DEV).join(&partition),
            disk: disk.name.clone(),
            partition,
            partno: entry.partno,
            role: BootPartitionRole::Xbootldr,
            removable: disk.removable,
        });
    }

    Ok(candidates)
}

#[derive(Clone, Copy, Debug)]
struct MbrBootEntry {
    partno: u32,
}

fn parse_mbr_boot_entries(raw: &[u8], disk_sectors_512: u64) -> io::Result<Vec<MbrBootEntry>> {
    if raw.len() < 512 || raw[510..512] != MBR_SIGNATURE {
        return Err(invalid_data("missing MBR signature"));
    }

    let mut entries = Vec::new();
    for index in 0..MBR_PRIMARY_PARTITIONS {
        let offset = MBR_PARTITION_TABLE_OFFSET + index * MBR_PARTITION_ENTRY_SIZE;
        let entry = &raw[offset..offset + MBR_PARTITION_ENTRY_SIZE];
        if entry[4] != MBR_XBOOTLDR_TYPE {
            continue;
        }

        let start_lba = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as u64;
        let sector_count = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as u64;
        if start_lba == 0 || sector_count == 0 {
            tracing::debug!(
                partno = index + 1,
                "skipping empty MBR boot partition entry"
            );
            continue;
        }
        let end_lba = start_lba
            .checked_add(sector_count - 1)
            .ok_or_else(|| invalid_data("MBR boot partition end LBA overflows"))?;
        if disk_sectors_512 != 0 && end_lba >= disk_sectors_512 {
            tracing::debug!(
                partno = index + 1,
                start_lba,
                sector_count,
                disk_sectors_512,
                "skipping MBR boot partition entry outside disk"
            );
            continue;
        }

        entries.push(MbrBootEntry {
            partno: (index + 1) as u32,
        });
    }

    Ok(entries)
}

fn validate_protective_mbr(file: &File) -> io::Result<()> {
    let mut mbr = [0; 512];
    read_exact_at(file, &mut mbr, 0)?;
    if mbr[510..512] != MBR_SIGNATURE {
        return Err(invalid_data("missing MBR signature before GPT"));
    }

    for index in 0..MBR_PRIMARY_PARTITIONS {
        let offset = MBR_PARTITION_TABLE_OFFSET + index * MBR_PARTITION_ENTRY_SIZE;
        let entry = &mbr[offset..offset + MBR_PARTITION_ENTRY_SIZE];
        let part_type = entry[4];
        let start_lba = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]);
        if part_type == GPT_PROTECTIVE_MBR_TYPE && start_lba == 1 {
            return Ok(());
        }
    }

    Err(invalid_data("missing GPT protective MBR partition"))
}

#[derive(Clone, Copy, Debug)]
struct GptHeader {
    first_usable_lba: u64,
    last_usable_lba: u64,
    entries_lba: u64,
    entry_count: u32,
    entry_size: u32,
    entry_array_crc32: u32,
}

fn parse_gpt_header(raw: &[u8], total_blocks: u64) -> io::Result<GptHeader> {
    if raw.len() < GPT_MIN_HEADER_SIZE || &raw[..8] != GPT_SIGNATURE {
        return Err(invalid_data("GPT header signature not found"));
    }

    let header_size = read_u32_le(raw, 12)? as usize;
    if !(GPT_MIN_HEADER_SIZE..=raw.len()).contains(&header_size) {
        return Err(invalid_data("GPT header size is invalid"));
    }

    let expected_crc = read_u32_le(raw, 16)?;
    let mut header_for_crc = raw[..header_size].to_vec();
    header_for_crc[16..20].fill(0);
    let actual_crc = crc32_ieee(&header_for_crc);
    if actual_crc != expected_crc {
        return Err(invalid_data("GPT header CRC mismatch"));
    }

    let current_lba = read_u64_le(raw, 24)?;
    let backup_lba = read_u64_le(raw, 32)?;
    let first_usable_lba = read_u64_le(raw, 40)?;
    let last_usable_lba = read_u64_le(raw, 48)?;
    let entries_lba = read_u64_le(raw, 72)?;
    let entry_count = read_u32_le(raw, 80)?;
    let entry_size = read_u32_le(raw, 84)?;
    let entry_array_crc32 = read_u32_le(raw, 88)?;

    if current_lba != 1 {
        return Err(invalid_data("primary GPT header is not at LBA1"));
    }
    if backup_lba >= total_blocks || entries_lba >= total_blocks {
        return Err(invalid_data("GPT header references LBAs outside disk"));
    }
    if first_usable_lba > last_usable_lba {
        return Err(invalid_data("GPT usable LBA range is invalid"));
    }
    if entry_count == 0 || entry_size < GPT_MIN_ENTRY_SIZE as u32 {
        return Err(invalid_data("GPT partition entry shape is invalid"));
    }

    Ok(GptHeader {
        first_usable_lba,
        last_usable_lba,
        entries_lba,
        entry_count,
        entry_size,
        entry_array_crc32,
    })
}

#[derive(Clone, Copy, Debug)]
struct GptEntry {
    partno: u32,
    type_guid: Guid,
    first_lba: u64,
    last_lba: u64,
}

impl GptEntry {
    fn is_unused(&self) -> bool {
        self.type_guid.0.iter().all(|byte| *byte == 0)
    }
}

fn parse_gpt_entry(raw: &[u8], partno: u32) -> io::Result<GptEntry> {
    if raw.len() < GPT_MIN_ENTRY_SIZE {
        return Err(invalid_data("GPT partition entry is too short"));
    }

    let mut type_guid = [0; 16];
    type_guid.copy_from_slice(&raw[..16]);
    Ok(GptEntry {
        partno,
        type_guid: Guid(type_guid),
        first_lba: read_u64_le(raw, 32)?,
        last_lba: read_u64_le(raw, 40)?,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Guid([u8; 16]);

fn boot_role_for_guid(guid: Guid) -> Option<BootPartitionRole> {
    match guid {
        XBOOTLDR_GUID => Some(BootPartitionRole::Xbootldr),
        ESP_GUID => Some(BootPartitionRole::Esp),
        _ => None,
    }
}

fn partition_name_for_partno(disk_sysfs: &Path, partno: u32) -> io::Result<Option<String>> {
    for entry in fs::read_dir(disk_sysfs)? {
        let entry = entry?;
        let path = entry.path();
        if !path.join("partition").exists() {
            continue;
        }
        if read_trimmed(path.join("partition")).and_then(|value| value.parse::<u32>().ok())
            == Some(partno)
        {
            return Ok(Some(entry.file_name().to_string_lossy().into_owned()));
        }
    }
    Ok(None)
}

fn mount_partition(candidate: &BootPartitionCandidate) -> io::Result<MountedPartition> {
    let root = Path::new(BOOT_MOUNT_ROOT).join(format!(
        "{}-{}",
        candidate.role.label(),
        candidate.partition
    ));
    fs::create_dir_all(&root)?;

    let mut last_error = None;
    for fstype in ["vfat", "ext4"] {
        match mount_fs(&candidate.dev_path, &root, fstype) {
            Ok(()) => return Ok(MountedPartition { root, fstype }),
            Err(err) => last_error = Some(err),
        }
    }

    Err(last_error.unwrap_or_else(|| invalid_data("no filesystem types configured")))
}

fn mount_fs(source: &Path, target: &Path, fstype: &'static str) -> io::Result<()> {
    let source = cstring_path(source)?;
    let target = cstring_path(target)?;
    let fstype = CString::new(fstype).expect("static fstype has no NUL");
    let flags =
        libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | MS_NOSYMFOLLOW;
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            flags,
            std::ptr::null::<libc::c_void>(),
        )
    };
    if rc == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EBUSY) {
        Ok(())
    } else {
        Err(err)
    }
}

fn scan_bls_entries(
    partition: &BootPartitionCandidate,
    mount: &MountedPartition,
    context: &BlsContext,
) -> Vec<BootEntry> {
    let entries_dir = match context.entries_dir(&mount.root) {
        Ok(entries_dir) => entries_dir,
        Err(err) => {
            tracing::warn!(
                mount = %mount.root.display(),
                error = ?err,
                "invalid BLS entries directory"
            );
            return Vec::new();
        }
    };
    let Ok(entries) = fs::read_dir(&entries_dir) else {
        tracing::debug!(path = %entries_dir.display(), "BLS entries directory not found");
        return Vec::new();
    };

    let mut boot_entries = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("conf") {
            continue;
        }

        match parse_bls_file(partition, &mount.root, &path, context) {
            Ok(Some(boot_entry)) => boot_entries.push(boot_entry),
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(path = %path.display(), error = ?err, "failed to parse BLS entry")
            }
        }
    }
    boot_entries
}

fn parse_bls_file(
    partition: &BootPartitionCandidate,
    root: &Path,
    source: &Path,
    context: &BlsContext,
) -> io::Result<Option<BootEntry>> {
    let text = fs::read_to_string(source).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("read BLS entry {}: {err}", source.display()),
        )
    })?;
    let bls = BlsSnippet::parse(&text);

    if !architecture_matches(bls.architecture.as_deref()) {
        tracing::debug!(
            path = %source.display(),
            architecture = bls.architecture.as_deref().unwrap_or(""),
            "skipping BLS entry for another architecture"
        );
        return Ok(None);
    }

    let Some(linux) = bls.linux.as_deref().filter(|value| !value.is_empty()) else {
        tracing::debug!(path = %source.display(), "skipping BLS entry without linux payload");
        return Ok(None);
    };
    let linux = context.expand(linux);
    let linux = resolve_boot_path(root, &linux)?;
    if !linux.is_file() {
        tracing::warn!(path = %source.display(), linux = %linux.display(), "BLS linux payload is missing");
        return Ok(None);
    }

    let mut initrds = Vec::new();
    for initrd in bls
        .initrds
        .iter()
        .flat_map(|value| split_bls_words(&context.expand(value)))
    {
        let path = resolve_boot_path(root, &initrd)?;
        if !path.is_file() {
            tracing::warn!(source = %source.display(), initrd = %path.display(), "BLS initrd payload is missing");
            return Ok(None);
        }
        initrds.push(path);
    }

    let filename = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.display().to_string());
    let preferred = context
        .preferred_entry()
        .is_some_and(|preferred| boot_entry_id_matches(&filename, preferred));
    let options = bls
        .options
        .iter()
        .map(|value| context.expand(value).trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();

    Ok(Some(BootEntry {
        id: filename,
        title: bls.title,
        version: bls.version,
        architecture: bls.architecture,
        source: source.to_path_buf(),
        role: partition.role,
        disk: partition.disk.clone(),
        partition: partition.partition.clone(),
        preferred,
        linux,
        initrds,
        options,
    }))
}

#[derive(Debug, Default)]
struct BlsSnippet {
    title: Option<String>,
    version: Option<String>,
    architecture: Option<String>,
    linux: Option<String>,
    initrds: Vec<String>,
    options: Vec<String>,
}

impl BlsSnippet {
    fn parse(text: &str) -> Self {
        let mut snippet = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let Some(split) = line.find(char::is_whitespace) else {
                continue;
            };
            let key = &line[..split];
            let value = line[split..].trim();
            if value.is_empty() {
                continue;
            }

            match key {
                "title" => snippet.title = Some(value.to_string()),
                "version" => snippet.version = Some(value.to_string()),
                "architecture" => snippet.architecture = Some(value.to_string()),
                "linux" => snippet.linux = Some(value.to_string()),
                "initrd" => snippet.initrds.push(value.to_string()),
                "options" => snippet.options.push(value.to_string()),
                "machine-id" | "machine_id" | "sort-key" | "sort_key" | "efi" | "devicetree"
                | "devicetree-overlay" | "devicetree_overlay" | "grub_class" | "grub_users"
                | "grub_hotkey" | "grub_arg" => {}
                _ => tracing::debug!(key, "ignoring unsupported BLS key"),
            }
        }
        snippet
    }
}

fn parse_grubenv_assignments(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if !is_grub_identifier(key) {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

fn parse_grub_set_assignments(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("set")?;
            if !rest.starts_with(char::is_whitespace) {
                return None;
            }
            parse_key_value_assignment(rest.trim())
        })
        .collect()
}

fn parse_key_value_assignment(line: &str) -> Option<(String, String)> {
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if !is_grub_identifier(key) {
        return None;
    }
    Some((key.to_string(), parse_grub_value(value.trim())))
}

fn parse_grub_value(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }

    let mut chars = value.chars();
    let Some(quote @ ('\'' | '"')) = chars.next() else {
        return value
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
    };

    let mut parsed = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            parsed.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            break;
        }
        parsed.push(ch);
    }
    parsed
}

fn is_grub_variable_key(key: &str) -> bool {
    GRUB_VARIABLE_KEYS.contains(&key)
}

fn is_grub_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn expand_grub_vars(value: &str, variables: &HashMap<String, String>) -> String {
    let mut expanded = String::with_capacity(value.len());
    let mut chars = value.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '$' {
            expanded.push(ch);
            continue;
        }

        let Some((var_start, next)) = chars.peek().copied() else {
            expanded.push('$');
            continue;
        };
        if next == '{' {
            chars.next();
            let name_start = var_start + next.len_utf8();
            let mut name_end = None;
            while let Some((index, ch)) = chars.peek().copied() {
                if ch == '}' {
                    name_end = Some(index);
                    chars.next();
                    break;
                }
                chars.next();
            }
            let Some(name_end) = name_end else {
                expanded.push_str(&value[var_start - 1..]);
                break;
            };
            let name = &value[name_start..name_end];
            push_grub_variable(
                &mut expanded,
                value,
                var_start - 1,
                name_end + 1,
                name,
                variables,
            );
            continue;
        }

        if !(next.is_ascii_alphabetic() || next == '_') {
            expanded.push('$');
            continue;
        }
        let name_start = var_start;
        let mut name_end = var_start + next.len_utf8();
        chars.next();
        while let Some((index, ch)) = chars.peek().copied() {
            if !(ch.is_ascii_alphanumeric() || ch == '_') {
                break;
            }
            name_end = index + ch.len_utf8();
            chars.next();
        }
        let name = &value[name_start..name_end];
        push_grub_variable(
            &mut expanded,
            value,
            name_start - 1,
            name_end,
            name,
            variables,
        );
    }
    expanded
}

fn push_grub_variable(
    output: &mut String,
    original: &str,
    token_start: usize,
    token_end: usize,
    name: &str,
    variables: &HashMap<String, String>,
) {
    if let Some(value) = variables.get(name) {
        output.push_str(value);
    } else if !GRUB_OPTIONAL_EMPTY_VARIABLES.contains(&name) {
        output.push_str(&original[token_start..token_end]);
    }
}

fn split_bls_words(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn boot_entry_id_matches(filename: &str, preferred: &str) -> bool {
    filename == preferred
        || filename
            .strip_suffix(".conf")
            .is_some_and(|stem| stem == preferred)
}

fn architecture_matches(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return true;
    };
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "aa64" | "aarch64" | "arm64"
    )
}

fn resolve_boot_path(root: &Path, value: &str) -> io::Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_data("empty BLS path"));
    }

    let mut relative = PathBuf::new();
    for component in Path::new(value.trim_start_matches('/')).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid_data(format!("unsafe BLS path {value:?}")));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(invalid_data(format!("empty BLS path {value:?}")));
    }

    Ok(root.join(relative))
}

fn compare_partition_candidates(
    left: &BootPartitionCandidate,
    right: &BootPartitionCandidate,
) -> Ordering {
    left.role
        .priority()
        .cmp(&right.role.priority())
        .then_with(|| left.removable.cmp(&right.removable))
        .then_with(|| left.disk.cmp(&right.disk))
        .then_with(|| left.partno.cmp(&right.partno))
}

fn compare_boot_entries(left: &BootEntry, right: &BootEntry) -> Ordering {
    left.role
        .priority()
        .cmp(&right.role.priority())
        .then_with(|| right.preferred.cmp(&left.preferred))
        .then_with(|| left.disk.cmp(&right.disk))
        .then_with(|| left.partition.cmp(&right.partition))
        .then_with(|| right.id.cmp(&left.id))
        .then_with(|| left.source.cmp(&right.source))
}

fn read_lba(file: &File, logical_block_size: u64, lba: u64) -> io::Result<Vec<u8>> {
    let offset = lba
        .checked_mul(logical_block_size)
        .ok_or_else(|| invalid_data("LBA offset overflows"))?;
    let mut block = vec![
        0;
        usize::try_from(logical_block_size).map_err(|_| {
            invalid_data("logical block size does not fit in memory")
        })?
    ];
    read_exact_at(file, &mut block, offset)?;
    Ok(block)
}

fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        let read = file.read_at(buffer, offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "block device ended early",
            ));
        }
        offset += read as u64;
        buffer = &mut buffer[read..];
    }
    Ok(())
}

fn read_u32_le(raw: &[u8], start: usize) -> io::Result<u32> {
    let bytes = raw
        .get(start..start + 4)
        .ok_or_else(|| invalid_data("u32 field exceeds record"))?;
    Ok(u32::from_le_bytes(
        bytes.try_into().expect("slice has length 4"),
    ))
}

fn read_u64_le(raw: &[u8], start: usize) -> io::Result<u64> {
    let bytes = raw
        .get(start..start + 8)
        .ok_or_else(|| invalid_data("u64 field exceeds record"))?;
    Ok(u64::from_le_bytes(
        bytes.try_into().expect("slice has length 8"),
    ))
}

fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320u32 & mask);
        }
    }
    !crc
}

fn is_local_disk_name(name: &str) -> bool {
    !is_excluded_disk_name(name)
        && (name.starts_with("mmcblk")
            || name.starts_with("nvme")
            || name.starts_with("vd")
            || name.starts_with("xvd")
            || is_scsi_disk_like_name(name))
        && !name.contains("boot")
        && !name.contains("rpmb")
}

fn is_excluded_disk_name(name: &str) -> bool {
    name.starts_with("loop")
        || name.starts_with("ram")
        || name.starts_with("zram")
        || name.starts_with("dm-")
        || name.starts_with("md")
        || name.starts_with("sr")
}

fn is_scsi_disk_like_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("sd") else {
        return false;
    };
    !suffix.is_empty()
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn is_virtual_block(path: &Path) -> bool {
    fs::canonicalize(path)
        .map(|path| path.starts_with(PathBuf::from("/sys/devices/virtual/block")))
        .unwrap_or(false)
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn cstring_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains NUL byte: {}", path.display()),
        )
    })
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_bls_entry() {
        let snippet = BlsSnippet::parse(
            "title Test OS\nversion 1.2.3\nlinux /vmlinuz\ninitrd /initrd.img\noptions root=UUID=abc quiet\n",
        );

        assert_eq!(snippet.title.as_deref(), Some("Test OS"));
        assert_eq!(snippet.version.as_deref(), Some("1.2.3"));
        assert_eq!(snippet.linux.as_deref(), Some("/vmlinuz"));
        assert_eq!(snippet.initrds, ["/initrd.img"]);
        assert_eq!(snippet.options, ["root=UUID=abc quiet"]);
    }

    #[test]
    fn bls_parser_ignores_unknown_keys() {
        let snippet = BlsSnippet::parse("linux /vmlinuz\nuki /EFI/Linux/test.efi\nfoo bar\n");

        assert_eq!(snippet.linux.as_deref(), Some("/vmlinuz"));
    }

    #[test]
    fn parses_mbr_xbootldr_partition() {
        let mut mbr = [0u8; 512];
        mbr[510..512].copy_from_slice(&MBR_SIGNATURE);
        let entry = MBR_PARTITION_TABLE_OFFSET + MBR_PARTITION_ENTRY_SIZE;
        mbr[entry + 4] = MBR_XBOOTLDR_TYPE;
        mbr[entry + 8..entry + 12].copy_from_slice(&2048u32.to_le_bytes());
        mbr[entry + 12..entry + 16].copy_from_slice(&4096u32.to_le_bytes());

        let entries = parse_mbr_boot_entries(&mbr, 8192).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].partno, 2);
    }

    #[test]
    fn grubenv_parser_reads_plain_assignments() {
        let values = parse_grubenv_assignments(
            "# GRUB Environment Block\nsaved_entry=fedora-1\nkernelopts=root=UUID=abc ro quiet\nmenu_auto_hide=1\n###\n",
        );

        assert!(
            values
                .iter()
                .any(|(key, value)| key == "saved_entry" && value == "fedora-1")
        );
        assert!(
            values
                .iter()
                .any(|(key, value)| key == "kernelopts" && value == "root=UUID=abc ro quiet")
        );
    }

    #[test]
    fn grub_set_parser_reads_quoted_values() {
        let values = parse_grub_set_assignments(
            "set tuned_initrd=\"\"\nif [ -z \"${kernelopts}\" ]; then\n  set kernelopts=\"root=UUID=abc ro quiet\"\nfi\n",
        );

        assert!(
            values
                .iter()
                .any(|(key, value)| key == "tuned_initrd" && value.is_empty())
        );
        assert!(
            values
                .iter()
                .any(|(key, value)| key == "kernelopts" && value == "root=UUID=abc ro quiet")
        );
    }

    #[test]
    fn expands_grub_vars_and_preserves_unknown_options() {
        let mut variables = HashMap::new();
        variables.insert("kernelopts".to_string(), "root=UUID=abc ro".to_string());

        assert_eq!(
            expand_grub_vars("$kernelopts $tuned_params $unknown", &variables),
            "root=UUID=abc ro  $unknown"
        );
    }

    #[test]
    fn splits_expanded_initrd_words() {
        assert_eq!(
            split_bls_words("/initramfs.img  /tuned.img"),
            ["/initramfs.img", "/tuned.img"]
        );
    }

    #[test]
    fn saved_entry_matches_bls_stem() {
        assert!(boot_entry_id_matches(
            "fb0ef46ea7c8465cb1cad9c0db16b643-7.1.0.conf",
            "fb0ef46ea7c8465cb1cad9c0db16b643-7.1.0"
        ));
        assert!(boot_entry_id_matches("entry.conf", "entry.conf"));
        assert!(!boot_entry_id_matches("entry.conf", "other"));
    }

    #[test]
    fn saved_entry_wins_when_next_entry_is_empty() {
        let mut context = BlsContext::default();
        context
            .variables
            .insert("next_entry".to_string(), String::new());
        context
            .variables
            .insert("saved_entry".to_string(), "fedora-1".to_string());

        assert_eq!(context.preferred_entry(), Some("fedora-1"));
    }

    #[test]
    fn resolves_partition_absolute_paths_under_root() {
        let path = resolve_boot_path(Path::new("/mnt/boot"), "/loader/entries/../bad");

        assert!(path.is_err());
        assert_eq!(
            resolve_boot_path(Path::new("/mnt/boot"), "/vmlinuz")
                .unwrap()
                .as_path(),
            Path::new("/mnt/boot/vmlinuz")
        );
    }

    #[test]
    fn recognizes_arm64_architecture_names() {
        assert!(architecture_matches(None));
        assert!(architecture_matches(Some("aa64")));
        assert!(architecture_matches(Some("AA64")));
        assert!(architecture_matches(Some("aarch64")));
        assert!(!architecture_matches(Some("x64")));
    }

    #[test]
    fn crc32_matches_gpt_known_value() {
        assert_eq!(crc32_ieee(b"123456789"), 0xcbf4_3926);
    }
}
