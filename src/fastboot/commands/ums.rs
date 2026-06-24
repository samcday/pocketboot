use std::{
    fs, io,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
};

use crate::{
    fastboot::{Command, CommandContext, CommandResult},
    gadget::{Gadget, MassStorageStart, MassStorageStop},
};

const START_COMMAND_PREFIX: &str = "oem ums-start:";
const STOP_COMMAND_PREFIX: &str = "oem ums-stop:";
const DEV: &str = "/dev";

#[derive(Clone)]
pub(super) struct FastbootUms {
    gadget: Gadget,
    dev_root: PathBuf,
}

impl FastbootUms {
    pub(super) fn new(gadget: Gadget) -> Self {
        Self {
            gadget,
            dev_root: PathBuf::from(DEV),
        }
    }

    pub(super) fn commands(self) -> Vec<Command> {
        let start = self.clone();
        vec![
            Command::prefix(
                START_COMMAND_PREFIX,
                move |context: &mut CommandContext<'_>, command: &str| {
                    start.handle_start(context, command)
                },
            ),
            Command::prefix(
                STOP_COMMAND_PREFIX,
                move |context: &mut CommandContext<'_>, command: &str| {
                    self.handle_stop(context, command)
                },
            ),
        ]
    }

    fn handle_start(
        &self,
        context: &mut CommandContext<'_>,
        command: &str,
    ) -> io::Result<CommandResult> {
        let requested = parse_backing(command, START_COMMAND_PREFIX)?;
        let backing = resolve_backing_path(requested, &self.dev_root)?;
        match self.gadget.start_mass_storage(backing.clone()) {
            Ok(MassStorageStart::Started { lun }) => {
                context.okay(format!(
                    "UMS started for {} on LUN {lun}",
                    backing.display()
                ))?;
                Ok(CommandResult::continue_())
            }
            Ok(MassStorageStart::AlreadyStarted { lun }) => {
                context.okay(format!(
                    "UMS already started for {} on LUN {lun}",
                    backing.display()
                ))?;
                Ok(CommandResult::continue_())
            }
            Err(err) => {
                context.fail(format!("UMS start failed: {err}"))?;
                Ok(CommandResult::continue_())
            }
        }
    }

    fn handle_stop(
        &self,
        context: &mut CommandContext<'_>,
        command: &str,
    ) -> io::Result<CommandResult> {
        let requested = parse_backing(command, STOP_COMMAND_PREFIX)?;
        let backing = resolve_backing_path(requested, &self.dev_root)?;
        match self.gadget.stop_mass_storage(backing.clone()) {
            Ok(MassStorageStop::Stopped { lun }) => {
                context.okay(format!(
                    "UMS stopped for {} from LUN {lun}",
                    backing.display()
                ))?;
                Ok(CommandResult::continue_())
            }
            Err(err) => {
                context.fail(stop_failure_message(&backing, &err))?;
                Ok(CommandResult::continue_())
            }
        }
    }
}

fn parse_backing<'a>(command: &'a str, prefix: &str) -> io::Result<&'a str> {
    let backing = command
        .strip_prefix(prefix)
        .ok_or_else(|| invalid_input("invalid UMS command"))?;
    if backing.is_empty() {
        return Err(invalid_input("UMS backing path is empty"));
    }
    Ok(backing)
}

fn resolve_backing_path(requested: &str, dev_root: &Path) -> io::Result<PathBuf> {
    let direct = Path::new(requested);
    match usable_backing_path(direct) {
        Ok(path) => return Ok(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound && !direct.is_absolute() => {}
        Err(err) => return Err(err),
    }

    match usable_backing_path(&dev_root.join(requested)) {
        Ok(path) => Ok(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
            err.kind(),
            format!(
                "UMS backing path {requested:?} not found directly or under {}",
                dev_root.display()
            ),
        )),
        Err(err) => Err(err),
    }
}

fn usable_backing_path(path: &Path) -> io::Result<PathBuf> {
    let metadata = fs::metadata(path)?;
    let file_type = metadata.file_type();
    if !file_type.is_file() && !file_type.is_block_device() {
        return Err(invalid_input(format!(
            "UMS backing path {} is not a regular file or block device",
            path.display()
        )));
    }

    fs::canonicalize(path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("resolve UMS backing path {}: {err}", path.display()),
        )
    })
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn stop_failure_message(backing: &Path, err: &io::Error) -> String {
    if err.raw_os_error() == Some(libc::EBUSY) {
        format!(
            "UMS stop failed for {}: unmount/eject on host first",
            backing.display()
        )
    } else {
        format!("UMS stop failed for {}: {err}", backing.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_ums_backing_path() {
        assert_eq!(
            parse_backing("oem ums-start:mmcblk0p1", START_COMMAND_PREFIX).unwrap(),
            "mmcblk0p1"
        );
        assert_eq!(
            parse_backing("oem ums-stop:/dev/mmcblk0p1", STOP_COMMAND_PREFIX).unwrap(),
            "/dev/mmcblk0p1"
        );
    }

    #[test]
    fn rejects_empty_ums_backing_path() {
        let err = parse_backing("oem ums-start:", START_COMMAND_PREFIX).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn resolves_existing_direct_path() {
        let temp = TempDir::new();
        let backing = temp.file("disk.img");
        fs::write(&backing, b"disk").unwrap();

        assert_eq!(
            resolve_backing_path(backing.to_str().unwrap(), Path::new("/dev")).unwrap(),
            fs::canonicalize(backing).unwrap()
        );
    }

    #[test]
    fn resolves_dev_root_fallback() {
        let temp = TempDir::new();
        let backing = temp.file("mmcblk0p1");
        fs::write(&backing, b"disk").unwrap();

        assert_eq!(
            resolve_backing_path("mmcblk0p1", &temp.path).unwrap(),
            fs::canonicalize(backing).unwrap()
        );
    }

    #[test]
    fn rejects_non_file_backing_path() {
        let temp = TempDir::new();
        let err = resolve_backing_path(temp.path.to_str().unwrap(), Path::new("/dev")).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let mut path = std::env::temp_dir();
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            path.push(format!(
                "pocketboot-ums-test-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn file(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
