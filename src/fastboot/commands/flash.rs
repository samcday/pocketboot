//! `flash:` and `erase:` for local block partitions.
//!
//! Target policy:
//! - Fixed disks (eMMC, UFS, NVMe, virtio): only the partitions named in
//!   `ALLOWED_PARTITION_BASES` (optionally `_a`/`_b`), requested by that
//!   PARTNAME or by the kernel name of a partition carrying it.
//! - SD cards (`Partition::sd_card`): any partition, by kernel name
//!   (`mmcblk1p3`) or by PARTNAME. The whole disk is never a target.
//! - A name matching more than one partition, such as the same PARTNAME on
//!   the eMMC and the SD card, is refused; use the kernel name instead.
//! - Read-only partitions are refused. Once the payload checks pass,
//!   Pocketboot's own read-only boot-scan mounts under `/run/pocketboot/boot/`
//!   (including loop-mapped boot partitions nested in userdata) are unmounted;
//!   a mount anywhere else refuses the target. The target is then opened
//!   `O_EXCL`, so nothing can mount the partition directly while it is being
//!   written. A loop device over it takes no such claim.
//! - Boot entries found on an unmounted boot-scan mount are stale until the
//!   next boot scan: `continue` is refused and the UI drops them, so finish
//!   with `fastboot reboot`.
//!
//! Android sparse images are expanded on the device. Write large SD images as
//! sparse images, e.g. `img2simg root.raw root.simg && fastboot flash
//! mmcblk1p3 root.simg`; the fastboot client splits them into pieces of at
//! most `max-download-size`. Zero FILL chunks of at least `ZEROOUT_MIN_BYTES`
//! go to the kernel as BLKZEROOUT only where the device offloads zeroing
//! (queue/write_zeroes_max_bytes > 0, e.g. an eMMC that TRIMs to zeroes). On
//! SD cards the kernel would only write zero pages synchronously, so there
//! zeroes go through the page cache like any other data.

use std::{
    ffi::{CString, OsString},
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::OpenOptionsExt,
        },
    },
    path::{Path, PathBuf},
};

use crate::{
    bootflow::{self, BOOT_MOUNT_ROOT},
    fastboot::{CommandContext, CommandResult},
};

use super::partitions;

const FLASH_COMMAND_PREFIX: &str = "flash:";
const ERASE_COMMAND_PREFIX: &str = "erase:";
const PROC_SELF_MOUNTINFO: &str = "/proc/self/mountinfo";
const COPY_CHUNK: usize = 1024 * 1024;
const BLKDISCARD: libc::Ioctl = 0x1277;
const BLKZEROOUT: libc::Ioctl = 0x127f;
/// Smaller zero fills are written through the page cache even where the
/// device offloads zeroing: one synchronous TRIM per small chunk costs more
/// than writing the zeroes.
const ZEROOUT_MIN_BYTES: u64 = 1024 * 1024;
const ANDROID_SPARSE_MAGIC: [u8; 4] = [0x3a, 0xff, 0x26, 0xed];
const ANDROID_SPARSE_MAGIC_LE: u32 = 0xed26_ff3a;
const SPARSE_HEADER_SIZE: u16 = 28;
const SPARSE_CHUNK_HEADER_SIZE: u16 = 12;
const SPARSE_MAJOR_VERSION: u16 = 1;
const SPARSE_MINOR_VERSION: u16 = 0;
const SPARSE_CHUNK_RAW: u16 = 0xcac1;
const SPARSE_CHUNK_FILL: u16 = 0xcac2;
const SPARSE_CHUNK_DONT_CARE: u16 = 0xcac3;
const SPARSE_CHUNK_CRC32: u16 = 0xcac4;
const ALLOWED_PARTITION_BASES: [&str; 7] = [
    "boot",
    "recovery",
    "vendor_boot",
    "init_boot",
    "dtbo",
    "dtb",
    "userdata",
];

pub(super) fn handle(context: &mut CommandContext<'_>, command: &str) -> io::Result<CommandResult> {
    let requested = parse_flash_partition(command)?;
    let partition = writable_target(requested, "flashing")?;

    let size = context.staged_len()?;
    if size == 0 {
        return Err(invalid_input("staged flash payload is empty"));
    }

    let mut source = context.staged_file()?;
    if is_sparse_image(&mut source)? {
        read_sparse_header(&mut source, partition.size_bytes)?;
        release_boot_scan_mounts(context, &partition)?;
        context.info(format!(
            "flashing sparse image to {} ({})",
            partition.fastboot_name(),
            partition.dev_path.display()
        ))?;
        let zero_out: Option<ZeroOut> = partition.zeroout_offload.then_some(zero_out);
        let stats = flash_sparse(
            &mut source,
            &partition.dev_path,
            partition.size_bytes,
            size,
            zero_out,
        )?;
        context.info(format!(
            "wrote {} data bytes ({} zeroed in-kernel) from {} sparse chunks; expanded size {} bytes",
            stats.written_size, stats.zeroed_size, stats.chunks, stats.expanded_size
        ))?;
        context.okay(format!("flashed {}", partition.fastboot_name()))?;
        return Ok(CommandResult::continue_());
    }

    if size > partition.size_bytes {
        return Err(invalid_input(format!(
            "staged payload is larger than {}: {size} > {} bytes",
            partition.fastboot_name(),
            partition.size_bytes
        )));
    }

    release_boot_scan_mounts(context, &partition)?;
    context.info(format!(
        "flashing {size} bytes to {} ({})",
        partition.fastboot_name(),
        partition.dev_path.display()
    ))?;
    let written = flash_raw(&mut source, &partition.dev_path, size)?;
    context.info(format!("wrote {written} bytes"))?;
    context.okay(format!("flashed {}", partition.fastboot_name()))?;
    Ok(CommandResult::continue_())
}

