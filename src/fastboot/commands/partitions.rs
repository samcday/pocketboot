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
    pub(super) dev_path: PathBuf,
    pub(super) devnum: String,
    pub(super) size_bytes: u64,
    pub(super) read_only: bool,
    /// The partition lives on an SD card (MMC card type SD/SDcombo, or a
    /// removable MMC disk). flash.rs applies its eMMC name allowlist only to
    /// partitions without this flag. Not the raw sysfs `removable` flag,
    /// which reads 0 for SD cards.
    pub(super) sd_card: bool,
    /// The disk's queue offloads write-zeroes (queue/write_zeroes_max_bytes
    /// > 0), e.g. an eMMC that TRIMs to zeroes. SD cards never do.
    pub(super) zeroout_offload: bool,
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

/// Device numbers of loop devices whose backing file is `partition`, such as
/// bootflow's read-only mappings of boot partitions nested in userdata.
pub(super) fn loop_devnums_backed_by(partition: &Partition) -> io::Result<Vec<String>> {
    Resolver::new(PathBuf::from(SYS_BLOCK), PathBuf::from(DEV)).loop_devnums_backed_by(partition)
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
            let Some(sd_card) = flash_disk_sd_card(&disk_name, &disk_path) else {
                continue;
            };

            partitions.extend(self.partitions_for_disk(&disk_path, sd_card)?);
        }

        partitions.sort_by(|left, right| {
            left.fastboot_name()
                .cmp(right.fastboot_name())
                .then_with(|| left.kernel_name.cmp(&right.kernel_name))
        });
        Ok(partitions)
    }

    fn partitions_for_disk(&self, disk_path: &Path, sd_card: bool) -> io::Result<Vec<Partition>> {
        let disk_read_only = read_trimmed(disk_path.join("ro")).as_deref() == Some("1");
        let zeroout_offload = queue_offloads_write_zeroes(disk_path);
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
                devnum: partition_devnum(&sysfs_path)?,
                size_bytes: partition_size_bytes(&sysfs_path)?,
                read_only: disk_read_only
                    || read_trimmed(sysfs_path.join("ro")).as_deref() == Some("1"),
                sd_card,
                zeroout_offload,
                kernel_name,
                dev_path,
            });
        }
        Ok(partitions)
    }

    fn loop_devnums_backed_by(&self, partition: &Partition) -> io::Result<Vec<String>> {
        let mut devnums = Vec::new();
        for entry in fs::read_dir(&self.sys_block)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with("loop") {
                continue;
            }

            let loop_path = entry.path();
            let Some(backing) = read_trimmed(loop_path.join("loop/backing_file")) else {
                continue;
            };
            if Path::new(&backing) != partition.dev_path {
                continue;
            }
            if let Some(devnum) = read_trimmed(loop_path.join("dev")) {
                devnums.push(devnum);
            }
        }
        Ok(devnums)
    }
}

