use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

use crate::cmdline::KernelCommandLine;

const SYS_BLOCK: &str = "/sys/block";
const DEV: &str = "/dev";
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_MIN_HEADER_SIZE: usize = 92;
const GPT_MIN_ENTRY_SIZE: usize = 128;
const GPT_TABLE_MAX_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_LOGICAL_BLOCK_SIZE: u64 = 512;
const PARTITION_NAME_OFFSET: usize = 56;
const PARTITION_NAME_BYTES: usize = 72;
const TYPE_GUID_OFFSET: usize = 0;
const TYPE_GUID_SIZE: usize = 16;
const AB_FLAG_OFFSET: usize = 48 + 6;
const AB_PARTITION_ATTR_SLOT_ACTIVE: u8 = 0x1 << 2;
const AB_PARTITION_ATTR_BOOT_SUCCESSFUL: u8 = 0x1 << 6;
const AB_PARTITION_ATTR_UNBOOTABLE: u8 = 0x1 << 7;
const AB_SLOT_ACTIVE_VAL: u8 = 0x0f;

#[derive(Clone, Debug)]
pub(crate) struct Slots {
    cmdline: KernelCommandLine,
}

impl Slots {
    pub(crate) fn new(cmdline: KernelCommandLine) -> Self {
        Self { cmdline }
    }

    pub(crate) fn slot_count(&self) -> io::Result<usize> {
        Ok(if partition_pair_exists("boot")? { 2 } else { 0 })
    }

    pub(crate) fn has_slot(&self, partition: &str) -> io::Result<bool> {
        Ok(self.slot_count()? > 0 && partition_pair_exists(partition_base(partition))?)
    }

    pub(crate) fn current_slot(&self) -> io::Result<Option<Slot>> {
        if let Some(slot) = self.cmdline_slot() {
            return Ok(Some(slot));
        }

        self.active_slot()
    }

    pub(crate) fn active_slot(&self) -> io::Result<Option<Slot>> {
        let Some(boot) = find_partition("boot_a")? else {
            return Ok(None);
        };
        if find_partition("boot_b")?.is_none() {
            return Ok(None);
        }

        let disk = GptDisk::load(&boot.disk)?;
        disk.active_slot_for_base("boot")
    }

    pub(crate) fn is_slot_successful(&self, slot: Slot) -> io::Result<Option<bool>> {
        self.boot_attribute(slot, AB_PARTITION_ATTR_BOOT_SUCCESSFUL)
    }

    pub(crate) fn is_slot_unbootable(&self, slot: Slot) -> io::Result<Option<bool>> {
        self.boot_attribute(slot, AB_PARTITION_ATTR_UNBOOTABLE)
    }

    pub(crate) fn set_active(&self, slot: Slot) -> io::Result<()> {
        set_active_slot(slot)
    }

    fn cmdline_slot(&self) -> Option<Slot> {
        self.cmdline
            .value("androidboot.slot_suffix")
            .or_else(|| self.cmdline.value("slot_suffix"))
            .and_then(Slot::parse)
    }

