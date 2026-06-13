use std::{
    fs, io, mem,
    os::fd::RawFd,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const SYS_BLOCK: &str = "/sys/block";
const NETLINK_KOBJECT_UEVENT: libc::c_int = 15;
const KOBJECT_UEVENT_GROUP: u32 = 1;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const QUIET_AFTER_PRESENT: Duration = Duration::from_millis(350);

#[derive(Debug)]
pub(crate) struct Report {
    pub(crate) elapsed: Duration,
    pub(crate) timed_out: bool,
    pub(crate) disks: usize,
    pub(crate) partitions: usize,
    pub(crate) events: u64,
    pub(crate) snapshot_changes: u64,
    pub(crate) summary: String,
}

pub(crate) fn wait_for_local_flash(max_wait: Duration) -> Report {
    let start = Instant::now();
    let deadline = start + max_wait;
    let mut socket = match UeventSocket::open() {
        Ok(socket) => Some(socket),
        Err(err) => {
            tracing::warn!(error = ?err, "kernel uevent socket unavailable; polling storage only");
            None
        }
    };

    let mut snapshot = snapshot_or_empty();
    let mut last_change = Instant::now();
    let mut events = 0;
    let mut snapshot_changes = 0;

    tracing::trace!(snapshot = %snapshot.summary(), "initial local flash snapshot");

    loop {
        let now = Instant::now();
        if let Some(uevent_socket) = &mut socket {
            match uevent_socket.drain() {
                Ok(drained) => {
                    for event in drained {
                        if event.is_relevant_block() {
                            events += 1;
                            last_change = now;
                            tracing::trace!(
                                action = event.action.as_deref().unwrap_or("?"),
                                devtype = event.devtype.as_deref().unwrap_or("?"),
                                devname = event.devname.as_deref().unwrap_or("?"),
                                devpath = event.devpath.as_deref().unwrap_or("?"),
                                "local flash uevent"
                            );
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(error = ?err, "kernel uevent socket failed; polling storage only");
                    socket = None;
                }
            }
        }

        let next_snapshot = snapshot_or_empty();
        if next_snapshot != snapshot {
            snapshot_changes += 1;
            last_change = now;
            tracing::trace!(
                before = %snapshot.summary(),
                after = %next_snapshot.summary(),
                "local flash snapshot changed"
            );
            snapshot = next_snapshot;
        }

        let quiet_for = now.saturating_duration_since(last_change);
        if !snapshot.disks.is_empty() && quiet_for >= QUIET_AFTER_PRESENT {
            return report(start, false, events, snapshot_changes, snapshot);
        }

        if now >= deadline {
            return report(start, true, events, snapshot_changes, snapshot);
        }

        let remaining = deadline.saturating_duration_since(now);
        thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

fn report(
    start: Instant,
    timed_out: bool,
    events: u64,
    snapshot_changes: u64,
    snapshot: Snapshot,
) -> Report {
    Report {
        elapsed: start.elapsed(),
        timed_out,
        disks: snapshot.disks.len(),
        partitions: snapshot.partition_count(),
        events,
        snapshot_changes,
        summary: snapshot.summary(),
    }
}

fn snapshot_or_empty() -> Snapshot {
    match Snapshot::capture() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::trace!(error = ?err, "failed to snapshot local flash");
            Snapshot::default()
        }
    }
}

struct UeventSocket {
    fd: RawFd,
}

impl UeventSocket {
    fn open() -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                NETLINK_KOBJECT_UEVENT,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let socket = Self { fd };
        let mut addr: libc::sockaddr_nl = unsafe { mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_pid = 0;
        addr.nl_groups = KOBJECT_UEVENT_GROUP;

        let rc = unsafe {
            libc::bind(
                socket.fd,
                &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
                mem::size_of_val(&addr) as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(socket)
    }

    fn drain(&mut self) -> io::Result<Vec<Uevent>> {
        let mut events = Vec::new();
        let mut buffer = [0u8; 8192];

        loop {
            let len = unsafe {
                libc::recv(
                    self.fd,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                    0,
                )
            };

            if len > 0 {
                if let Some(event) = Uevent::parse(&buffer[..len as usize]) {
                    events.push(event);
                }
                continue;
            }

            if len == 0 {
                return Ok(events);
            }

            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EAGAIN) => return Ok(events),
                Some(libc::EINTR) => continue,
                _ => return Err(err),
            }
        }
    }
}

impl Drop for UeventSocket {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[derive(Debug)]
struct Uevent {
    action: Option<String>,
    subsystem: Option<String>,
    devtype: Option<String>,
    devname: Option<String>,
    devpath: Option<String>,
}

impl Uevent {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let mut event = Self {
            action: None,
            subsystem: None,
            devtype: None,
            devname: None,
            devpath: None,
        };

        for field in bytes
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
        {
            let field = std::str::from_utf8(field).ok()?;
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };

            match key {
                "ACTION" => event.action = Some(value.to_string()),
                "SUBSYSTEM" => event.subsystem = Some(value.to_string()),
                "DEVTYPE" => event.devtype = Some(value.to_string()),
                "DEVNAME" => event.devname = Some(value.to_string()),
                "DEVPATH" => event.devpath = Some(value.to_string()),
                _ => {}
            }
        }

        Some(event)
    }

    fn is_relevant_block(&self) -> bool {
        if self.subsystem.as_deref() != Some("block") {
            return false;
        }

        if !matches!(
            self.action.as_deref(),
            Some("add" | "change" | "remove" | "bind" | "unbind")
        ) {
            return false;
        }

        if !matches!(self.devtype.as_deref(), Some("disk" | "partition")) {
            return false;
        }

        self.devname
            .as_deref()
            .map(is_local_flash_like_name)
            .unwrap_or(true)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Snapshot {
    disks: Vec<DiskSnapshot>,
}

impl Snapshot {
    fn capture() -> io::Result<Self> {
        let mut disks = Vec::new();
        let entries = fs::read_dir(SYS_BLOCK)?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_local_flash_disk(&name, &path) {
                continue;
            }

            disks.push(DiskSnapshot {
                partitions: partitions_for(&path)?,
                dev: read_trimmed(path.join("dev")).unwrap_or_default(),
                size: read_trimmed(path.join("size")).unwrap_or_default(),
                name,
            });
        }

        disks.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Self { disks })
    }

    fn partition_count(&self) -> usize {
        self.disks.iter().map(|disk| disk.partitions.len()).sum()
    }

    fn summary(&self) -> String {
        if self.disks.is_empty() {
            return "none".to_string();
        }

        self.disks
            .iter()
            .map(DiskSnapshot::summary)
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DiskSnapshot {
    name: String,
    dev: String,
    size: String,
    partitions: Vec<PartitionSnapshot>,
}

impl DiskSnapshot {
    fn summary(&self) -> String {
        let partitions = self
            .partitions
            .iter()
            .map(|partition| partition.name.as_str())
            .collect::<Vec<_>>()
            .join("|");
        format!(
            "{}:{}:{}[{}]",
            self.name,
            self.dev,
            self.size,
            if partitions.is_empty() {
                "-".to_string()
            } else {
                partitions
            }
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PartitionSnapshot {
    name: String,
    dev: String,
    start: String,
    size: String,
    partname: String,
}

fn partitions_for(device_path: &Path) -> io::Result<Vec<PartitionSnapshot>> {
    let mut partitions = Vec::new();
    let entries = fs::read_dir(device_path)?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.join("partition").exists() {
            continue;
        }

        partitions.push(PartitionSnapshot {
            name: entry.file_name().to_string_lossy().into_owned(),
            dev: read_trimmed(path.join("dev")).unwrap_or_default(),
            start: read_trimmed(path.join("start")).unwrap_or_default(),
            size: read_trimmed(path.join("size")).unwrap_or_default(),
            partname: uevent_value(path.join("uevent"), "PARTNAME").unwrap_or_default(),
        });
    }

    partitions.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(partitions)
}

fn is_local_flash_disk(name: &str, path: &Path) -> bool {
    if is_excluded_disk_name(name) || is_virtual_block(path) {
        return false;
    }

    if read_trimmed(path.join("removable")).as_deref() == Some("1") {
        return false;
    }

    is_local_flash_like_name(name)
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
