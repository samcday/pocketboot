use std::{
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use crate::cmdline::KernelCommandLine;

const CMDLINE_PARAM: &str = "pocketboot.usb_role";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Policy {
    #[default]
    Unchanged,
    Device,
}

impl Policy {
    pub(crate) fn from_cmdline(cmdline: &KernelCommandLine) -> Result<Self, String> {
        // values() omits empty values; neither a bare flag nor an empty value
        // is an opt-in to changing the controller's role.
        if cmdline.is_set(CMDLINE_PARAM) || cmdline.is_set(&format!("{CMDLINE_PARAM}=")) {
            return Err(format!("{CMDLINE_PARAM} requires the value 'device'"));
        }

        let mut policy = Self::Unchanged;
        for value in cmdline.values(CMDLINE_PARAM) {
            if value != "device" {
                return Err(format!(
                    "unsupported {CMDLINE_PARAM}={value}; only 'device' is supported"
                ));
            }
            policy = Self::Device;
        }
        Ok(policy)
    }

    /// Apply the opt-in after binding the selected UDC. Do not access role
    /// switches at all under the default policy.
    pub(crate) fn apply(self, sys_class: &Path, udc: &OsStr) -> io::Result<Option<PathBuf>> {
        if self == Self::Unchanged {
            return Ok(None);
        }

        request_device_role(sys_class, udc)
            .map(Some)
            .map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("request USB device role for UDC {}: {err}", udc.display()),
                )
            })
    }
}

fn request_device_role(sys_class: &Path, udc: &OsStr) -> io::Result<PathBuf> {
    let udc_device = canonical_device(&sys_class.join("udc").join(udc))?;
    let role_class = sys_class.join("usb_role");
    let entries = fs::read_dir(&role_class).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("read USB role-switch class {}: {err}", role_class.display()),
        )
    })?;
    let mut matched: Option<PathBuf> = None;
    for entry in entries {
        let entry = entry.map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "read USB role-switch entry in {}: {err}",
                    role_class.display()
                ),
            )
        })?;
        let role_switch = entry.path();
        // Fail closed if an entry cannot be resolved: uniqueness is unproven.
        if canonical_device(&role_switch)? != udc_device {
            continue;
        }
        if let Some(previous) = matched {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ambiguous USB role switches for {}: {} and {}",
                    udc_device.display(),
                    previous.display(),
                    role_switch.display()
                ),
            ));
        }
        matched = Some(role_switch);
    }

    let role_switch = matched.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no USB role switch matches UDC device {}",
                udc_device.display()
            ),
        )
    })?;
    let role = role_switch.join("role");
    // Open only after proving a unique match, and never create a missing
    // attribute. No unrelated controller may receive a role write.
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&role)
        .and_then(|mut file| file.write_all(b"device"))
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "write device to USB role attribute {}: {err}",
                    role.display()
                ),
            )
        })?;
    Ok(role_switch)
}

