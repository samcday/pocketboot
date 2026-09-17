use std::{ffi::CString, fmt, fs, io, path::Path, thread, time::Duration};

const MDSS_DRIVERS: [&str; 2] = ["msm-mdss", "msm_mdss"];
const GUARD_ATTEMPTS: u32 = 20;
const GUARD_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
pub(crate) struct QuiesceReport {
    pub(crate) unbound: Vec<String>,
    pub(crate) deferred_devices: Vec<String>,
    pub(crate) probe_in_flight: Vec<String>,
    pub(crate) guard_passed: bool,
}

impl fmt::Display for QuiesceReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unbound=[{}] guard_passed={} deferred=[{}] probe_in_flight=[{}]",
            self.unbound.join(","),
            self.guard_passed,
            self.deferred_devices.join(" "),
            self.probe_in_flight.join(" ")
        )
    }
}

pub(crate) fn quiesce_display(sys: &Path, proc_root: &Path) -> io::Result<QuiesceReport> {
    Ok(quiesce_with(sys, proc_root, GUARD_ATTEMPTS, GUARD_INTERVAL))
}

fn quiesce_with(sys: &Path, proc_root: &Path, attempts: u32, interval: Duration) -> QuiesceReport {
    let mut report = QuiesceReport::default();

    unbind_display_masters(sys, &mut report);

    ensure_debugfs(sys, proc_root);

    let (guard_passed, deferred_devices, probe_in_flight) =
        wait_for_guard(sys, proc_root, attempts, interval);
    report.guard_passed = guard_passed;
    report.deferred_devices = deferred_devices;
    report.probe_in_flight = probe_in_flight;

    report
}

// Only the display master is unbound. The adv7511 i2c bridge is deliberately
// left bound: unbinding it runs adv7511_remove() first, which unregisters the
// bridge's CEC/ancillary devices, before the devres group releases
// devm_mipi_dsi_detach -> dsi_host_detach -> component_del -> msm_drm_unbind ->
// msm_drm_uninit -> drm_atomic_helper_shutdown -> adv7511_bridge_atomic_disable
// -> adv7533_dsi_power_off -> regmap_write on the already-freed CEC regmap,
// which is a NULL deref (lane 40 hop 1: adv7511_cec_register_volatile). Unbinding
// the msm-mdss platform master instead runs of_platform_depopulate/component_del
// and the same atomic shutdown while the bridge is still fully alive.
fn unbind_display_masters(sys: &Path, report: &mut QuiesceReport) {
    let devices_dir = sys.join("bus/platform/devices");
    let entries = match fs::read_dir(&devices_dir) {
        Ok(entries) => entries,
        Err(err) => {
            if err.kind() != io::ErrorKind::NotFound {
                tracing::warn!(
                    dir = %devices_dir.display(),
                    error = %err,
                    "display quiesce could not read platform devices"
                );
            }
            return;
        }
    };

    let mut targets = Vec::new();
    for entry in entries.flatten() {
        let Some(device) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !is_plain_name(&device) {
            continue;
        }
        let driver = fs::read_link(entry.path().join("driver"))
            .ok()
            .and_then(|target| target.file_name()?.to_str().map(str::to_string));
        if let Some(driver) = driver
            && MDSS_DRIVERS.contains(&driver.as_str())
        {
            targets.push((device, driver));
        }
    }
    targets.sort();

    for (device, driver) in targets {
        let driver_dir = sys.join("bus/platform/drivers").join(&driver);
        if !driver_dir.is_dir() {
            tracing::warn!(
                driver = %driver,
                dir = %driver_dir.display(),
                "display quiesce found no driver directory to unbind"
            );
            continue;
        }
        unbind(&driver_dir, &device, report);
    }
}

fn unbind(driver_dir: &Path, device: &str, report: &mut QuiesceReport) {
    if !is_plain_name(device) {
        tracing::warn!(device, "display quiesce refused unsafe device name");
        return;
    }

    let path = driver_dir.join("unbind");
    match fs::write(&path, device.as_bytes()) {
        Ok(()) => {
            tracing::info!(
                device,
                driver = %driver_dir.display(),
                "display quiesce unbound device"
            );
            report.unbound.push(device.to_string());
        }
        Err(err) => {
            tracing::warn!(
                device,
                path = %path.display(),
                error = %err,
                "display quiesce unbind failed"
            );
        }
    }
}

fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

fn ensure_debugfs(sys: &Path, proc_root: &Path) {
    if sys != Path::new("/sys") {
        return;
    }

    let target = sys.join("kernel/debug");
    if debugfs_mounted(proc_root, &target) {
        return;
    }

    let (Some(source), Some(fstype), Some(target_c)) = (
        cstring("none"),
        cstring("debugfs"),
        cstring(&target.to_string_lossy()),
    ) else {
        return;
    };

    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target_c.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if rc == 0 {
        return;
    }

    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EBUSY) | Some(libc::EPERM) => {
            tracing::debug!(error = %err, "display quiesce debugfs mount ignored");
        }
        _ => tracing::warn!(error = %err, "display quiesce debugfs mount failed"),
    }
}

fn debugfs_mounted(proc_root: &Path, target: &Path) -> bool {
    let Ok(mounts) = fs::read_to_string(proc_root.join("mounts")) else {
        return false;
    };
    let target = target.to_string_lossy();
    mounts.lines().any(|line| {
        line.split_whitespace()
            .nth(1)
            .is_some_and(|mount_point| mount_point == target)
    })
}

fn cstring(value: &str) -> Option<CString> {
    CString::new(value).ok()
}

fn wait_for_guard(
    sys: &Path,
    proc_root: &Path,
    attempts: u32,
    interval: Duration,
) -> (bool, Vec<String>, Vec<String>) {
    let attempts = attempts.max(1);
    let mut deferred = Vec::new();
    let mut in_flight = Vec::new();

    for attempt in 0..attempts {
        deferred = read_deferred(sys);
        in_flight = probe_in_flight(proc_root);
        if deferred.is_empty() && in_flight.is_empty() {
            return (true, deferred, in_flight);
        }
        if attempt + 1 < attempts && !interval.is_zero() {
            thread::sleep(interval);
        }
    }

    (false, deferred, in_flight)
}

fn read_deferred(sys: &Path) -> Vec<String> {
    let path = sys.join("kernel/debug/devices_deferred");
    match fs::read_to_string(&path) {
        Ok(contents) => contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        Err(err) => {
            if err.kind() != io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "display quiesce could not read devices_deferred"
                );
            }
            Vec::new()
        }
    }
}