    fn boot_attribute(&self, slot: Slot, mask: u8) -> io::Result<Option<bool>> {
        let Some(boot) = find_partition(&format!("boot{}", slot.suffix()))? else {
            return Ok(None);
        };

        let disk = GptDisk::load(&boot.disk)?;
        let Some(attr) = disk.attribute_byte(&format!("boot{}", slot.suffix()))? else {
            return Ok(None);
        };
        Ok(Some(attr & mask != 0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Slot {
    A,
    B,
}

impl Slot {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "0" | "a" | "A" | "_a" | "_A" => Some(Self::A),
            "1" | "b" | "B" | "_b" | "_B" => Some(Self::B),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }

    pub(crate) fn suffix(self) -> &'static str {
        match self {
            Self::A => "_a",
            Self::B => "_b",
        }
    }
}

fn partition_base(partition: &str) -> &str {
    partition
        .strip_suffix("_a")
        .or_else(|| partition.strip_suffix("_b"))
        .unwrap_or(partition)
}

fn partition_pair_exists(base: &str) -> io::Result<bool> {
    Ok(find_partition(&format!("{base}_a"))?.is_some()
        && find_partition(&format!("{base}_b"))?.is_some())
}

fn set_active_slot(slot: Slot) -> io::Result<()> {
    let Some(boot_a) = find_partition("boot_a")? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "boot_a partition was not found",
        ));
    };
    let Some(boot_b) = find_partition("boot_b")? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "boot_b partition was not found",
        ));
    };
    if boot_a.disk.name != boot_b.disk.name {
        return Err(invalid_data("boot_a and boot_b are on different disks"));
    }

    let disks = local_disks()?;
    let mut saw_boot_pair = false;
    let mut updated_pairs = 0usize;

    for disk in disks {
        let mut gpt = match GptDisk::load(&disk) {
            Ok(gpt) => gpt,
            Err(err) => {
                if disk.name == boot_a.disk.name {
                    return Err(io::Error::new(
                        err.kind(),
                        format!("load GPT for boot slot disk {}: {err}", disk.name),
                    ));
                }
                tracing::debug!(disk = %disk.name, error = ?err, "disk is not usable for A/B slot metadata");
                continue;
            }
        };

        let pairs = gpt.slot_pairs()?;
        if pairs.iter().any(|base| base == "boot") {
            saw_boot_pair = true;
        }

        let mut disk_changed = false;
        for base in pairs {
            if base == "xbl" {
                continue;
            }

            match gpt.set_active_pair(&base, slot)? {
                PairUpdate::Updated => {
                    disk_changed = true;
                    updated_pairs += 1;
                    tracing::info!(disk = %disk.name, partition = base, slot = slot.name(), "updated A/B slot metadata");
                }
                PairUpdate::SkippedNoActiveMetadata => {
                    if base == "boot" {
                        return Err(invalid_data("boot_a/boot_b have no active slot metadata"));
                    }
                    tracing::debug!(disk = %disk.name, partition = base, "skipping A/B pair without active slot metadata");
                }
            }
        }

        if disk_changed {
            gpt.commit()?;
        }
    }

    if !saw_boot_pair {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "boot_a/boot_b partitions were not found in GPT",
        ));
    }
    if updated_pairs == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no A/B GPT partition metadata was updated",
        ));
    }

    Ok(())
}

#[derive(Clone, Debug)]
struct Partition {
    disk: Disk,
}

fn find_partition(partname: &str) -> io::Result<Option<Partition>> {
    let matches = list_partitions()?
        .into_iter()
        .filter(|partition| partition.partname.as_deref() == Some(partname))
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => Ok(None),
        [partition] => Ok(Some(Partition {
            disk: partition.disk.clone(),
        })),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("partition {partname:?} is ambiguous"),
        )),
    }
}

#[derive(Clone, Debug)]
struct PartitionInfo {
    partname: Option<String>,
    disk: Disk,
}

fn list_partitions() -> io::Result<Vec<PartitionInfo>> {
    let mut partitions = Vec::new();
    for disk in local_disks()? {
        for entry in fs::read_dir(&disk.sysfs_path)? {
            let entry = entry?;
            let sysfs_path = entry.path();
            if !sysfs_path.join("partition").exists() {
                continue;
            }

            partitions.push(PartitionInfo {
                partname: uevent_value(sysfs_path.join("uevent"), "PARTNAME"),
                disk: disk.clone(),
            });
        }
    }
    Ok(partitions)
}

#[derive(Clone, Debug)]
struct Disk {
    name: String,
    sysfs_path: PathBuf,
    dev_path: PathBuf,
    logical_block_size: u64,
    total_bytes: u64,
}