pub(super) fn handle_erase(
    context: &mut CommandContext<'_>,
    command: &str,
) -> io::Result<CommandResult> {
    let requested = parse_erase_partition(command)?;
    let partition = writable_target(requested, "erasing")?;
    release_boot_scan_mounts(context, &partition)?;

    context.info(format!(
        "erasing {} ({})",
        partition.fastboot_name(),
        partition.dev_path.display()
    ))?;
    let stats = erase_partition(&partition.dev_path, partition.size_bytes)?;
    context.info(format!(
        "erased {} bytes using {}",
        stats.erased_size,
        stats.method.label()
    ))?;
    context.okay(format!("erased {}", partition.fastboot_name()))?;
    Ok(CommandResult::continue_())
}

fn parse_flash_partition(command: &str) -> io::Result<&str> {
    parse_partition(command, FLASH_COMMAND_PREFIX, "flash")
}

fn parse_erase_partition(command: &str) -> io::Result<&str> {
    parse_partition(command, ERASE_COMMAND_PREFIX, "erase")
}

fn parse_partition<'a>(command: &'a str, prefix: &str, label: &str) -> io::Result<&'a str> {
    let partition = command
        .strip_prefix(prefix)
        .ok_or_else(|| invalid_input(format!("invalid {label} command")))?;
    if partition.is_empty() {
        return Err(invalid_input(format!("{label} partition is empty")));
    }
    Ok(partition)
}

fn writable_target(requested: &str, action: &str) -> io::Result<partitions::Partition> {
    let partition = partitions::find(requested)?;
    validate_target(requested, &partition, action)?;
    Ok(partition)
}

fn validate_target(
    requested: &str,
    partition: &partitions::Partition,
    action: &str,
) -> io::Result<()> {
    if !is_allowed_partition(requested, partition) {
        return Err(invalid_input(format!(
            "{action} partition {requested:?} is not allowed"
        )));
    }
    if partition.read_only {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is read-only", partition.fastboot_name()),
        ));
    }
    Ok(())
}

fn is_allowed_partition(requested: &str, partition: &partitions::Partition) -> bool {
    partition.sd_card
        || is_allowed_partition_name(requested)
        || partition
            .partname
            .as_deref()
            .is_some_and(is_allowed_partition_name)
}

fn is_allowed_partition_name(name: &str) -> bool {
    let base = name
        .strip_suffix("_a")
        .or_else(|| name.strip_suffix("_b"))
        .unwrap_or(name);
    let base = base.to_lowercase();
    ALLOWED_PARTITION_BASES.contains(&base.as_str())
}

/// Unmounts Pocketboot's own boot-scan mounts of `partition`, directly or
/// through a loop device over it, right before it is written. Fails without
/// unmounting anything when some other mount holds the partition. Each
/// unmount is recorded with bootflow, which treats the entries discovered on
/// it as stale until the next boot scan.
fn release_boot_scan_mounts(
    context: &mut CommandContext<'_>,
    partition: &partitions::Partition,
) -> io::Result<()> {
    let mut devnums = vec![partition.devnum.clone()];
    devnums.extend(partitions::loop_devnums_backed_by(partition)?);

    let mountinfo = fs::read_to_string(PROC_SELF_MOUNTINFO)?;
    for mount_point in boot_scan_mounts_to_release(&mountinfo, &devnums, partition)? {
        unmount(&mount_point).map_err(|err| {
            tracing::warn!(
                partition = partition.fastboot_name(),
                mount = %mount_point.display(),
                error = ?err,
                "failed to unmount boot-scan mount before writing partition"
            );
            unmount_error(&mount_point, &err)
        })?;
        bootflow::mark_boot_mount_released(&mount_point);
        tracing::info!(
            partition = partition.fastboot_name(),
            mount = %mount_point.display(),
            "unmounted boot-scan mount before writing partition"
        );
        context.info(format!("unmounted {}", mount_point.display()))?;
    }

    let mountinfo = fs::read_to_string(PROC_SELF_MOUNTINFO)?;
    if let Some(mount) = mounts_of(&mountinfo, &devnums).first() {
        return Err(mounted_error(partition, &mount.mount_point));
    }
    Ok(())
}

/// Returns the boot-scan mount points holding `devnums`, newest first, or an
/// error naming the first mount that Pocketboot's boot scan does not own.
fn boot_scan_mounts_to_release(
    mountinfo: &str,
    devnums: &[String],
    partition: &partitions::Partition,
) -> io::Result<Vec<PathBuf>> {
    let mounts = mounts_of(mountinfo, devnums);
    if let Some(mount) = mounts
        .iter()
        .find(|mount| !is_boot_scan_mount(&mount.mount_point))
    {
        return Err(mounted_error(partition, &mount.mount_point));
    }
    Ok(mounts
        .into_iter()
        .rev()
        .map(|mount| mount.mount_point)
        .collect())
}

fn is_boot_scan_mount(mount_point: &Path) -> bool {
    mount_point.parent() == Some(Path::new(BOOT_MOUNT_ROOT))
}

/// FAIL replies are cut to 60 bytes, so the cause comes first and the mount
/// point is shortened to its directory name.
fn unmount_error(mount_point: &Path, err: &io::Error) -> io::Error {
    let name = mount_point
        .file_name()
        .unwrap_or(mount_point.as_os_str())
        .to_string_lossy();
    io::Error::new(err.kind(), format!("cannot unmount {name}: {}", err.kind()))
}

