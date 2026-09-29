//! Preserve previous-boot evidence before any recovery interfaces start.
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

const STORE: &str = "/sys/fs/pstore";
const SNAPSHOT: &str = "/run/pocketboot/pstore";
const PSTORE_MAGIC: u64 = 0x6165676c;
const MOUNT_FLAGS: libc::c_ulong =
    libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;

/// Best effort only: losing access to evidence must not prevent recovery boot.
pub fn capture() {
    match mount_store() {
        Ok(false) => tracing::info!("pstore is not supported by this kernel"),
        Err(error) => tracing::warn!(%error, "pstore mount failed"),
        Ok(true) => match copy_store(Path::new(STORE), Path::new(SNAPSHOT)) {
            Ok(Capture::Empty) => tracing::info!("pstore contains no regular records"),
            Ok(Capture::Captured(records)) => {
                tracing::info!(
                    records,
                    path = SNAPSHOT,
                    "captured previous-boot pstore records"
                );
            }
            Ok(Capture::Existing) => {
                tracing::warn!(path = SNAPSHOT, "preserving existing pstore snapshot");
            }
            Err(error) => tracing::warn!(%error, "pstore capture failed"),
        },
    }
}

fn mount_store() -> io::Result<bool> {
    // Check first: kernels without pstore may not expose its mountpoint in
    // sysfs, where mkdir itself would otherwise fail before mount can say ENODEV.
    if !fs::read_to_string("/proc/filesystems")?
        .lines()
        .any(|line| line.split_whitespace().last() == Some("pstore"))
    {
        return Ok(false);
    }
    fs::create_dir_all(STORE)?;
    let path = cstring(Path::new(STORE))?;
    let already_mounted = store_magic(&path)? == PSTORE_MAGIC;
    // Even a pre-existing pstore mount must have the recovery mount's protections.
    let flags = MOUNT_FLAGS | if already_mounted { libc::MS_REMOUNT } else { 0 };
    let result = unsafe {
        libc::mount(
            c"pstore".as_ptr(),
            path.as_ptr(),
            c"pstore".as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        // ENODEV specifically means the filesystem type is unavailable.
        if !already_mounted && error.raw_os_error() == Some(libc::ENODEV) {
            return Ok(false);
        }
        return Err(error);
    }
    // Never copy an ordinary directory just because a mount appeared to succeed.
    if store_magic(&path)? != PSTORE_MAGIC {
        return Err(io::Error::other("mount is not a pstore filesystem"));
    }
    Ok(true)
}

/// `statfs.f_type` is signed on some targets and unsigned on others; compare
/// the filesystem magic through one width.
fn store_magic(path: &CString) -> io::Result<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() }.f_type as u64)
}

#[derive(Debug, PartialEq, Eq)]
enum Capture {
    Empty,
    Captured(usize),
    Existing,
}

fn cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn context(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{action} {}: {error}", path.display()),
    )
}

/// Copy opaque bytes and names, without clearing the source. A snapshot is
/// immutable: if the destination exists (even as a symlink), leave it alone.
/// Stage beside it and publish with RENAME_NOREPLACE so errors and concurrent
/// captures cannot expose partial records or replace an earlier snapshot.
fn copy_store(source: &Path, destination: &Path) -> io::Result<Capture> {
    copy_store_using(source, destination, |input, output| io::copy(input, output))
}

fn copy_store_using(
    source: &Path,
    destination: &Path,
    mut copy: impl FnMut(&mut File, &mut File) -> io::Result<u64>,
) -> io::Result<Capture> {
    match fs::symlink_metadata(destination) {
        Ok(_) => return Ok(Capture::Existing),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(context("inspect snapshot", destination, error)),
    }
    let entries = fs::read_dir(source).map_err(|error| context("read store", source, error))?;
    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "snapshot needs a parent directory",
        )
    })?;
    fs::create_dir_all(parent).map_err(|error| context("create snapshot parent", parent, error))?;
    let template = cstring(&parent.join(".pstore-XXXXXX"))?;
    let mut template = template.into_bytes_with_nul();
    if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
        return Err(context(
            "create staging directory",
            parent,
            io::Error::last_os_error(),
        ));
    }
    let staging = Staging(PathBuf::from(std::ffi::OsStr::from_bytes(
        &template[..template.len() - 1],
    )));
    let mut records = 0;
    for entry in entries {
        let entry = entry.map_err(|error| context("read store entry", source, error))?;
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|error| context("inspect record", &path, error))?
            .is_file()
        {
            continue;
        }
        // NOFOLLOW closes the symlink race; NONBLOCK avoids hanging on a FIFO
        // substituted after file_type(). Check the opened object again.
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|error| context("open record", &path, error))?;
        if !input
            .metadata()
            .map_err(|error| context("inspect opened record", &path, error))?
            .is_file()
        {
            continue;
        }
        let output_path = staging.0.join(entry.file_name());
        let mut output = File::create_new(&output_path)
            .map_err(|error| context("create staged record", &output_path, error))?;
        copy(&mut input, &mut output).map_err(|error| context("copy record", &path, error))?;
        records += 1;
    }
    if records == 0 {
        return Ok(Capture::Empty);
    }
    let from = cstring(&staging.0)?;
    let to = cstring(destination)?;
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::AlreadyExists {
            return Ok(Capture::Existing);
        }
        return Err(context("publish snapshot", destination, error));
    }
    Ok(Capture::Captured(records))
}

struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        // Only our private staging directory; never remove anything in pstore.
        if let Err(error) = fs::remove_dir_all(&self.0)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, path = %self.0.display(), "failed to remove pstore staging directory");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "pocketboot-pstore-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::create_dir(path.join("source")).unwrap();
            Self(path)
        }
        fn source(&self) -> PathBuf {
            self.0.join("source")
        }
        fn destination(&self) -> PathBuf {
            self.0.join("snapshot")
        }
        fn copy(&self) -> io::Result<Capture> {
            copy_store(&self.source(), &self.destination())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn empty_store() {
        let f = Fixture::new();
        assert_eq!(f.copy().unwrap(), Capture::Empty);
        assert!(!f.destination().exists());
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    }

    #[test]
    fn byte_exact_and_immutable_snapshot() {
        let f = Fixture::new();
        let name = "dmesg-ramoops-0.enc.z";
        let bytes = [0, 0xff, 0x1f, 0x8b, 0, 13, 10];
        fs::write(f.source().join(name), bytes).unwrap();
        assert_eq!(f.copy().unwrap(), Capture::Captured(1));
        assert_eq!(fs::read(f.source().join(name)).unwrap(), bytes);
        assert_eq!(fs::read(f.destination().join(name)).unwrap(), bytes);
        fs::write(f.source().join(name), b"changed").unwrap();
        fs::write(f.source().join("new-record"), b"new").unwrap();
        assert_eq!(f.copy().unwrap(), Capture::Existing);
        assert_eq!(fs::read(f.destination().join(name)).unwrap(), bytes);
        assert!(!f.destination().join("new-record").exists());
    }

    #[test]
    fn ignores_non_regular_entries() {
        let f = Fixture::new();
        fs::create_dir(f.source().join("directory")).unwrap();
        fs::write(f.source().join("directory/hidden"), b"hidden").unwrap();
        symlink("directory/hidden", f.source().join("link")).unwrap();
        symlink("missing", f.source().join("dangling")).unwrap();
        let fifo = cstring(&f.source().join("fifo")).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert_eq!(f.copy().unwrap(), Capture::Empty);
        fs::write(f.source().join("regular"), b"record").unwrap();
        assert_eq!(f.copy().unwrap(), Capture::Captured(1));
        assert_eq!(fs::read_dir(f.destination()).unwrap().count(), 1);
    }

    #[test]
    fn preserves_destination_collisions_including_symlinks() {
        let f = Fixture::new();
        fs::write(f.destination(), b"existing").unwrap();
        assert_eq!(f.copy().unwrap(), Capture::Existing);
        assert_eq!(fs::read(f.destination()).unwrap(), b"existing");
        fs::remove_file(f.destination()).unwrap();
        symlink("missing", f.destination()).unwrap();
        assert_eq!(f.copy().unwrap(), Capture::Existing);
        assert_eq!(
            fs::read_link(f.destination()).unwrap(),
            Path::new("missing")
        );
    }

    #[test]
    fn reports_paths_and_operations_for_failures() {
        let f = Fixture::new();
        fs::remove_dir(f.source()).unwrap();
        let error = f.copy().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("read store"));
        assert!(error.to_string().contains("source"));
        assert!(!f.destination().exists());
        fs::create_dir(f.source()).unwrap();
        fs::write(f.0.join("blocked"), b"not a directory").unwrap();
        let error = copy_store(&f.source(), &f.0.join("blocked/snapshot")).unwrap_err();
        assert!(error.to_string().contains("blocked"));
        assert!(!f.destination().exists());
    }

    #[test]
    fn failed_copy_never_publishes_partial_records() {
        use std::io::Write;
        let f = Fixture::new();
        fs::write(f.source().join("record"), b"complete").unwrap();
        let error = copy_store_using(&f.source(), &f.destination(), |_, output| {
            output.write_all(b"partial")?;
            Err(io::Error::other("injected read failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("copy record"));
        assert!(error.to_string().contains("injected read failure"));
        assert!(!f.destination().exists());
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
        assert_eq!(fs::read(f.source().join("record")).unwrap(), b"complete");
    }

    #[test]
    fn a_later_copy_failure_keeps_the_complete_source_set_available() {
        use std::io::Write;
        let f = Fixture::new();
        fs::write(f.source().join("console-ramoops-0"), b"console").unwrap();
        fs::write(f.source().join("dmesg-ramoops-0.enc.z"), b"opaque").unwrap();
        let mut attempted = 0;
        let error = copy_store_using(&f.source(), &f.destination(), |input, output| {
            attempted += 1;
            if attempted == 1 {
                io::copy(input, output)
            } else {
                output.write_all(b"partial")?;
                Err(io::Error::other("injected later copy failure"))
            }
        })
        .unwrap_err();
        assert_eq!(attempted, 2);
        assert!(error.to_string().contains("injected later copy failure"));
        assert!(!f.destination().exists());
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
        assert_eq!(
            fs::read(f.source().join("console-ramoops-0")).unwrap(),
            b"console"
        );
        assert_eq!(
            fs::read(f.source().join("dmesg-ramoops-0.enc.z")).unwrap(),
            b"opaque"
        );
    }

    #[test]
    fn concurrent_destination_is_not_replaced() {
        let f = Fixture::new();
        fs::write(f.source().join("record"), b"new").unwrap();
        let result = copy_store_using(&f.source(), &f.destination(), |input, output| {
            fs::create_dir(f.destination())?;
            fs::write(f.destination().join("record"), b"existing")?;
            io::copy(input, output)
        })
        .unwrap();
        assert_eq!(result, Capture::Existing);
        assert_eq!(
            fs::read(f.destination().join("record")).unwrap(),
            b"existing"
        );
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 2);
    }
}