fn local_disks() -> io::Result<Vec<Disk>> {
    let mut disks = Vec::new();
    for entry in fs::read_dir(SYS_BLOCK)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let sysfs_path = entry.path();
        if !is_local_flash_disk(&name, &sysfs_path) {
            continue;
        }

        let dev_path = Path::new(DEV).join(&name);
        if !dev_path.exists() {
            continue;
        }

        let sectors_512 = read_trimmed(sysfs_path.join("size"))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let total_bytes = sectors_512
            .checked_mul(512)
            .ok_or_else(|| invalid_data(format!("disk {name} byte size overflows")))?;

        disks.push(Disk {
            logical_block_size: read_trimmed(sysfs_path.join("queue/logical_block_size"))
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value >= 512 && value.is_power_of_two())
                .unwrap_or(DEFAULT_LOGICAL_BLOCK_SIZE),
            total_bytes,
            name,
            sysfs_path,
            dev_path,
        });
    }

    disks.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(disks)
}

fn is_local_flash_disk(name: &str, path: &Path) -> bool {
    !is_excluded_disk_name(name)
        && !is_virtual_block(path)
        && read_trimmed(path.join("removable")).as_deref() != Some("1")
        && is_local_flash_like_name(name)
}

fn is_excluded_disk_name(name: &str) -> bool {
    name.starts_with("loop")
        || name.starts_with("ram")
        || name.starts_with("zram")
        || name.starts_with("dm-")
        || name.starts_with("md")
        || name.starts_with("sr")
}

fn is_virtual_block(path: &Path) -> bool {
    fs::canonicalize(path)
        .map(|path| path.starts_with(PathBuf::from("/sys/devices/virtual/block")))
        .unwrap_or(false)
}