fn mounted_error(partition: &partitions::Partition, mount_point: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::ResourceBusy,
        format!(
            "{} is mounted at {}",
            partition.fastboot_name(),
            mount_point.display()
        ),
    )
}

#[derive(Debug, Eq, PartialEq)]
struct MountEntry {
    devnum: String,
    mount_point: PathBuf,
}

fn mounts_of(mountinfo: &str, devnums: &[String]) -> Vec<MountEntry> {
    mountinfo
        .lines()
        .filter_map(parse_mountinfo_line)
        .filter(|mount| devnums.contains(&mount.devnum))
        .collect()
}

/// Parses `major:minor` (field 3) and the mount point (field 5) of a
/// /proc/self/mountinfo line.
fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
    let mut fields = line.split_whitespace();
    let devnum = fields.nth(2)?;
    let mount_point = fields.nth(1)?;
    Some(MountEntry {
        devnum: devnum.to_string(),
        mount_point: unescape_mountinfo_path(mount_point),
    })
}

/// Undoes the kernel's `\ooo` octal escaping of space, tab, newline and
/// backslash in mountinfo paths.
fn unescape_mountinfo_path(field: &str) -> PathBuf {
    let bytes = field.as_bytes();
    let mut unescaped = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'\\')
            .then(|| bytes.get(index + 1..index + 4).and_then(parse_octal_byte))
            .flatten();
        match escaped {
            Some(byte) => {
                unescaped.push(byte);
                index += 4;
            }
            None => {
                unescaped.push(bytes[index]);
                index += 1;
            }
        }
    }
    PathBuf::from(OsString::from_vec(unescaped))
}

fn parse_octal_byte(digits: &[u8]) -> Option<u8> {
    digits.iter().try_fold(0u8, |value, digit| {
        if !(b'0'..=b'7').contains(digit) {
            return None;
        }
        value.checked_mul(8)?.checked_add(digit - b'0')
    })
}

fn unmount(mount_point: &Path) -> io::Result<()> {
    let path = CString::new(mount_point.as_os_str().as_bytes())
        .map_err(|_| invalid_input(format!("mount point {mount_point:?} contains NUL")))?;
    let rc = unsafe { libc::umount2(path.as_ptr(), libc::UMOUNT_NOFOLLOW) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Opens a flash or erase target for writing. On a block device `O_EXCL`
/// holds an exclusive claim until the file is closed, so a direct mount that
/// appears after the mount check fails instead of racing the write, and a
/// device that is still mounted or claimed fails here with EBUSY.
fn open_target(target: &Path) -> io::Result<File> {
    File::options()
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(target)
        .map_err(|err| open_target_error(target, err))
}

fn open_target_error(target: &Path, err: io::Error) -> io::Error {
    let message = if err.raw_os_error() == Some(libc::EBUSY) {
        format!("{} busy (mounted or claimed): {err}", target.display())
    } else {
        format!("open {}: {err}", target.display())
    };
    io::Error::new(err.kind(), message)
}

fn is_sparse_image(source: &mut File) -> io::Result<bool> {
    source.seek(SeekFrom::Start(0))?;
    let mut magic = [0; ANDROID_SPARSE_MAGIC.len()];
    let read = source.read(&mut magic)?;
    source.seek(SeekFrom::Start(0))?;
    Ok(read == magic.len() && magic == ANDROID_SPARSE_MAGIC)
}

fn flash_raw(source: &mut File, target: &Path, mut remaining: u64) -> io::Result<u64> {
    source.seek(SeekFrom::Start(0))?;
    let mut target = open_target(target)?;
    target.seek(SeekFrom::Start(0))?;

    let mut buffer = vec![0; COPY_CHUNK];
    let mut written = 0;
    while remaining > 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| invalid_input("flash payload chunk is too large for this platform"))?;
        source.read_exact(&mut buffer[..chunk_len])?;
        target.write_all(&buffer[..chunk_len])?;
        remaining -= chunk_len as u64;
        written += chunk_len as u64;
    }

    target.sync_all()?;
    Ok(written)
}

#[derive(Debug, Eq, PartialEq)]
struct EraseStats {
    erased_size: u64,
    method: EraseMethod,
}

#[derive(Debug, Eq, PartialEq)]
enum EraseMethod {
    Discard,
    ZeroFill,
}

impl EraseMethod {
    fn label(&self) -> &'static str {
        match self {
            Self::Discard => "discard",
            Self::ZeroFill => "zero-fill",
        }
    }
}

fn erase_partition(target: &Path, size: u64) -> io::Result<EraseStats> {
    let mut target = open_target(target)?;

    match discard_partition(&target, size) {
        Ok(()) => {
            target.sync_all()?;
            Ok(EraseStats {
                erased_size: size,
                method: EraseMethod::Discard,
            })
        }
        Err(err) if is_discard_unsupported(&err) => {
            target.seek(SeekFrom::Start(0))?;
            let erased_size = write_zeroes(&mut target, size)?;
            target.sync_all()?;
            Ok(EraseStats {
                erased_size,
                method: EraseMethod::ZeroFill,
            })
        }
        Err(err) => Err(err),
    }
}

fn discard_partition(target: &File, size: u64) -> io::Result<()> {
    let range = [0u64, size];
    let rc = unsafe { libc::ioctl(target.as_raw_fd(), BLKDISCARD, range.as_ptr()) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn is_discard_unsupported(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL | libc::ENOSYS)
    )
}

