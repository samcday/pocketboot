use std::{
    ffi::CString,
    fs::{self, File},
    io,
    os::fd::AsRawFd,
    path::Path,
};

use crate::Result;

const BOOTMENU_MODULES: &str = "/etc/pocketboot/modules/bootmenu.list";
const MODULE_INIT_COMPRESSED_FILE: libc::c_int = 4;

pub(crate) fn load_bootmenu() -> Result<()> {
    load_manifest(BOOTMENU_MODULES)
}

fn load_manifest(path: &str) -> Result<()> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            tracing::info!(path, "kernel module manifest not found");
            return Ok(());
        }
        Err(err) => return Err(format!("read kernel module manifest {path}: {err}")),
    };

    let mut failures = 0;
    for line in contents.lines() {
        let module = line.trim();
        if module.is_empty() || module.starts_with('#') {
            continue;
        }
        if let Err(err) = load_module(Path::new(module)) {
            failures += 1;
            tracing::warn!(path = module, error = %err, "failed to load kernel module");
        }
    }

    if failures == 0 {
        Ok(())
    } else {
        Err(format!("failed to load {failures} bootmenu kernel modules"))
    }
}

fn load_module(path: &Path) -> Result<()> {
    let file = File::open(path).map_err(|err| format!("open {}: {err}", path.display()))?;
    let params = CString::new("").expect("empty string has no NUL");
    let flags = if path.extension().and_then(|extension| extension.to_str()) == Some("xz") {
        MODULE_INIT_COMPRESSED_FILE
    } else {
        0
    };

    let rc = unsafe {
        libc::syscall(
            libc::SYS_finit_module,
            file.as_raw_fd(),
            params.as_ptr(),
            flags,
        )
    };
    if rc == 0 {
        tracing::info!(path = %path.display(), "loaded kernel module");
        return Ok(());
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EEXIST) {
        tracing::debug!(path = %path.display(), "kernel module already loaded");
        return Ok(());
    }

    Err(format!("finit_module {}: {err}", path.display()))
}