fn is_local_flash_like_name(name: &str) -> bool {
    name.starts_with("mmcblk")
        || name.starts_with("nvme")
        || name.starts_with("vd")
        || name.starts_with("xvd")
        || is_scsi_disk_like_name(name)
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

#[derive(Debug)]
struct GptDisk {
    disk: Disk,
    primary_header: GptHeader,
    backup_header: GptHeader,
    primary_header_bytes: Vec<u8>,
    backup_header_bytes: Vec<u8>,
    primary_entries: Vec<u8>,
    backup_entries: Vec<u8>,
}

impl GptDisk {
    fn load(disk: &Disk) -> io::Result<Self> {
        let total_blocks = disk.total_bytes / disk.logical_block_size;
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

        let primary_header_bytes = read_lba(&file, disk.logical_block_size, 1)?;
        let primary_header = parse_gpt_header(&primary_header_bytes, total_blocks)?;
        let backup_header_bytes =
            read_lba(&file, disk.logical_block_size, primary_header.backup_lba)?;
        let backup_header = parse_gpt_header(&backup_header_bytes, total_blocks)?;
        let primary_entries = read_entry_array(&file, disk, &primary_header)?;
        let backup_entries = read_entry_array(&file, disk, &backup_header)?;

        validate_entry_array_crc(&primary_entries, &primary_header)?;
        validate_entry_array_crc(&backup_entries, &backup_header)?;

        if primary_header.entry_count != backup_header.entry_count
            || primary_header.entry_size != backup_header.entry_size
        {
            return Err(invalid_data("primary and backup GPT entry shapes differ"));
        }

        Ok(Self {
            disk: disk.clone(),
            primary_header,
            backup_header,
            primary_header_bytes,
            backup_header_bytes,
            primary_entries,
            backup_entries,
        })
    }

    fn slot_pairs(&self) -> io::Result<Vec<String>> {
        let mut a_bases = BTreeSet::new();
        let mut b_bases = BTreeSet::new();
        for name in self.partition_names()? {
            if let Some(base) = name.strip_suffix("_a") {
                a_bases.insert(base.to_string());
            } else if let Some(base) = name.strip_suffix("_b") {
                b_bases.insert(base.to_string());
            }
        }

        Ok(a_bases.intersection(&b_bases).cloned().collect())
    }

    fn active_slot_for_base(&self, base: &str) -> io::Result<Option<Slot>> {
        let a_attr = self.attribute_byte(&format!("{base}_a"))?;
        let b_attr = self.attribute_byte(&format!("{base}_b"))?;

        Ok(match (a_attr, b_attr) {
            (Some(a), _) if a & AB_PARTITION_ATTR_SLOT_ACTIVE != 0 => Some(Slot::A),
            (_, Some(b)) if b & AB_PARTITION_ATTR_SLOT_ACTIVE != 0 => Some(Slot::B),
            _ => None,
        })
    }

    fn attribute_byte(&self, name: &str) -> io::Result<Option<u8>> {
        Ok(
            find_entry(&self.primary_entries, self.primary_header.entry_size, name)?
                .map(|entry| entry[AB_FLAG_OFFSET]),
        )
    }

    fn set_active_pair(&mut self, base: &str, target: Slot) -> io::Result<PairUpdate> {
        let a_name = format!("{base}_a");
        let b_name = format!("{base}_b");
        let a_primary = find_entry(
            &self.primary_entries,
            self.primary_header.entry_size,
            &a_name,
        )?
        .ok_or_else(|| invalid_data(format!("missing GPT entry {a_name}")))?;
        let b_primary = find_entry(
            &self.primary_entries,
            self.primary_header.entry_size,
            &b_name,
        )?
        .ok_or_else(|| invalid_data(format!("missing GPT entry {b_name}")))?;

        let a_active = is_slot_active(a_primary);
        let b_active = is_slot_active(b_primary);
        if !a_active && !b_active {
            return Ok(PairUpdate::SkippedNoActiveMetadata);
        }

        let mut active_guid = [0; TYPE_GUID_SIZE];
        let mut inactive_guid = [0; TYPE_GUID_SIZE];
        if a_active {
            active_guid.copy_from_slice(&a_primary[TYPE_GUID_OFFSET..TYPE_GUID_SIZE]);
            inactive_guid.copy_from_slice(&b_primary[TYPE_GUID_OFFSET..TYPE_GUID_SIZE]);
        } else {
            active_guid.copy_from_slice(&b_primary[TYPE_GUID_OFFSET..TYPE_GUID_SIZE]);
            inactive_guid.copy_from_slice(&a_primary[TYPE_GUID_OFFSET..TYPE_GUID_SIZE]);
        }

        update_pair_table(
            &mut self.primary_entries,
            self.primary_header.entry_size,
            &a_name,
            &b_name,
            target,
            active_guid,
            inactive_guid,
        )?;
        update_pair_table(
            &mut self.backup_entries,
            self.backup_header.entry_size,
            &a_name,
            &b_name,
            target,
            active_guid,
            inactive_guid,
        )?;

        Ok(PairUpdate::Updated)
    }

    fn partition_names(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in entries(&self.primary_entries, self.primary_header.entry_size) {
            if is_unused_entry(entry) {
                continue;
            }
            if let Some(name) = partition_name(entry)? {
                names.push(name);
            }
        }
        Ok(names)
    }

    fn commit(&mut self) -> io::Result<()> {
        update_entry_array_crc(&mut self.primary_header_bytes, &self.primary_entries)?;
        update_entry_array_crc(&mut self.backup_header_bytes, &self.backup_entries)?;
        update_header_crc(&mut self.primary_header_bytes)?;
        update_header_crc(&mut self.backup_header_bytes)?;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.disk.dev_path)
            .map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!(
                        "open {} for GPT update: {err}",
                        self.disk.dev_path.display()
                    ),
                )
            })?;

        write_all_at(
            &file,
            &self.primary_entries,
            checked_lba_offset(
                self.primary_header.entries_lba,
                self.disk.logical_block_size,
            )?,
        )?;
        write_all_at(
            &file,
            &self.primary_header_bytes,
            checked_lba_offset(
                self.primary_header.current_lba,
                self.disk.logical_block_size,
            )?,
        )?;
        write_all_at(
            &file,
            &self.backup_entries,
            checked_lba_offset(self.backup_header.entries_lba, self.disk.logical_block_size)?,
        )?;
        write_all_at(
            &file,
            &self.backup_header_bytes,
            checked_lba_offset(self.backup_header.current_lba, self.disk.logical_block_size)?,
        )?;
        file.sync_all()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PairUpdate {
    Updated,
    SkippedNoActiveMetadata,
}