fn write_zeroes(target: &mut File, mut remaining: u64) -> io::Result<u64> {
    let buffer = vec![0; COPY_CHUNK];
    let mut written = 0;
    while remaining > 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| invalid_input("erase chunk is too large for this platform"))?;
        target.write_all(&buffer[..chunk_len])?;
        remaining -= chunk_len as u64;
        written += chunk_len as u64;
    }

    Ok(written)
}

#[derive(Debug)]
struct SparseFlashStats {
    expanded_size: u64,
    /// Bytes produced by RAW and FILL chunks, however they reached the target.
    written_size: u64,
    /// The part of `written_size` zeroed through the offload (BLKZEROOUT).
    zeroed_size: u64,
    chunks: u32,
}

#[derive(Debug, Eq, PartialEq)]
struct SparseHeader {
    chunk_header_size: u16,
    block_size: u32,
    total_blocks: u32,
    total_chunks: u32,
}

impl SparseHeader {
    fn read_from(source: &mut File) -> io::Result<Self> {
        let magic = read_u32_le(source)?;
        if magic != ANDROID_SPARSE_MAGIC_LE {
            return Err(invalid_data("invalid Android sparse image magic"));
        }

        let major = read_u16_le(source)?;
        let minor = read_u16_le(source)?;
        if (major, minor) != (SPARSE_MAJOR_VERSION, SPARSE_MINOR_VERSION) {
            return Err(invalid_data(format!(
                "unsupported Android sparse image version {major}.{minor}"
            )));
        }

        let file_header_size = read_u16_le(source)?;
        let chunk_header_size = read_u16_le(source)?;
        let block_size = read_u32_le(source)?;
        let total_blocks = read_u32_le(source)?;
        let total_chunks = read_u32_le(source)?;
        let _checksum = read_u32_le(source)?;

        if file_header_size < SPARSE_HEADER_SIZE {
            return Err(invalid_data(format!(
                "Android sparse file header is too small: {file_header_size}"
            )));
        }
        if chunk_header_size < SPARSE_CHUNK_HEADER_SIZE {
            return Err(invalid_data(format!(
                "Android sparse chunk header is too small: {chunk_header_size}"
            )));
        }
        if block_size == 0 || block_size % 4 != 0 {
            return Err(invalid_data(format!(
                "Android sparse block size is invalid: {block_size}"
            )));
        }

        skip_input(source, u64::from(file_header_size - SPARSE_HEADER_SIZE))?;

        Ok(Self {
            chunk_header_size,
            block_size,
            total_blocks,
            total_chunks,
        })
    }

    fn expanded_size(&self) -> io::Result<u64> {
        u64::from(self.total_blocks)
            .checked_mul(u64::from(self.block_size))
            .ok_or_else(|| invalid_data("Android sparse expanded size overflows"))
    }
}

#[derive(Debug, Eq, PartialEq)]
struct SparseChunkHeader {
    chunk_type: u16,
    chunk_blocks: u32,
    data_size: u64,
}

impl SparseChunkHeader {
    fn read_from(source: &mut File, header_size: u16) -> io::Result<Self> {
        let chunk_type = read_u16_le(source)?;
        let _reserved = read_u16_le(source)?;
        let chunk_blocks = read_u32_le(source)?;
        let total_size = read_u32_le(source)?;
        if total_size < u32::from(header_size) {
            return Err(invalid_data(format!(
                "Android sparse chunk total size {total_size} is smaller than header size {header_size}"
            )));
        }

        skip_input(source, u64::from(header_size - SPARSE_CHUNK_HEADER_SIZE))?;

        Ok(Self {
            chunk_type,
            chunk_blocks,
            data_size: u64::from(total_size - u32::from(header_size)),
        })
    }

    fn output_size(&self, block_size: u32) -> io::Result<u64> {
        u64::from(self.chunk_blocks)
            .checked_mul(u64::from(block_size))
            .ok_or_else(|| invalid_data("Android sparse chunk output size overflows"))
    }
}

/// Zeroes `len` bytes at `start` of the target without writing them through
/// the page cache. Production uses `zero_out` (BLKZEROOUT).
type ZeroOut = fn(&File, u64, u64) -> io::Result<()>;

/// Reads the sparse header from the start of `source` and checks that the
/// image fits the partition, so a bad image fails before anything is touched.
fn read_sparse_header(source: &mut File, partition_size: u64) -> io::Result<SparseHeader> {
    source.seek(SeekFrom::Start(0))?;
    let header = SparseHeader::read_from(source)?;
    let expanded_size = header.expanded_size()?;
    if expanded_size > partition_size {
        return Err(invalid_input(format!(
            "sparse image expands beyond target partition: {expanded_size} > {partition_size} bytes"
        )));
    }
    Ok(header)
}

