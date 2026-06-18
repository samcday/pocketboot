use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

use crate::fastboot::{CommandContext, CommandResult};

use super::partitions;

const COMMAND_PREFIX: &str = "flash:";
const PROC_SELF_MOUNTINFO: &str = "/proc/self/mountinfo";
const COPY_CHUNK: usize = 1024 * 1024;
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
    let requested = parse_partition(command)?;
    let partition = partitions::find(requested)?;
    validate_target(requested, &partition)?;

    let size = context.staged_len()?;
    if size == 0 {
        return Err(invalid_input("staged flash payload is empty"));
    }

    let mut source = context.staged_file()?;
    if is_sparse_image(&mut source)? {
        context.info(format!(
            "flashing sparse image to {} ({})",
            partition.fastboot_name(),
            partition.dev_path.display()
        ))?;
        let stats = flash_sparse(&mut source, &partition.dev_path, partition.size_bytes, size)?;
        context.info(format!(
            "wrote {} data bytes from {} sparse chunks; expanded size {} bytes",
            stats.written_size, stats.chunks, stats.expanded_size
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

fn parse_partition(command: &str) -> io::Result<&str> {
    let partition = command
        .strip_prefix(COMMAND_PREFIX)
        .ok_or_else(|| invalid_input("invalid flash command"))?;
    if partition.is_empty() {
        return Err(invalid_input("flash partition is empty"));
    }
    Ok(partition)
}

fn validate_target(requested: &str, partition: &partitions::Partition) -> io::Result<()> {
    if !is_allowed_partition(requested, partition) {
        return Err(invalid_input(format!(
            "flashing partition {requested:?} is not allowed"
        )));
    }
    if partition.read_only {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is read-only", partition.fastboot_name()),
        ));
    }
    ensure_not_mounted(partition)
}

fn is_allowed_partition(requested: &str, partition: &partitions::Partition) -> bool {
    is_allowed_partition_name(requested)
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
    ALLOWED_PARTITION_BASES.contains(&base)
}

fn ensure_not_mounted(partition: &partitions::Partition) -> io::Result<()> {
    let mountinfo = fs::read_to_string(PROC_SELF_MOUNTINFO)?;
    for line in mountinfo.lines() {
        if mountinfo_devnum(line) == Some(partition.devnum.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!("{} is mounted", partition.fastboot_name()),
            ));
        }
    }
    Ok(())
}

fn mountinfo_devnum(line: &str) -> Option<&str> {
    line.split_whitespace().nth(2)
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
    let mut target = File::options()
        .write(true)
        .open(target)
        .map_err(|err| io::Error::new(err.kind(), format!("open {}: {err}", target.display())))?;
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

#[derive(Debug)]
struct SparseFlashStats {
    expanded_size: u64,
    written_size: u64,
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

fn flash_sparse(
    source: &mut File,
    target: &Path,
    partition_size: u64,
    staged_size: u64,
) -> io::Result<SparseFlashStats> {
    source.seek(SeekFrom::Start(0))?;
    let header = SparseHeader::read_from(source)?;
    let expanded_size = header.expanded_size()?;
    if expanded_size > partition_size {
        return Err(invalid_input(format!(
            "sparse image expands beyond target partition: {expanded_size} > {partition_size} bytes"
        )));
    }

    let mut target = File::options()
        .write(true)
        .open(target)
        .map_err(|err| io::Error::new(err.kind(), format!("open {}: {err}", target.display())))?;
    target.seek(SeekFrom::Start(0))?;

    let mut position = 0u64;
    let mut written_size = 0u64;
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
                write_fill(&mut target, fill, output_size)?;
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

    #[test]
    fn parses_flash_partition() {
        assert_eq!(parse_partition("flash:boot").unwrap(), "boot");
    }

    #[test]
    fn rejects_empty_flash_partition() {
        let err = parse_partition("flash:").unwrap_err();

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

        let stats = flash_sparse(&mut source, &target, 4 * 4096, image.len() as u64).unwrap();
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
    fn rejects_sparse_images_larger_than_partition() {
        let temp = TempDir::new();
        let target = temp.file("userdata.img");
        fs::write(&target, vec![0; 4096]).unwrap();

        let mut image = sparse_header(2, 1);
        image.extend_from_slice(&sparse_chunk_header(SPARSE_CHUNK_DONT_CARE, 2, 12));
        let mut source = payload_file(&image);

        let err = flash_sparse(&mut source, &target, 4096, image.len() as u64).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
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
            let mut path = std::env::temp_dir();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            path.push(format!(
                "pocketboot-flash-test-{}-{nonce}",
                std::process::id()
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
