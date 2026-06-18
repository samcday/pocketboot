use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
};

use crate::fastboot::{CommandContext, CommandResult};

use super::partitions;

const COMMAND_PREFIX: &str = "flash:";
const PROC_SELF_MOUNTINFO: &str = "/proc/self/mountinfo";
const COPY_CHUNK: usize = 1024 * 1024;
const ANDROID_SPARSE_MAGIC: [u8; 4] = [0x3a, 0xff, 0x26, 0xed];
const ALLOWED_PARTITION_BASES: [&str; 6] = [
    "boot",
    "recovery",
    "vendor_boot",
    "init_boot",
    "dtbo",
    "dtb",
];

pub(super) fn handle(context: &mut CommandContext<'_>, command: &str) -> io::Result<CommandResult> {
    let requested = parse_partition(command)?;
    let partition = partitions::find(requested)?;
    validate_target(requested, &partition)?;

    let size = context.staged_len()?;
    if size == 0 {
        return Err(invalid_input("staged flash payload is empty"));
    }
    if size > partition.size_bytes {
        return Err(invalid_input(format!(
            "staged payload is larger than {}: {size} > {} bytes",
            partition.fastboot_name(),
            partition.size_bytes
        )));
    }

    let mut source = context.staged_file()?;
    reject_sparse_image(&mut source)?;

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

fn reject_sparse_image(source: &mut File) -> io::Result<()> {
    source.seek(SeekFrom::Start(0))?;
    let mut magic = [0; ANDROID_SPARSE_MAGIC.len()];
    let read = source.read(&mut magic)?;
    source.seek(SeekFrom::Start(0))?;
    if read == magic.len() && magic == ANDROID_SPARSE_MAGIC {
        return Err(invalid_input("Android sparse images are not supported yet"));
    }
    Ok(())
}

fn flash_raw(source: &mut File, target: &std::path::Path, mut remaining: u64) -> io::Result<u64> {
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
    fn allows_only_boot_like_partitions() {
        for name in [
            "boot",
            "boot_a",
            "boot_b",
            "vendor_boot",
            "init_boot",
            "dtbo",
        ] {
            assert!(is_allowed_partition_name(name), "{name} should be allowed");
        }
        for name in ["system", "userdata", "modemst1", "abl", "xbl", "bootloader"] {
            assert!(
                !is_allowed_partition_name(name),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_android_sparse_images() {
        let mut payload = kexec::create_payload_memfd("sparse-test").unwrap();
        payload.write_all(&ANDROID_SPARSE_MAGIC).unwrap();
        payload.write_all(b"payload").unwrap();

        let err = reject_sparse_image(&mut payload).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn accepts_raw_images() {
        let mut payload = kexec::create_payload_memfd("raw-test").unwrap();
        payload.write_all(b"ANDROID!").unwrap();

        reject_sparse_image(&mut payload).unwrap();
    }
}