fn canonical_device(class_device: &Path) -> io::Result<PathBuf> {
    let device = class_device.join("device");
    fs::canonicalize(&device).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("resolve USB controller device {}: {err}", device.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct FakeSysfs {
        root: PathBuf,
    }

    impl FakeSysfs {
        fn new() -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "pocketboot-usb-role-test-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::create_dir_all(root.join("class/udc")).unwrap();
            fs::create_dir_all(root.join("class/usb_role")).unwrap();
            fs::create_dir(root.join("devices")).unwrap();
            Self { root }
        }

        fn controller(&self, name: &str) -> PathBuf {
            let path = self.root.join("devices").join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn udc(&self, name: &str, controller: &Path) {
            let path = self.root.join("class/udc").join(name);
            fs::create_dir(&path).unwrap();
            symlink(controller, path.join("device")).unwrap();
        }

        fn role_switch(&self, name: &str, controller: &Path) -> PathBuf {
            let path = self.root.join("class/usb_role").join(name);
            fs::create_dir(&path).unwrap();
            symlink(controller, path.join("device")).unwrap();
            fs::write(path.join("role"), b"none\n").unwrap();
            path
        }

        fn apply(&self, policy: Policy, udc: &str) -> io::Result<Option<PathBuf>> {
            policy.apply(&self.root.join("class"), OsStr::new(udc))
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn assert_unchanged(role_switch: &Path) {
        assert_eq!(fs::read(role_switch.join("role")).unwrap(), b"none\n");
    }

    #[test]
    fn usb_role_requires_explicit_opt_in() {
        for cmdline in [
            "",
            "pocketboot.usb_role.extra=device",
            "other.usb_role=device",
        ] {
            assert_eq!(
                Policy::from_cmdline(&KernelCommandLine::parse(cmdline)).unwrap(),
                Policy::Unchanged
            );
        }
        for cmdline in [
            "foo pocketboot.usb_role=device bar",
            "pocketboot.usb_role=device pocketboot.usb_role=device",
        ] {
            assert_eq!(
                Policy::from_cmdline(&KernelCommandLine::parse(cmdline)).unwrap(),
                Policy::Device
            );
        }
    }

    #[test]
    fn usb_role_rejects_unsupported_and_missing_values() {
        for cmdline in [
            "pocketboot.usb_role=host",
            "pocketboot.usb_role=none",
            "pocketboot.usb_role=otg",
            "pocketboot.usb_role=Device",
            "pocketboot.usb_role",
            "pocketboot.usb_role=",
            "pocketboot.usb_role=device pocketboot.usb_role=host",
            "pocketboot.usb_role=host pocketboot.usb_role=device",
            "pocketboot.usb_role=device pocketboot.usb_role=",
        ] {
            let err = Policy::from_cmdline(&KernelCommandLine::parse(cmdline)).unwrap_err();
            assert!(err.contains(CMDLINE_PARAM), "{cmdline}: {err}");
            assert!(err.contains("device"), "{cmdline}: {err}");
        }
    }

    #[test]
    fn unchanged_policy_does_not_access_sysfs() {
        let sysfs = FakeSysfs::new();
        assert_eq!(
            Policy::default()
                .apply(&sysfs.root.join("nonexistent"), OsStr::new("missing-udc"))
                .unwrap(),
            None
        );
        let controller = sysfs.controller("controller");
        sysfs.udc("chosen", &controller);
        let role_switch = sysfs.role_switch("switch", &controller);
        assert_eq!(sysfs.apply(Policy::Unchanged, "chosen").unwrap(), None);
        assert_unchanged(&role_switch);
    }

    #[test]
    fn matches_only_the_exact_canonical_controller_parent() {
        let sysfs = FakeSysfs::new();
        let controller = sysfs.controller("controller");
        let alias = sysfs.root.join("controller-alias");
        symlink(&controller, &alias).unwrap();
        sysfs.udc("chosen", &controller);
        let matched = sysfs.role_switch("z-matched", &alias);
        let sibling = sysfs.role_switch("a-unrelated", &sysfs.controller("controller-other"));
        let child = sysfs.role_switch("b-child", &sysfs.controller("controller/child"));
        let parent = sysfs.role_switch("c-parent", &sysfs.root.join("devices"));
        // The other controller's UDC sorts first; only the already-selected
        // UDC's device parent determines which switch is eligible.
        sysfs.udc("a-other", &sysfs.root.join("devices/controller-other"));

        assert_eq!(
            sysfs.apply(Policy::Device, "chosen").unwrap(),
            Some(matched.clone())
        );
        assert_eq!(fs::read(matched.join("role")).unwrap(), b"device");
        for unrelated in [&sibling, &child, &parent] {
            assert_unchanged(unrelated);
        }
    }

    #[test]
    fn missing_match_does_not_write_other_switches() {
        let sysfs = FakeSysfs::new();
        sysfs.udc("chosen", &sysfs.controller("controller"));
        let unrelated = sysfs.role_switch("unrelated", &sysfs.controller("other"));

        let err = sysfs.apply(Policy::Device, "chosen").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("no USB role switch matches"));
        assert!(err.to_string().contains("chosen"));
        assert_unchanged(&unrelated);
    }

    #[test]
    fn ambiguous_match_does_not_write_any_switch() {
        let sysfs = FakeSysfs::new();
        let controller = sysfs.controller("controller");
        sysfs.udc("chosen", &controller);
        let first = sysfs.role_switch("first", &controller);
        let second = sysfs.role_switch("second", &controller);
        let unrelated = sysfs.role_switch("unrelated", &sysfs.controller("other"));

        let err = sysfs.apply(Policy::Device, "chosen").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("ambiguous"));
        for role_switch in [&first, &second, &unrelated] {
            assert_unchanged(role_switch);
        }
    }

    #[test]
    fn missing_role_class_is_an_error() {
        let sysfs = FakeSysfs::new();
        sysfs.udc("chosen", &sysfs.controller("controller"));
        fs::remove_dir(sysfs.root.join("class/usb_role")).unwrap();

        let err = sysfs.apply(Policy::Device, "chosen").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("read USB role-switch class"));
    }

    #[test]
    fn unresolved_device_symlink_prevents_role_writes() {
        let sysfs = FakeSysfs::new();
        let controller = sysfs.controller("controller");
        sysfs.udc("chosen", &controller);
        let matched = sysfs.role_switch("matched", &controller);
        let broken = sysfs.role_switch("broken", &sysfs.root.join("missing"));

        let err = sysfs.apply(Policy::Device, "chosen").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("resolve USB controller device"));
        assert_unchanged(&matched);
        assert_unchanged(&broken);
    }

    #[test]
    fn missing_udc_device_is_an_error_without_writes() {
        let sysfs = FakeSysfs::new();
        let role_switch = sysfs.role_switch("switch", &sysfs.controller("controller"));

        let err = sysfs.apply(Policy::Device, "missing").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("udc/missing/device"));
        assert_unchanged(&role_switch);
    }

    #[test]
    fn missing_role_attribute_is_not_created() {
        let sysfs = FakeSysfs::new();
        let controller = sysfs.controller("controller");
        sysfs.udc("chosen", &controller);
        let matched = sysfs.role_switch("matched", &controller);
        let unrelated = sysfs.role_switch("unrelated", &sysfs.controller("other"));
        let role = matched.join("role");
        fs::remove_file(&role).unwrap();

        let err = sysfs.apply(Policy::Device, "chosen").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains(&role.display().to_string()));
        assert!(!role.exists());
        assert_unchanged(&unrelated);
    }

    #[test]
    fn unwritable_role_attribute_returns_a_contextual_error() {
        let sysfs = FakeSysfs::new();
        let controller = sysfs.controller("controller");
        sysfs.udc("chosen", &controller);
        let matched = sysfs.role_switch("matched", &controller);
        let unrelated = sysfs.role_switch("unrelated", &sysfs.controller("other"));
        let role = matched.join("role");
        fs::set_permissions(&role, fs::Permissions::from_mode(0o444)).unwrap();
        let expected_kind = if fs::OpenOptions::new().write(true).open(&role).is_ok() {
            // Privileged test runners can bypass file permissions. A directory
            // still provides a deterministic open failure without real sysfs.
            fs::remove_file(&role).unwrap();
            fs::create_dir(&role).unwrap();
            io::ErrorKind::IsADirectory
        } else {
            io::ErrorKind::PermissionDenied
        };

        let err = sysfs.apply(Policy::Device, "chosen").unwrap_err();
        assert_eq!(err.kind(), expected_kind);
        assert!(
            err.to_string()
                .contains("write device to USB role attribute")
        );
        assert!(err.to_string().contains(&role.display().to_string()));
        if role.is_file() {
            assert_unchanged(&matched);
        }
        assert_unchanged(&unrelated);
    }
}