fn probe_in_flight(proc_root: &Path) -> Vec<String> {
    let entries = match fs::read_dir(proc_root) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(
                dir = %proc_root.display(),
                error = %err,
                "display quiesce could not read procfs"
            );
            return Vec::new();
        }
    };

    let mut in_flight = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else {
            continue;
        };
        if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(wchan) = fs::read_to_string(entry.path().join("wchan")) else {
            continue;
        };
        let wchan = wchan.trim();
        if !wchan.contains("probe") && !wchan.contains("i2c") && !wchan.contains("qup") {
            continue;
        }
        let comm = fs::read_to_string(entry.path().join("comm"))
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        in_flight.push(format!("{pid}:{comm}:{wchan}"));
    }
    in_flight.sort();
    in_flight
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> PathBuf {
        let unique = format!(
            "pocketboot-quiesce-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_file(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn empty_proc_root(root: &Path) -> PathBuf {
        let proc_root = root.join("proc");
        fs::create_dir_all(&proc_root).unwrap();
        proc_root
    }

    #[test]
    fn unbinds_only_the_display_master() {
        let root = temp_root("master-only");
        let sys = root.join("sys");
        let proc_root = empty_proc_root(&root);

        let i2c_dir = sys.join("bus/i2c/drivers/adv7511");
        fs::create_dir_all(i2c_dir.join("3-0039")).unwrap();
        write_file(&i2c_dir.join("uevent"), "");
        fs::create_dir_all(sys.join("bus/platform/drivers/msm-mdss")).unwrap();
        let device = sys.join("bus/platform/devices/1a00000.display-subsystem");
        fs::create_dir_all(&device).unwrap();
        symlink(
            sys.join("bus/platform/drivers/msm-mdss"),
            device.join("driver"),
        )
        .unwrap();
        write_file(&sys.join("kernel/debug/devices_deferred"), "");

        let report = quiesce_with(&sys, &proc_root, 1, Duration::ZERO);

        assert_eq!(
            report.unbound,
            vec!["1a00000.display-subsystem"],
            "only the msm-mdss display master may be unbound"
        );
        assert_eq!(
            fs::read_to_string(sys.join("bus/platform/drivers/msm-mdss/unbind")).unwrap(),
            "1a00000.display-subsystem"
        );
        assert!(
            !i2c_dir.join("unbind").exists(),
            "the adv7511 bridge must be left bound"
        );
        assert!(report.guard_passed);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn accepts_legacy_msm_mdss_driver_name() {
        let root = temp_root("legacy");
        let sys = root.join("sys");
        let proc_root = empty_proc_root(&root);

        fs::create_dir_all(sys.join("bus/platform/drivers/msm_mdss")).unwrap();
        let device = sys.join("bus/platform/devices/1a00000.display-subsystem");
        fs::create_dir_all(&device).unwrap();
        symlink(
            sys.join("bus/platform/drivers/msm_mdss"),
            device.join("driver"),
        )
        .unwrap();

        let report = quiesce_with(&sys, &proc_root, 1, Duration::ZERO);

        assert_eq!(report.unbound, vec!["1a00000.display-subsystem"]);
        assert_eq!(
            fs::read_to_string(sys.join("bus/platform/drivers/msm_mdss/unbind")).unwrap(),
            "1a00000.display-subsystem"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn skips_absent_driver_directories() {
        let root = temp_root("absent");
        let sys = root.join("sys");
        let proc_root = empty_proc_root(&root);
        fs::create_dir_all(&sys).unwrap();

        let report = quiesce_with(&sys, &proc_root, 1, Duration::ZERO);

        assert!(report.unbound.is_empty());
        assert!(report.guard_passed);
        assert!(!sys.join("bus/i2c/drivers/adv7511/unbind").exists());
        assert!(!sys.join("bus/platform/drivers/msm-mdss/unbind").exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn guard_fails_on_deferred_devices() {
        let root = temp_root("deferred");
        let sys = root.join("sys");
        let proc_root = empty_proc_root(&root);
        write_file(&sys.join("kernel/debug/devices_deferred"), "3-0039\n");

        let report = quiesce_with(&sys, &proc_root, 1, Duration::ZERO);

        assert!(!report.guard_passed);
        assert_eq!(report.deferred_devices, vec!["3-0039"]);
        assert!(report.probe_in_flight.is_empty());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn guard_fails_on_probe_in_flight() {
        let root = temp_root("probe");
        let sys = root.join("sys");
        let proc_root = empty_proc_root(&root);
        fs::create_dir_all(&sys).unwrap();
        write_file(&proc_root.join("36/wchan"), "i2c_qup_xfer+0x0/0x100\n");
        write_file(&proc_root.join("36/comm"), "kworker/u19:0\n");
        write_file(&proc_root.join("1/wchan"), "do_wait\n");

        let report = quiesce_with(&sys, &proc_root, 1, Duration::ZERO);

        assert!(!report.guard_passed);
        assert!(report.deferred_devices.is_empty());
        assert_eq!(
            report.probe_in_flight,
            vec!["36:kworker/u19:0:i2c_qup_xfer+0x0/0x100"]
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ignores_unrelated_wchan() {
        let root = temp_root("wchan");
        let sys = root.join("sys");
        let proc_root = empty_proc_root(&root);
        fs::create_dir_all(&sys).unwrap();
        write_file(&proc_root.join("1/wchan"), "do_wait\n");

        let report = quiesce_with(&sys, &proc_root, 1, Duration::ZERO);

        assert!(report.guard_passed);
        assert!(report.probe_in_flight.is_empty());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rejects_unsafe_device_names() {
        assert!(is_plain_name("3-0039"));
        assert!(is_plain_name("1a00000.display-subsystem"));
        assert!(!is_plain_name(""));
        assert!(!is_plain_name("."));
        assert!(!is_plain_name(".."));
        assert!(!is_plain_name("../../etc/passwd"));
        assert!(!is_plain_name("bus/../escape"));
    }
}
