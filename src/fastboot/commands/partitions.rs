use std::{
    fs, io,
    path::{Path, PathBuf},
};

const SYS_BLOCK: &str = "/sys/block";
const DEV: &str = "/dev";

#[derive(Clone, Debug)]
pub(super) struct Partition {
    pub(super) kernel_name: String,
    pub(super) partname: Option<String>,
    pub(super) size_bytes: u64,
}

impl Partition {
    pub(super) fn fastboot_name(&self) -> &str {
        self.partname.as_deref().unwrap_or(&self.kernel_name)
    }

    fn matches_name(&self, name: &str) -> bool {
        self.kernel_name == name || self.partname.as_deref() == Some(name)
    }
}

pub(super) fn find(name: &str) -> io::Result<Partition> {
    Resolver::new(PathBuf::from(SYS_BLOCK), PathBuf::from(DEV)).find(name)
}

pub(super) fn list() -> io::Result<Vec<Partition>> {
    Resolver::new(PathBuf::from(SYS_BLOCK), PathBuf::from(DEV)).list()
}

struct Resolver {
    sys_block: PathBuf,
    dev_root: PathBuf,
}

impl Resolver {
    fn new(sys_block: PathBuf, dev_root: PathBuf) -> Self {
        Self {
            sys_block,
            dev_root,
        }
    }

    fn find(&self, name: &str) -> io::Result<Partition> {
        let matches = self
            .list()?
            .into_iter()
            .filter(|partition| partition.matches_name(name))
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [] => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("partition {name:?} not found"),
            )),
            [partition] => Ok(partition.clone()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("partition {name:?} is ambiguous"),
            )),
        }
    }

    fn list(&self) -> io::Result<Vec<Partition>> {
        let mut partitions = Vec::new();
        for entry in fs::read_dir(&self.sys_block)? {
            let entry = entry?;
            let disk_path = entry.path();
            let disk_name = entry.file_name().to_string_lossy().into_owned();
            if !is_local_flash_disk(&disk_name, &disk_path) {
                continue;
            }

            partitions.extend(self.partitions_for_disk(&disk_path)?);
        }

        partitions.sort_by(|left, right| {
            left.fastboot_name()
                .cmp(right.fastboot_name())
                .then_with(|| left.kernel_name.cmp(&right.kernel_name))
        });
        Ok(partitions)
    }

    fn partitions_for_disk(&self, disk_path: &Path) -> io::Result<Vec<Partition>> {
        let mut partitions = Vec::new();
        for entry in fs::read_dir(disk_path)? {
            let entry = entry?;
            let sysfs_path = entry.path();
            if !sysfs_path.join("partition").exists() {
                continue;
            }

            let kernel_name = entry.file_name().to_string_lossy().into_owned();
            let dev_path = self.dev_root.join(&kernel_name);
            if !dev_path.exists() {
                continue;
            }

            partitions.push(Partition {
                partname: uevent_value(sysfs_path.join("uevent"), "PARTNAME"),
                size_bytes: partition_size_bytes(&sysfs_path)?,
                kernel_name,
            });
        }
        Ok(partitions)
    }
}

fn partition_size_bytes(sysfs_path: &Path) -> io::Result<u64> {
    let sectors = read_trimmed(sysfs_path.join("size"))
        .ok_or_else(|| invalid_data(format!("{} has no size", sysfs_path.display())))?
        .parse::<u64>()
        .map_err(|err| invalid_data(format!("{} size is invalid: {err}", sysfs_path.display())))?;
    sectors
        .checked_mul(512)
        .ok_or_else(|| invalid_data(format!("{} size overflows", sysfs_path.display())))
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
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolves_partition_by_partname() {
        let temp = TempTree::new();
        temp.add_partition("mmcblk0", "mmcblk0p1", Some("boot"), 2048, false);

        let partition = temp.resolver().find("boot").unwrap();

        assert_eq!(partition.kernel_name, "mmcblk0p1");
        assert_eq!(partition.size_bytes, 2048 * 512);
    }

    #[test]
    fn resolves_partition_by_kernel_name() {
        let temp = TempTree::new();
        temp.add_partition("mmcblk0", "mmcblk0p1", Some("boot"), 2048, false);

        let partition = temp.resolver().find("mmcblk0p1").unwrap();

        assert_eq!(partition.fastboot_name(), "boot");
    }

    #[test]
    fn rejects_ambiguous_partition_names() {
        let temp = TempTree::new();
        temp.add_partition("mmcblk0", "mmcblk0p1", Some("boot"), 2048, false);
        temp.add_partition("mmcblk1", "mmcblk1p1", Some("boot"), 2048, false);

        let err = temp.resolver().find("boot").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            let mut root = std::env::temp_dir();
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            root.push(format!(
                "pocketboot-partitions-test-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join("sys-block")).unwrap();
            fs::create_dir(root.join("dev")).unwrap();
            Self { root }
        }

        fn resolver(&self) -> Resolver {
            Resolver::new(self.root.join("sys-block"), self.root.join("dev"))
        }

        fn add_partition(
            &self,
            disk: &str,
            partition: &str,
            partname: Option<&str>,
            sectors: u64,
            read_only: bool,
        ) {
            let disk_path = self.root.join("sys-block").join(disk);
            fs::create_dir_all(&disk_path).unwrap();
            fs::write(disk_path.join("removable"), "0").unwrap();
            fs::write(disk_path.join("ro"), "0").unwrap();

            let partition_path = disk_path.join(partition);
            fs::create_dir(&partition_path).unwrap();
            fs::write(partition_path.join("partition"), "1").unwrap();
            fs::write(partition_path.join("size"), sectors.to_string()).unwrap();
            fs::write(partition_path.join("ro"), if read_only { "1" } else { "0" }).unwrap();
            if let Some(partname) = partname {
                fs::write(
                    partition_path.join("uevent"),
                    format!("PARTNAME={partname}\n"),
                )
                .unwrap();
            }
            fs::write(self.root.join("dev").join(partition), b"").unwrap();
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