#[derive(Clone, Copy, Debug)]
struct GptHeader {
    current_lba: u64,
    backup_lba: u64,
    entries_lba: u64,
    entry_count: u32,
    entry_size: usize,
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
    let entry_size = read_u32_le(raw, 84)? as usize;
    let entry_array_crc32 = read_u32_le(raw, 88)?;

    if current_lba >= total_blocks || backup_lba >= total_blocks || entries_lba >= total_blocks {
        return Err(invalid_data("GPT header references LBAs outside disk"));
    }
    if first_usable_lba > last_usable_lba {
        return Err(invalid_data("GPT usable LBA range is invalid"));
    }
    if entry_count == 0 || entry_size < GPT_MIN_ENTRY_SIZE {
        return Err(invalid_data("GPT partition entry shape is invalid"));
    }

    Ok(GptHeader {
        current_lba,
        backup_lba,
        entries_lba,
        entry_count,
        entry_size,
        entry_array_crc32,
    })
}

fn validate_entry_array_crc(entries: &[u8], header: &GptHeader) -> io::Result<()> {
    let actual = crc32_ieee(entries);
    if actual != header.entry_array_crc32 {
        return Err(invalid_data("GPT partition entry array CRC mismatch"));
    }
    Ok(())
}

fn read_entry_array(file: &File, disk: &Disk, header: &GptHeader) -> io::Result<Vec<u8>> {
    let table_bytes = table_bytes(header)?;
    let offset = checked_lba_offset(header.entries_lba, disk.logical_block_size)?;
    if offset.checked_add(table_bytes).unwrap_or(u64::MAX) > disk.total_bytes {
        return Err(invalid_data("GPT partition table exceeds disk size"));
    }

    let mut entries = vec![
        0;
        usize::try_from(table_bytes).map_err(|_| {
            invalid_data("GPT partition table is too large for this platform")
        })?
    ];
    read_exact_at(file, &mut entries, offset)?;
    Ok(entries)
}

fn table_bytes(header: &GptHeader) -> io::Result<u64> {
    let bytes = u64::from(header.entry_count)
        .checked_mul(header.entry_size as u64)
        .ok_or_else(|| invalid_data("GPT partition table size overflows"))?;
    if bytes == 0 || bytes > GPT_TABLE_MAX_BYTES {
        return Err(invalid_data(format!(
            "GPT partition table size {bytes} is unsupported"
        )));
    }
    Ok(bytes)
}

fn update_pair_table(
    table: &mut [u8],
    entry_size: usize,
    a_name: &str,
    b_name: &str,
    target: Slot,
    active_guid: [u8; TYPE_GUID_SIZE],
    inactive_guid: [u8; TYPE_GUID_SIZE],
) -> io::Result<()> {
    let a_index = find_entry_index(table, entry_size, a_name)?
        .ok_or_else(|| invalid_data(format!("missing GPT entry {a_name}")))?;
    let b_index = find_entry_index(table, entry_size, b_name)?
        .ok_or_else(|| invalid_data(format!("missing GPT entry {b_name}")))?;

    update_slot_entry(
        entry_mut(table, entry_size, a_index)?,
        target == Slot::A,
        active_guid,
        inactive_guid,
    );
    update_slot_entry(
        entry_mut(table, entry_size, b_index)?,
        target == Slot::B,
        active_guid,
        inactive_guid,
    );
    Ok(())
}