fn partition_devnum(sysfs_path: &Path) -> io::Result<String> {
    read_trimmed(sysfs_path.join("dev"))
        .ok_or_else(|| invalid_data(format!("{} has no dev", sysfs_path.display())))
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

/// Classifies a `/sys/block` disk: `None` when fastboot must not address it,
/// otherwise whether it is an SD card.
///
/// The MMC block driver never sets GENHD_FL_REMOVABLE, so an SD card reports
/// `removable` 0 just like the eMMC; the card type under `device/type` is what
/// tells them apart. Other removable disks (USB sticks and card readers seen
/// as `sd*` in USB host mode) stay excluded: they are not the device's own
/// storage, and their kernel names shift with hotplug order, which is unsafe
/// for a policy that allows any partition by kernel name.
fn flash_disk_sd_card(name: &str, path: &Path) -> Option<bool> {
    if is_excluded_disk_name(name) || is_virtual_block(path) || !is_local_flash_like_name(name) {
        return None;
    }

    let removable = read_trimmed(path.join("removable")).as_deref() == Some("1");
    if name.starts_with("mmcblk") {
        return Some(removable || is_sd_card(path));
    }
    (!removable).then_some(false)
}

fn is_sd_card(disk_path: &Path) -> bool {
    matches!(
        read_trimmed(disk_path.join("device/type")).as_deref(),
        Some("SD" | "SDcombo")
    )
}

fn queue_offloads_write_zeroes(disk_path: &Path) -> bool {
    read_trimmed(disk_path.join("queue/write_zeroes_max_bytes"))
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|bytes| bytes > 0)
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
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn resolves_partition_by_partname() {
        let temp = TempTree::new();
        temp.add_partition("mmcblk0", "mmcblk0p1", Some("boot"), 2048, false);

        let partition = temp.resolver().find("boot").unwrap();

        assert_eq!(partition.kernel_name, "mmcblk0p1");
        assert_eq!(partition.size_bytes, 2048 * 512);
        assert_eq!(partition.dev_path, temp.root.join("dev/mmcblk0p1"));
    }

    #[test]
    fn resolves_partition_by_kernel_name() {
        let temp = TempTree::new();
        temp.add_partition("mmcblk0", "mmcblk0p1", Some("boot"), 2048, false);

        let partition = temp.resolver().find("mmcblk0p1").unwrap();

        assert_eq!(partition.fastboot_name(), "boot");
        assert!(!partition.read_only);
    }

    #[test]
    fn rejects_ambiguous_partition_names() {
        let temp = TempTree::new();
        temp.add_partition("mmcblk0", "mmcblk0p1", Some("boot"), 2048, false);
        temp.add_partition("mmcblk1", "mmcblk1p1", Some("boot"), 2048, false);

        let err = temp.resolver().find("boot").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn marks_sd_card_partitions() {
        let temp = TempTree::new();
        temp.add_disk("mmcblk0", false, Some("MMC"));
        temp.add_partition("mmcblk0", "mmcblk0p1", Some("boot"), 2048, false);
        temp.add_disk("mmcblk1", false, Some("SD"));
        temp.add_partition("mmcblk1", "mmcblk1p3", None, 4096, false);

        let emmc = temp.resolver().find("boot").unwrap();
        let sd = temp.resolver().find("mmcblk1p3").unwrap();

        assert!(!emmc.sd_card);
        assert!(sd.sd_card);
        assert_eq!(sd.fastboot_name(), "mmcblk1p3");
    }

    #[test]
    fn lists_removable_mmc_disks_as_sd_cards() {
        let temp = TempTree::new();
        temp.add_disk("mmcblk1", true, None);
        temp.add_partition("mmcblk1", "mmcblk1p2", Some("rootfs"), 4096, false);

        let partitions = temp.resolver().list().unwrap();

        assert_eq!(partitions.len(), 1);
        assert_eq!(partitions[0].kernel_name, "mmcblk1p2");
        assert!(partitions[0].sd_card);
    }

    #[test]
    fn excludes_removable_scsi_disks() {
        let temp = TempTree::new();
        temp.add_disk("sda", true, None);
        temp.add_partition("sda", "sda1", Some("stick"), 2048, false);
        temp.add_disk("sdb", false, None);
        temp.add_partition("sdb", "sdb1", Some("userdata"), 2048, false);

        let partitions = temp.resolver().list().unwrap();

        assert_eq!(partitions.len(), 1);
        assert_eq!(partitions[0].kernel_name, "sdb1");
        assert!(!partitions[0].sd_card);
    }

    #[test]
    fn reads_write_zeroes_offload_from_disk_queue() {
        let temp = TempTree::new();
        temp.add_disk("mmcblk0", false, Some("MMC"));
        temp.set_write_zeroes_max_bytes("mmcblk0", "8388608");
        temp.add_partition("mmcblk0", "mmcblk0p1", Some("userdata"), 2048, false);
        temp.add_disk("mmcblk1", false, Some("SD"));
        temp.set_write_zeroes_max_bytes("mmcblk1", "0");
        temp.add_partition("mmcblk1", "mmcblk1p3", None, 2048, false);
        temp.add_partition("vda", "vda1", Some("boot"), 2048, false);

        let resolver = temp.resolver();

        assert!(resolver.find("userdata").unwrap().zeroout_offload);
        assert!(!resolver.find("mmcblk1p3").unwrap().zeroout_offload);
        assert!(!resolver.find("boot").unwrap().zeroout_offload);
    }

    #[test]
    fn finds_loop_devices_backed_by_partition() {
        let temp = TempTree::new();
        temp.add_partition("mmcblk0", "mmcblk0p1", Some("userdata"), 2048, false);
        temp.add_partition("mmcblk0", "mmcblk0p2", Some("boot"), 2048, false);
        temp.add_loop("loop0", "7:0", &temp.root.join("dev/mmcblk0p1"));
        temp.add_loop("loop1", "7:1", &temp.root.join("dev/mmcblk0p2"));
        let partition = temp.resolver().find("userdata").unwrap();

        let devnums = temp.resolver().loop_devnums_backed_by(&partition).unwrap();

        assert_eq!(devnums, ["7:0"]);
        assert_eq!(temp.resolver().list().unwrap().len(), 2);
    }

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let mut root = std::env::temp_dir();
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            root.push(format!(
                "pocketboot-partitions-test-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join("sys-block")).unwrap();
            fs::create_dir(root.join("dev")).unwrap();
            Self { root }
        }

        fn resolver(&self) -> Resolver {
            Resolver::new(self.root.join("sys-block"), self.root.join("dev"))
        }

        fn add_disk(&self, disk: &str, removable: bool, card_type: Option<&str>) {
            let disk_path = self.root.join("sys-block").join(disk);
            fs::create_dir_all(&disk_path).unwrap();
            fs::write(
                disk_path.join("removable"),
                if removable { "1" } else { "0" },
            )
            .unwrap();
            fs::write(disk_path.join("ro"), "0").unwrap();
            if let Some(card_type) = card_type {
                fs::create_dir(disk_path.join("device")).unwrap();
                fs::write(disk_path.join("device/type"), format!("{card_type}\n")).unwrap();
            }
        }

        fn set_write_zeroes_max_bytes(&self, disk: &str, bytes: &str) {
            let queue = self.root.join("sys-block").join(disk).join("queue");
            fs::create_dir_all(&queue).unwrap();
            fs::write(queue.join("write_zeroes_max_bytes"), format!("{bytes}\n")).unwrap();
        }

        fn add_loop(&self, name: &str, devnum: &str, backing: &Path) {
            let loop_path = self.root.join("sys-block").join(name);
            fs::create_dir_all(loop_path.join("loop")).unwrap();
            fs::write(loop_path.join("dev"), devnum).unwrap();
            fs::write(
                loop_path.join("loop/backing_file"),
                format!("{}\n", backing.display()),
            )
            .unwrap();
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
            if !disk_path.exists() {
                self.add_disk(disk, false, None);
            }

            let partition_path = disk_path.join(partition);
            fs::create_dir(&partition_path).unwrap();
            fs::write(partition_path.join("partition"), "1").unwrap();
            fs::write(partition_path.join("dev"), "179:1").unwrap();
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
