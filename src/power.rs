use std::{ffi::CString, fs, io, path::Path};

const REBOOT_MODE_CLASS: &str = "/sys/class/reboot-mode";
const BOOTLOADER_REBOOT_MODE: &str = "bootloader";
const LINUX_REBOOT_MAGIC1: libc::c_int = 0xfee1_dead_u32 as libc::c_int;
const LINUX_REBOOT_MAGIC2: libc::c_int = 672_274_793;
const LINUX_REBOOT_CMD_RESTART: libc::c_int = 0x0123_4567;
const LINUX_REBOOT_CMD_RESTART2: libc::c_int = 0xa1b2_c3d4_u32 as libc::c_int;
const LINUX_REBOOT_CMD_POWER_OFF: libc::c_int = 0x4321_fedc_u32 as libc::c_int;

pub(crate) fn reboot() -> io::Result<()> {
    reboot_command(LINUX_REBOOT_CMD_RESTART, "rebooting system")
}

pub(crate) fn power_off() -> io::Result<()> {
    reboot_command(LINUX_REBOOT_CMD_POWER_OFF, "powering off system")
}

pub(crate) fn bootloader_reboot_action() -> io::Result<fn() -> io::Result<()>> {
    require_reboot_mode(Path::new(REBOOT_MODE_CLASS), BOOTLOADER_REBOOT_MODE)?;
    Ok(reboot_to_bootloader)
}

fn reboot_command(command: libc::c_int, message: &str) -> io::Result<()> {
    tracing::info!("{message}");
    let rc = unsafe { libc::reboot(command) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    Err(io::Error::other("reboot returned unexpectedly"))
}

fn reboot_to_bootloader() -> io::Result<()> {
    reboot_command_with_argument(
        LINUX_REBOOT_CMD_RESTART2,
        BOOTLOADER_REBOOT_MODE,
        "rebooting system to bootloader",
    )
}

fn reboot_command_with_argument(
    command: libc::c_int,
    argument: &str,
    message: &str,
) -> io::Result<()> {
    let argument = CString::new(argument)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "reboot argument contains NUL"))?;
    tracing::info!(reboot_argument = %argument.to_string_lossy(), "{message}");
    let rc = unsafe {
        libc::syscall(
            libc::SYS_reboot,
            LINUX_REBOOT_MAGIC1,
            LINUX_REBOOT_MAGIC2,
            command,
            argument.as_ptr(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    Err(io::Error::other("reboot returned unexpectedly"))
}

fn require_reboot_mode(class: &Path, mode: &str) -> io::Result<()> {
    let entries = fs::read_dir(class).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("read reboot-mode class {}: {err}", class.display()),
        )
    })?;
    let mut first_read_error = None;

    for entry in entries {
        let entry = entry.map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("read reboot-mode entry under {}: {err}", class.display()),
            )
        })?;
        let modes_path = entry.path().join("reboot_modes");
        match fs::read_to_string(&modes_path) {
            Ok(modes) if reboot_modes_contain(&modes, mode) => return Ok(()),
            Ok(_) => {}
            Err(err) if first_read_error.is_none() => {
                first_read_error = Some(io::Error::new(
                    err.kind(),
                    format!("read {}: {err}", modes_path.display()),
                ));
            }
            Err(_) => {}
        }
    }

    if let Some(err) = first_read_error {
        return Err(err);
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "reboot mode {mode:?} is not advertised under {}",
            class.display()
        ),
    ))
}

fn reboot_modes_contain(modes: &str, expected: &str) -> bool {
    modes.split_ascii_whitespace().any(|mode| mode == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "pocketboot-reboot-mode-test-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn driver(&self, name: &str, modes: &str) {
            let driver = self.path.join(name);
            fs::create_dir_all(&driver).unwrap();
            fs::write(driver.join("reboot_modes"), modes).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn accepts_advertised_bootloader_mode() {
        let class = TempDir::new();
        class.driver("qcom-pon", "recovery bootloader\n");

        require_reboot_mode(&class.path, "bootloader").unwrap();
    }

    #[test]
    fn rejects_unadvertised_bootloader_mode() {
        let class = TempDir::new();
        class.driver("qcom-pon", "recovery\n");

        let err = require_reboot_mode(&class.path, "bootloader").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(err.to_string().contains("not advertised"));
    }

    #[test]
    fn rejects_missing_reboot_mode_class() {
        let class = TempDir::new();
        let missing = class.path.join("missing");

        let err = require_reboot_mode(&missing, "bootloader").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("read reboot-mode class"));
    }

    #[test]
    fn reboot_mode_matching_uses_complete_tokens() {
        assert!(reboot_modes_contain("recovery bootloader\n", "bootloader"));
        assert!(!reboot_modes_contain(
            "recovery bootloader-debug\n",
            "bootloader"
        ));
    }
}