/// Expands the sparse image in `source` onto `target`. Zero fills go to
/// `zero_out` when it is given (see `write_fill_chunk`).
fn flash_sparse(
    source: &mut File,
    target: &Path,
    partition_size: u64,
    staged_size: u64,
    zero_out: Option<ZeroOut>,
) -> io::Result<SparseFlashStats> {
    let header = read_sparse_header(source, partition_size)?;
    let expanded_size = header.expanded_size()?;

    let mut target = open_target(target)?;
    target.seek(SeekFrom::Start(0))?;

    let mut position = 0u64;
    let mut written_size = 0u64;
    let mut zeroed_size = 0u64;
    for _ in 0..header.total_chunks {
        let chunk = SparseChunkHeader::read_from(source, header.chunk_header_size)?;
        let output_size = chunk.output_size(header.block_size)?;
        let next_position = position
            .checked_add(output_size)
            .ok_or_else(|| invalid_data("Android sparse write position overflows"))?;
        if next_position > expanded_size {
            return Err(invalid_data(
                "Android sparse chunks exceed declared expanded size",
            ));
        }

        match chunk.chunk_type {
            SPARSE_CHUNK_RAW => {
                if chunk.data_size != output_size {
                    return Err(invalid_data("Android sparse raw chunk size is invalid"));
                }
                copy_exact(source, &mut target, output_size)?;
                written_size = checked_add_written(written_size, output_size)?;
            }
            SPARSE_CHUNK_FILL => {
                if chunk.data_size != 4 {
                    return Err(invalid_data("Android sparse fill chunk size is invalid"));
                }
                let fill = read_fill(source)?;
                if write_fill_chunk(&mut target, fill, position, next_position, zero_out)?
                    == FillMethod::ZeroOut
                {
                    zeroed_size = checked_add_written(zeroed_size, output_size)?;
                }
                written_size = checked_add_written(written_size, output_size)?;
            }
            SPARSE_CHUNK_DONT_CARE => {
                if chunk.data_size != 0 {
                    return Err(invalid_data(
                        "Android sparse don't-care chunk size is invalid",
                    ));
                }
                target.seek(SeekFrom::Start(next_position))?;
            }
            SPARSE_CHUNK_CRC32 => {
                if output_size != 0 || chunk.data_size != 4 {
                    return Err(invalid_data("Android sparse CRC chunk size is invalid"));
                }
                skip_input(source, 4)?;
            }
            _ => return Err(invalid_data("unknown Android sparse chunk type")),
        }

        position = next_position;
    }

    if position != expanded_size {
        return Err(invalid_data(format!(
            "Android sparse image ended at {position} bytes, expected {expanded_size}"
        )));
    }
    let consumed = source.stream_position()?;
    if consumed != staged_size {
        return Err(invalid_data(format!(
            "Android sparse image has trailing data: consumed {consumed} of {staged_size} bytes"
        )));
    }

    target.sync_all()?;
    Ok(SparseFlashStats {
        expanded_size,
        written_size,
        zeroed_size,
        chunks: header.total_chunks,
    })
}

fn checked_add_written(left: u64, right: u64) -> io::Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| invalid_data("Android sparse written byte count overflows"))
}

fn copy_exact(source: &mut File, target: &mut File, mut remaining: u64) -> io::Result<()> {
    let mut buffer = vec![0; COPY_CHUNK];
    while remaining > 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| invalid_input("sparse raw chunk is too large for this platform"))?;
        source.read_exact(&mut buffer[..chunk_len])?;
        target.write_all(&buffer[..chunk_len])?;
        remaining -= chunk_len as u64;
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum FillMethod {
    ZeroOut,
    Write,
}

/// Fills `start..end` of the target. When `zero_out` is given, a zero fill of
/// at least `ZEROOUT_MIN_BYTES` goes to it instead of through the page cache,
/// and the target is left positioned at `end`. Any failure there, such as
/// EINVAL for a range the device cannot align, falls back to writing the
/// pattern.
fn write_fill_chunk(
    target: &mut File,
    fill: [u8; 4],
    start: u64,
    end: u64,
    zero_out: Option<ZeroOut>,
) -> io::Result<FillMethod> {
    let len = end - start;
    if let Some(zero_out) = zero_out.filter(|_| fill == [0; 4] && len >= ZEROOUT_MIN_BYTES) {
        match zero_out(target, start, len) {
            Ok(()) => {
                target.seek(SeekFrom::Start(end))?;
                return Ok(FillMethod::ZeroOut);
            }
            Err(err) => {
                tracing::debug!(start, end, error = ?err, "zeroing offload failed; writing zeroes");
            }
        }
    }

    target.seek(SeekFrom::Start(start))?;
    write_fill(target, fill, len)?;
    Ok(FillMethod::Write)
}

/// BLKZEROOUT. Only used where the queue advertises write-zeroes offload;
/// elsewhere the kernel falls back to synchronous zero-page writes.
fn zero_out(target: &File, start: u64, len: u64) -> io::Result<()> {
    let range = [start, len];
    let rc = unsafe { libc::ioctl(target.as_raw_fd(), BLKZEROOUT, range.as_ptr()) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_fill(target: &mut File, fill: [u8; 4], mut remaining: u64) -> io::Result<()> {
    let mut buffer = vec![0; COPY_CHUNK];
    for value in buffer.chunks_exact_mut(fill.len()) {
        value.copy_from_slice(&fill);
    }

    while remaining > 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| invalid_input("sparse fill chunk is too large for this platform"))?;
        target.write_all(&buffer[..chunk_len])?;
        remaining -= chunk_len as u64;
    }
    Ok(())
}

fn read_fill(source: &mut File) -> io::Result<[u8; 4]> {
    let mut fill = [0; 4];
    source.read_exact(&mut fill)?;
    Ok(fill)
}