fn update_slot_entry(
    entry: &mut [u8],
    active: bool,
    active_guid: [u8; TYPE_GUID_SIZE],
    inactive_guid: [u8; TYPE_GUID_SIZE],
) {
    if active {
        entry[TYPE_GUID_OFFSET..TYPE_GUID_SIZE].copy_from_slice(&active_guid);
        entry[AB_FLAG_OFFSET] = AB_SLOT_ACTIVE_VAL;
    } else {
        entry[TYPE_GUID_OFFSET..TYPE_GUID_SIZE].copy_from_slice(&inactive_guid);
        entry[AB_FLAG_OFFSET] &= !AB_PARTITION_ATTR_SLOT_ACTIVE;
    }
}

fn is_slot_active(entry: &[u8]) -> bool {
    entry[AB_FLAG_OFFSET] & AB_PARTITION_ATTR_SLOT_ACTIVE != 0
}

fn find_entry<'a>(table: &'a [u8], entry_size: usize, name: &str) -> io::Result<Option<&'a [u8]>> {
    Ok(find_entry_index(table, entry_size, name)?.map(|index| {
        let offset = index * entry_size;
        &table[offset..offset + entry_size]
    }))
}

fn find_entry_index(table: &[u8], entry_size: usize, name: &str) -> io::Result<Option<usize>> {
    for (index, entry) in entries(table, entry_size).enumerate() {
        if is_unused_entry(entry) {
            continue;
        }
        if partition_name(entry)?.as_deref() == Some(name) {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

fn entry_mut(table: &mut [u8], entry_size: usize, index: usize) -> io::Result<&mut [u8]> {
    let offset = index
        .checked_mul(entry_size)
        .ok_or_else(|| invalid_data("GPT partition entry offset overflows"))?;
    let end = offset
        .checked_add(entry_size)
        .ok_or_else(|| invalid_data("GPT partition entry end overflows"))?;
    table
        .get_mut(offset..end)
        .ok_or_else(|| invalid_data("GPT partition entry exceeds table"))
}

fn entries(table: &[u8], entry_size: usize) -> impl Iterator<Item = &[u8]> {
    table.chunks_exact(entry_size)
}

fn is_unused_entry(entry: &[u8]) -> bool {
    entry[..TYPE_GUID_SIZE].iter().all(|byte| *byte == 0)
}

fn partition_name(entry: &[u8]) -> io::Result<Option<String>> {
    let raw = entry
        .get(PARTITION_NAME_OFFSET..PARTITION_NAME_OFFSET + PARTITION_NAME_BYTES)
        .ok_or_else(|| invalid_data("GPT partition entry name exceeds record"))?;

    let mut units = Vec::new();
    for chunk in raw.chunks_exact(2) {
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }

    if units.is_empty() {
        return Ok(None);
    }
    String::from_utf16(&units)
        .map(Some)
        .map_err(|err| invalid_data(format!("GPT partition name is invalid UTF-16: {err}")))
}

fn update_entry_array_crc(header: &mut [u8], entries: &[u8]) -> io::Result<()> {
    write_u32_le(header, 88, crc32_ieee(entries))
}

fn update_header_crc(header: &mut [u8]) -> io::Result<()> {
    let header_size = read_u32_le(header, 12)? as usize;
    if !(GPT_MIN_HEADER_SIZE..=header.len()).contains(&header_size) {
        return Err(invalid_data("GPT header size is invalid"));
    }
    write_u32_le(header, 16, 0)?;
    let crc = crc32_ieee(&header[..header_size]);
    write_u32_le(header, 16, crc)
}

fn read_lba(file: &File, logical_block_size: u64, lba: u64) -> io::Result<Vec<u8>> {
    let offset = checked_lba_offset(lba, logical_block_size)?;
    let mut block = vec![
        0;
        usize::try_from(logical_block_size).map_err(|_| {
            invalid_data("logical block size does not fit in memory")
        })?
    ];
    read_exact_at(file, &mut block, offset)?;
    Ok(block)
}

fn checked_lba_offset(lba: u64, logical_block_size: u64) -> io::Result<u64> {
    lba.checked_mul(logical_block_size)
        .ok_or_else(|| invalid_data("LBA offset overflows"))
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

fn write_all_at(file: &File, mut buffer: &[u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        let written = file.write_at(buffer, offset)?;
        if written == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short GPT write"));
        }
        offset += written as u64;
        buffer = &buffer[written..];
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

fn write_u32_le(raw: &mut [u8], start: usize, value: u32) -> io::Result<()> {
    let bytes = raw
        .get_mut(start..start + 4)
        .ok_or_else(|| invalid_data("u32 field exceeds record"))?;
    bytes.copy_from_slice(&value.to_le_bytes());
    Ok(())
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

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn uevent_value(path: impl AsRef<Path>, key: &str) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    contents.lines().find_map(|line| {
        let (line_key, value) = line.split_once('=')?;
        (line_key == key).then(|| value.to_string())
    })
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slot_names() {
        assert_eq!(Slot::parse("a"), Some(Slot::A));
        assert_eq!(Slot::parse("_b"), Some(Slot::B));
        assert_eq!(Slot::parse("1"), Some(Slot::B));
        assert_eq!(Slot::parse("c"), None);
    }

    #[test]
    fn reads_cmdline_slot_suffix() {
        let slots = Slots::new(KernelCommandLine::parse(
            "foo androidboot.slot_suffix=_b bar",
        ));

        assert_eq!(slots.cmdline_slot(), Some(Slot::B));
    }

    #[test]
    fn strips_slot_suffix_for_partition_base() {
        assert_eq!(partition_base("boot"), "boot");
        assert_eq!(partition_base("boot_a"), "boot");
        assert_eq!(partition_base("vendor_boot_b"), "vendor_boot");
    }

    #[test]
    fn crc32_matches_gpt_known_value() {
        assert_eq!(crc32_ieee(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn decodes_gpt_partition_name() {
        let mut entry = vec![0; GPT_MIN_ENTRY_SIZE];
        entry[0] = 1;
        write_partition_name(&mut entry, "boot_a");

        assert_eq!(partition_name(&entry).unwrap().as_deref(), Some("boot_a"));
    }

    #[test]
    fn updates_pair_like_qbootctl() {
        let mut table = vec![0; GPT_MIN_ENTRY_SIZE * 2];
        let active_guid = [0x11; TYPE_GUID_SIZE];
        let inactive_guid = [0x22; TYPE_GUID_SIZE];
        make_entry(
            &mut table[..GPT_MIN_ENTRY_SIZE],
            "boot_a",
            active_guid,
            0x0f,
        );
        make_entry(&mut table[GPT_MIN_ENTRY_SIZE..], "boot_b", inactive_guid, 0);

        update_pair_table(
            &mut table,
            GPT_MIN_ENTRY_SIZE,
            "boot_a",
            "boot_b",
            Slot::B,
            active_guid,
            inactive_guid,
        )
        .unwrap();

        let a = find_entry(&table, GPT_MIN_ENTRY_SIZE, "boot_a")
            .unwrap()
            .unwrap();
        let b = find_entry(&table, GPT_MIN_ENTRY_SIZE, "boot_b")
            .unwrap()
            .unwrap();
        assert_eq!(&a[..TYPE_GUID_SIZE], &inactive_guid);
        assert_eq!(a[AB_FLAG_OFFSET] & AB_PARTITION_ATTR_SLOT_ACTIVE, 0);
        assert_eq!(&b[..TYPE_GUID_SIZE], &active_guid);
        assert_eq!(b[AB_FLAG_OFFSET], AB_SLOT_ACTIVE_VAL);
    }

    fn make_entry(entry: &mut [u8], name: &str, guid: [u8; TYPE_GUID_SIZE], attr: u8) {
        entry[..TYPE_GUID_SIZE].copy_from_slice(&guid);
        entry[AB_FLAG_OFFSET] = attr;
        write_partition_name(entry, name);
    }

    fn write_partition_name(entry: &mut [u8], name: &str) {
        for (index, unit) in name.encode_utf16().enumerate() {
            let offset = PARTITION_NAME_OFFSET + index * 2;
            entry[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
}
