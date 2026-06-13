use std::{
    ffi::CString,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

mod gadget;
#[path = "kmsg-forwarder.rs"]
mod kmsg_forwarder;

type Result<T> = std::result::Result<T, String>;

const SYS_BLOCK: &str = "/sys/block";

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(err) => {
            log_line(&format!("pocketboot: fatal: {err}"));
            1
        }
    };

    std::process::exit(code);
}

fn run() -> Result<()> {
    if unsafe { libc::getpid() } != 1 {
        return Err("pocketboot must run as PID 1 (/init)".to_string());
    }

    log_line("pocketboot: starting pid1 beachhead");
    mount_core_vfs()?;
    log_line("pocketboot: mounted /proc /sys /dev /run");
    gadget::spawn();
    kmsg_forwarder::spawn();

    wait_for_block_devices(Duration::from_secs(5));
    let devices = block_devices()?;
    if devices.is_empty() {
        log_line("pocketboot: no block devices found");
    } else {
        log_line(&format!("pocketboot: block devices ({})", devices.len()));
        for device in devices {
            log_line(&format!("pocketboot:   {}", device.describe()));
            for partition in device.partitions {
                log_line(&format!("pocketboot:     {}", partition.describe()));
            }
        }
    }

    thread::sleep(Duration::from_millis(3000));
    log_line("pocketboot: exiting so the kernel can panic/reboot");
    thread::sleep(Duration::from_millis(3000));

    Ok(())
}

fn mount_core_vfs() -> Result<()> {
    for dir in ["/proc", "/sys", "/dev", "/run"] {
        fs::create_dir_all(dir).map_err(|err| format!("create {dir}: {err}"))?;
    }

    mount_fs(Some("proc"), "/proc", Some("proc"), 0, None)?;
    mount_fs(Some("sysfs"), "/sys", Some("sysfs"), 0, None)?;
    mount_fs(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        0,
        Some("mode=0755"),
    )?;
    mount_fs(Some("tmpfs"), "/run", Some("tmpfs"), 0, Some("mode=0755"))?;
    Ok(())
}

fn mount_fs(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    let source = source.map(cstring).transpose()?;
    let target_c = cstring(target)?;
    let fstype = fstype.map(cstring).transpose()?;
    let data = data.map(cstring).transpose()?;
    let data_ptr = data
        .as_ref()
        .map(|s| s.as_ptr() as *const libc::c_void)
        .unwrap_or(std::ptr::null());

    let rc = unsafe {
        libc::mount(
            source
                .as_ref()
                .map(|s| s.as_ptr())
                .unwrap_or(std::ptr::null()),
            target_c.as_ptr(),
            fstype
                .as_ref()
                .map(|s| s.as_ptr())
                .unwrap_or(std::ptr::null()),
            flags,
            data_ptr,
        )
    };

    if rc == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EBUSY) {
        return Ok(());
    }

    Err(format!(
        "mount {} on {target} as {}: {err}",
        source
            .as_ref()
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|| "none".into()),
        fstype
            .as_ref()
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|| "none".into())
    ))
}

fn cstring(value: &str) -> Result<CString> {
    CString::new(value).map_err(|_| format!("string contains NUL byte: {value:?}"))
}

fn wait_for_block_devices(timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if has_block_devices() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn has_block_devices() -> bool {
    fs::read_dir(SYS_BLOCK)
        .map(|mut entries| entries.any(|entry| entry.is_ok()))
        .unwrap_or(false)
}

#[derive(Debug)]
struct BlockDevice {
    name: String,
    path: PathBuf,
    partitions: Vec<BlockDevice>,
}

impl BlockDevice {
    fn describe(&self) -> String {
        let dev = read_trimmed(self.path.join("dev")).unwrap_or_else(|| "?:?".to_string());
        let size = read_trimmed(self.path.join("size"))
            .and_then(|value| value.parse::<u64>().ok())
            .map(format_size)
            .unwrap_or_else(|| "size=?".to_string());
        let access = match read_trimmed(self.path.join("ro")).as_deref() {
            Some("1") => "ro",
            Some("0") => "rw",
            _ => "ro=?",
        };
        let removable = match read_trimmed(self.path.join("removable")).as_deref() {
            Some("1") => " removable",
            _ => "",
        };
        let partname = uevent_value(self.path.join("uevent"), "PARTNAME")
            .map(|value| format!(" partname={value}"))
            .unwrap_or_default();

        format!(
            "{} dev={} {} {}{}{}",
            self.name, dev, size, access, removable, partname
        )
    }
}

fn block_devices() -> Result<Vec<BlockDevice>> {
    let mut devices = Vec::new();
    let entries = fs::read_dir(SYS_BLOCK).map_err(|err| format!("read {SYS_BLOCK}: {err}"))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("read {SYS_BLOCK} entry: {err}"))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        devices.push(BlockDevice {
            partitions: partitions_for(&path)?,
            name,
            path,
        });
    }

    devices.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(devices)
}

fn partitions_for(device_path: &Path) -> Result<Vec<BlockDevice>> {
    let mut partitions = Vec::new();
    let entries = fs::read_dir(device_path)
        .map_err(|err| format!("read partitions under {}: {err}", device_path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "read partition entry under {}: {err}",
                device_path.display()
            )
        })?;
        let path = entry.path();
        if !path.join("partition").exists() {
            continue;
        }
        partitions.push(BlockDevice {
            name: entry.file_name().to_string_lossy().into_owned(),
            path,
            partitions: Vec::new(),
        });
    }

    partitions.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(partitions)
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

fn format_size(sectors: u64) -> String {
    let bytes = sectors as u128 * 512;
    let mib = bytes / 1024 / 1024;
    format!("size={sectors} sectors/{mib} MiB")
}

pub(crate) fn log_line(message: &str) {
    if write_line("/dev/kmsg", message).is_ok() {
        return;
    }
    if write_line("/dev/console", message).is_ok() {
        return;
    }
    eprintln!("{message}");
}

fn write_line(path: &str, message: &str) -> io::Result<()> {
    let mut file = File::options().write(true).open(path)?;
    writeln!(file, "{message}")
}