fn read_u16_le(source: &mut File) -> io::Result<u16> {
    let mut bytes = [0; 2];
    source.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32_le(source: &mut File) -> io::Result<u32> {
    let mut bytes = [0; 4];
    source.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn skip_input(source: &mut File, bytes: u64) -> io::Result<()> {
    let bytes = i64::try_from(bytes).map_err(|_| invalid_input("skip length is too large"))?;
    source.seek(SeekFrom::Current(bytes))?;
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kexec;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn parses_flash_partition() {
        assert_eq!(parse_flash_partition("flash:boot").unwrap(), "boot");
    }

    #[test]
    fn parses_erase_partition() {
        assert_eq!(parse_erase_partition("erase:userdata").unwrap(), "userdata");
    }

    #[test]
    fn rejects_empty_flash_partition() {
        let err = parse_flash_partition("flash:").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_empty_erase_partition() {
        let err = parse_erase_partition("erase:").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn allows_only_safe_partitions() {
        for name in [
            "boot",
            "boot_a",
            "boot_b",
            "vendor_boot",
            "init_boot",
            "dtbo",
            "userdata",
        ] {
            assert!(is_allowed_partition_name(name), "{name} should be allowed");
        }
        for name in ["system", "modemst1", "abl", "xbl", "bootloader"] {
            assert!(
                !is_allowed_partition_name(name),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn allows_any_partition_on_sd_cards() {
        let unnamed = test_partition("mmcblk1p3", None, true);
        let named = test_partition("mmcblk1p2", Some("system"), true);

        assert!(is_allowed_partition("mmcblk1p3", &unnamed));
        assert!(is_allowed_partition("mmcblk1p2", &named));
        assert!(is_allowed_partition("system", &named));
    }

    #[test]
    fn keeps_allowlist_for_fixed_disks() {
        let boot = test_partition("mmcblk0p9", Some("boot"), false);
        let abl = test_partition("mmcblk0p5", Some("abl"), false);
        let unnamed = test_partition("mmcblk0p30", None, false);

        assert!(is_allowed_partition("boot", &boot));
        assert!(is_allowed_partition("mmcblk0p9", &boot));
        assert!(!is_allowed_partition("abl", &abl));
        assert!(!is_allowed_partition("mmcblk0p5", &abl));
        assert!(!is_allowed_partition("mmcblk0p30", &unnamed));
    }

    #[test]
    fn rejects_read_only_sd_card_partitions() {
        let mut partition = test_partition("mmcblk1p3", None, true);
        partition.read_only = true;

        let err = validate_target("mmcblk1p3", &partition, "flashing").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn parses_mountinfo_devnum_and_escaped_mount_point() {
        let line = r"37 25 179:3 / /mnt/with\040space\134x rw,relatime - ext4 /dev/mmcblk1p3 rw";

        assert_eq!(
            parse_mountinfo_line(line),
            Some(MountEntry {
                devnum: "179:3".to_string(),
                mount_point: PathBuf::from(r"/mnt/with space\x"),
            })
        );
    }

    #[test]
    fn releases_boot_scan_mounts_newest_first() {
        let partition = test_partition("mmcblk1p2", None, true);
        let devnums = ["179:2".to_string(), "7:0".to_string()];

        let mount_points =
            boot_scan_mounts_to_release(TEST_MOUNTINFO, &devnums, &partition).unwrap();

        assert_eq!(
            mount_points,
            [
                PathBuf::from("/run/pocketboot/boot/nested-mmcblk1p2p1"),
                PathBuf::from("/run/pocketboot/boot/xbootldr-mmcblk1p2"),
            ]
        );
    }

    #[test]
    fn refuses_partitions_mounted_outside_boot_scan() {
        let partition = test_partition("mmcblk1p3", None, true);
        let devnums = ["179:3".to_string()];

        let err = boot_scan_mounts_to_release(TEST_MOUNTINFO, &devnums, &partition).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::ResourceBusy);
        assert!(err.to_string().contains("/sysroot"), "{err}");
    }

    #[test]
    fn ignores_unmounted_partitions() {
        let partition = test_partition("mmcblk1p1", None, true);
        let devnums = ["179:1".to_string()];

        let mount_points =
            boot_scan_mounts_to_release(TEST_MOUNTINFO, &devnums, &partition).unwrap();

        assert!(mount_points.is_empty());
    }

    #[test]
    fn keeps_failure_causes_within_fastboot_reply_limit() {
        let busy = io::Error::from_raw_os_error(libc::EBUSY);

        let unmount = unmount_error(Path::new("/run/pocketboot/boot/xbootldr-mmcblk1p2"), &busy);
        let open = open_target_error(Path::new("/dev/mmcblk1p3"), busy);

        assert_eq!(
            unmount.to_string(),
            "cannot unmount xbootldr-mmcblk1p2: resource busy"
        );
        assert_eq!(unmount.kind(), io::ErrorKind::ResourceBusy);
        let open = open.to_string();
        let reply = &open.as_bytes()[..open.len().min(60)];
        assert!(
            reply.starts_with(b"/dev/mmcblk1p3 busy (mounted or claimed): "),
            "{open}"
        );
    }

    #[test]
    fn recognizes_only_direct_boot_scan_mount_points() {
        assert!(is_boot_scan_mount(Path::new(
            "/run/pocketboot/boot/xbootldr-mmcblk1p2"
        )));
        assert!(!is_boot_scan_mount(Path::new("/run/pocketboot/boot")));
        assert!(!is_boot_scan_mount(Path::new("/run/pocketboot/boot/a/b")));
        assert!(!is_boot_scan_mount(Path::new("/run/pocketboot/bootx/a")));
    }

    #[test]
    fn detects_android_sparse_images() {
        let mut payload = kexec::create_payload_memfd("sparse-test").unwrap();
        payload.write_all(&ANDROID_SPARSE_MAGIC).unwrap();
        payload.write_all(b"payload").unwrap();

        assert!(is_sparse_image(&mut payload).unwrap());
    }

    #[test]
    fn does_not_detect_raw_images_as_sparse() {
        let mut payload = kexec::create_payload_memfd("raw-test").unwrap();
        payload.write_all(b"ANDROID!").unwrap();

        assert!(!is_sparse_image(&mut payload).unwrap());
    }

    #[test]
    fn parses_sparse_header() {
        let mut payload = payload_file(&sparse_header(4, 3));

        let header = SparseHeader::read_from(&mut payload).unwrap();

        assert_eq!(
            header,
            SparseHeader {
                chunk_header_size: SPARSE_CHUNK_HEADER_SIZE,
                block_size: 4096,
                total_blocks: 4,
                total_chunks: 3,
            }
        );
        assert_eq!(header.expanded_size().unwrap(), 16 * 1024);
    }

    #[test]
    fn flashes_sparse_image_and_preserves_dont_care_ranges() {
        let temp = TempDir::new();
        let target = temp.file("userdata.img");
        fs::write(&target, vec![0x5a; 4 * 4096]).unwrap();

        let raw_block = vec![0x11; 4096];
        let fill = [0x22, 0x33, 0x44, 0x55];
        let mut image = sparse_header(4, 3);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_RAW, 1, 12 + 4096));
        image.extend_from_slice(&raw_block);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_DONT_CARE, 1, 12));
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_FILL, 2, 16));
        image.extend_from_slice(&fill);
        let mut source = payload_file(&image);

        let stats = flash_sparse(&mut source, &target, 4 * 4096, image.len() as u64, None).unwrap();
        let flashed = fs::read(&target).unwrap();

        assert_eq!(stats.expanded_size, 4 * 4096);
        assert_eq!(stats.written_size, 3 * 4096);
        assert_eq!(&flashed[..4096], raw_block.as_slice());
        assert!(flashed[4096..8192].iter().all(|byte| *byte == 0x5a));
        assert!(
            flashed[8192..]
                .chunks_exact(fill.len())
                .all(|chunk| chunk == fill.as_slice())
        );
    }

    #[test]
    fn flashes_split_sparse_images_with_leading_dont_care_offsets() {
        let temp = TempDir::new();
        let target = temp.file("userdata.img");
        fs::write(&target, vec![0x5a; 4 * 4096]).unwrap();

        let first_block = vec![0x11; 4096];
        let mut first_image = sparse_header(4, 2);
        first_image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_RAW, 1, 12 + 4096));
        first_image.extend_from_slice(&first_block);
        first_image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_DONT_CARE, 3, 12));
        let mut first_source = payload_file(&first_image);

        flash_sparse(
            &mut first_source,
            &target,
            4 * 4096,
            first_image.len() as u64,
            None,
        )
        .unwrap();

        let second_block = vec![0x22; 4096];
        let mut second_image = sparse_header(4, 3);
        second_image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_DONT_CARE, 1, 12));
        second_image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_RAW, 1, 12 + 4096));
        second_image.extend_from_slice(&second_block);
        second_image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_DONT_CARE, 2, 12));
        let mut second_source = payload_file(&second_image);

        flash_sparse(
            &mut second_source,
            &target,
            4 * 4096,
            second_image.len() as u64,
            None,
        )
        .unwrap();

        let flashed = fs::read(&target).unwrap();
        assert_eq!(&flashed[..4096], first_block.as_slice());
        assert_eq!(&flashed[4096..8192], second_block.as_slice());
        assert!(flashed[8192..].iter().all(|byte| *byte == 0x5a));
    }

    #[test]
    fn writes_zero_fill_chunks_without_offload() {
        let temp = TempDir::new();
        let target = temp.file("rootfs.img");
        fs::write(&target, vec![0x5a; 4 * 4096]).unwrap();

        let raw_block = vec![0x11; 4096];
        let mut image = sparse_header(4, 3);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_FILL, 2, 16));
        image.extend_from_slice(&[0; 4]);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_RAW, 1, 12 + 4096));
        image.extend_from_slice(&raw_block);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_FILL, 1, 16));
        image.extend_from_slice(&[0; 4]);
        let mut source = payload_file(&image);

        let stats = flash_sparse(&mut source, &target, 4 * 4096, image.len() as u64, None).unwrap();
        let flashed = fs::read(&target).unwrap();

        assert_eq!(stats.written_size, 4 * 4096);
        assert_eq!(stats.zeroed_size, 0);
        assert!(flashed[..8192].iter().all(|byte| *byte == 0));
        assert_eq!(&flashed[8192..12288], raw_block.as_slice());
        assert!(flashed[12288..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn zeroes_large_fill_chunks_through_offload() {
        let temp = TempDir::new();
        let target = temp.file("userdata.img");
        let large_blocks = (ZEROOUT_MIN_BYTES / 4096) as u32;
        let total_blocks = large_blocks + 2;
        let large_len = ZEROOUT_MIN_BYTES as usize;
        fs::write(&target, vec![0x5a; total_blocks as usize * 4096]).unwrap();

        let raw_block = vec![0x11; 4096];
        let mut image = sparse_header(total_blocks, 3);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_FILL, large_blocks, 16));
        image.extend_from_slice(&[0; 4]);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_RAW, 1, 12 + 4096));
        image.extend_from_slice(&raw_block);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_FILL, 1, 16));
        image.extend_from_slice(&[0; 4]);
        let mut source = payload_file(&image);

        let stats = flash_sparse(
            &mut source,
            &target,
            u64::from(total_blocks) * 4096,
            image.len() as u64,
            Some(zero_with_pwrite),
        )
        .unwrap();
        let flashed = fs::read(&target).unwrap();

        assert_eq!(
            stats.zeroed_size, ZEROOUT_MIN_BYTES,
            "only the large fill is offloaded"
        );
        assert_eq!(stats.written_size, u64::from(total_blocks) * 4096);
        assert!(flashed[..large_len].iter().all(|byte| *byte == 0));
        assert_eq!(
            &flashed[large_len..large_len + 4096],
            raw_block.as_slice(),
            "RAW data after an offloaded fill lands at the fill's end"
        );
        assert!(flashed[large_len + 4096..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn writes_zeroes_when_offload_fails() {
        let temp = TempDir::new();
        let target = temp.file("userdata.img");
        let blocks = (ZEROOUT_MIN_BYTES / 4096) as u32 + 1;
        let fill_len = ZEROOUT_MIN_BYTES as usize;
        fs::write(&target, vec![0x5a; blocks as usize * 4096]).unwrap();

        let raw_block = vec![0x11; 4096];
        let mut image = sparse_header(blocks, 2);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_FILL, blocks - 1, 16));
        image.extend_from_slice(&[0; 4]);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_RAW, 1, 12 + 4096));
        image.extend_from_slice(&raw_block);
        let mut source = payload_file(&image);

        // The real BLKZEROOUT fails with ENOTTY on a regular file.
        let stats = flash_sparse(
            &mut source,
            &target,
            u64::from(blocks) * 4096,
            image.len() as u64,
            Some(zero_out),
        )
        .unwrap();
        let flashed = fs::read(&target).unwrap();

        assert_eq!(stats.zeroed_size, 0);
        assert!(flashed[..fill_len].iter().all(|byte| *byte == 0));
        assert_eq!(&flashed[fill_len..], raw_block.as_slice());
    }

    #[test]
    fn zero_out_fails_on_regular_files() {
        let temp = TempDir::new();
        let target = temp.file("rootfs.img");
        fs::write(&target, vec![0x5a; 4096]).unwrap();
        let file = open_target(&target).unwrap();

        let err = zero_out(&file, 0, 4096).unwrap_err();

        assert_eq!(err.raw_os_error(), Some(libc::ENOTTY));
    }

    #[test]
    fn rejects_sparse_images_larger_than_partition() {
        let temp = TempDir::new();
        let target = temp.file("userdata.img");
        fs::write(&target, vec![0; 4096]).unwrap();

        let mut image = sparse_header(2, 1);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_DONT_CARE, 2, 12));
        let mut source = payload_file(&image);

        let precheck = read_sparse_header(&mut source, 4096).unwrap_err();
        let err = flash_sparse(&mut source, &target, 4096, image.len() as u64, None).unwrap_err();

        assert_eq!(precheck.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            read_sparse_header(&mut source, 2 * 4096)
                .unwrap()
                .total_blocks,
            2
        );
    }

    #[test]
    fn erases_partition_with_zero_fill_fallback() {
        let temp = TempDir::new();
        let target = temp.file("userdata.img");
        fs::write(&target, vec![0x5a; 128]).unwrap();

        let stats = erase_partition(&target, 128).unwrap();
        let erased = fs::read(&target).unwrap();

        assert_eq!(
            stats,
            EraseStats {
                erased_size: 128,
                method: EraseMethod::ZeroFill,
            }
        );
        assert!(erased.iter().all(|byte| *byte == 0));
    }

    const TEST_MOUNTINFO: &str = "\
22 1 0:20 / /run rw,nosuid,nodev - tmpfs tmpfs rw
36 22 179:2 / /run/pocketboot/boot/xbootldr-mmcblk1p2 ro,nosuid,nodev,noexec - ext4 /dev/mmcblk1p2 ro,noload
37 22 7:0 / /run/pocketboot/boot/nested-mmcblk1p2p1 ro,nosuid,nodev,noexec - ext4 /dev/loop0 ro,noload
38 1 179:3 / /sysroot rw,relatime - ext4 /dev/mmcblk1p3 rw
";

    fn test_partition(
        kernel_name: &str,
        partname: Option<&str>,
        sd_card: bool,
    ) -> partitions::Partition {
        partitions::Partition {
            kernel_name: kernel_name.to_string(),
            partname: partname.map(str::to_string),
            dev_path: Path::new("/dev").join(kernel_name),
            devnum: "179:0".to_string(),
            size_bytes: 4096,
            read_only: false,
            sd_card,
            zeroout_offload: false,
        }
    }

    /// Stands in for a BLKZEROOUT that succeeds on a block device.
    fn zero_with_pwrite(target: &File, start: u64, len: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        target.write_all_at(&vec![0; len as usize], start)
    }

    fn payload_file(data: &[u8]) -> File {
        let mut file = kexec::create_payload_memfd("flash-test").unwrap();
        file.write_all(data).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    fn sparse_header(blocks: u32, chunks: u32) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(&ANDROID_SPARSE_MAGIC_LE.to_le_bytes());
        header.extend_from_slice(&SPARSE_MAJOR_VERSION.to_le_bytes());
        header.extend_from_slice(&SPARSE_MINOR_VERSION.to_le_bytes());
        header.extend_from_slice(&SPARSE_HEADER_SIZE.to_le_bytes());
        header.extend_from_slice(&SPARSE_CHUNK_HEADER_SIZE.to_le_bytes());
        header.extend_from_slice(&4096u32.to_le_bytes());
        header.extend_from_slice(&blocks.to_le_bytes());
        header.extend_from_slice(&chunks.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header
    }

    fn sparse_chunk_header(chunk_type: u16, blocks: u32, total_size: u32) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(&chunk_type.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&blocks.to_le_bytes());
        header.extend_from_slice(&total_size.to_le_bytes());
        header
    }

    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let mut path = std::env::temp_dir();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            path.push(format!(
                "pocketboot-flash-test-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn file(&self, name: &str) -> std::path::PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
