use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

use crate::{
    fastboot::{CommandContext, CommandResult},
    kexec,
};

const COMMAND_PREFIX: &str = "oem cat:";
const MAX_STAGED_SIZE: u64 = u32::MAX as u64;
const COPY_CHUNK: usize = 1024 * 1024;

pub(super) fn handle(context: &mut CommandContext<'_>, command: &str) -> io::Result<CommandResult> {
    context.clear_staged();
    let path = parse_path(command)?;
    let staged = stage_file(path)?;
    let bytes = staged.size;

    context.stage_file(format!("cat:{path}"), staged.file, bytes);
    context.info(format!("{path} staged ({bytes} bytes)"))?;
    context.info(b"run fastboot get_staged /dev/stdout to view")?;
    context.okay(b"")?;
    Ok(CommandResult::continue_())
}

struct StagedFile {
    file: File,
    size: u64,
}

fn parse_path(command: &str) -> io::Result<&str> {
    let path = command
        .strip_prefix(COMMAND_PREFIX)
        .ok_or_else(|| invalid_input("invalid cat command"))?;

    if path.is_empty() {
        return Err(invalid_input("cat path is empty"));
    }

    Ok(path)
}

fn stage_file(path: &str) -> io::Result<StagedFile> {
    let mut source = File::open(path)
        .map_err(|err| io::Error::new(err.kind(), format!("open {path}: {err}")))?;
    let mut staged = kexec::create_payload_memfd(
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("cat"),
    )?;

    let size = copy_limited(&mut source, &mut staged)
        .map_err(|err| io::Error::new(err.kind(), format!("read {path}: {err}")))?;
    staged.seek(SeekFrom::Start(0))?;

    Ok(StagedFile { file: staged, size })
}

fn copy_limited(source: &mut File, staged: &mut File) -> io::Result<u64> {
    let mut buffer = vec![0; COPY_CHUNK];
    let mut total = 0;

    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            return Ok(total);
        }

        total = total
            .checked_add(read as u64)
            .ok_or_else(|| invalid_input("cat file is too large"))?;
        if total > MAX_STAGED_SIZE {
            return Err(invalid_input("cat file is too large"));
        }

        staged.write_all(&buffer[..read])?;
    }
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Read,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn parses_cat_path() {
        assert_eq!(
            parse_path("oem cat:/proc/cmdline").unwrap(),
            "/proc/cmdline"
        );
    }

    #[test]
    fn rejects_empty_cat_path() {
        let err = parse_path("oem cat:").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn stages_file_contents() {
        let temp = TempFile::new(b"hello from device\n");
        let staged = stage_file(temp.path.to_str().unwrap()).unwrap();

        assert_eq!(staged.size, 18);
        assert_eq!(read_staged(staged.file), b"hello from device\n");
    }

    #[test]
    fn stages_empty_file() {
        let temp = TempFile::new(b"");
        let staged = stage_file(temp.path.to_str().unwrap()).unwrap();

        assert_eq!(staged.size, 0);
        assert!(read_staged(staged.file).is_empty());
    }

    struct TempFile {
        path: std::path::PathBuf,
    }

    impl TempFile {
        fn new(contents: &[u8]) -> Self {
            let mut path = std::env::temp_dir();
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            path.push(format!(
                "pocketboot-cat-test-{}-{nonce}",
                std::process::id()
            ));
            fs::write(&path, contents).unwrap();
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn read_staged(mut staged: File) -> Vec<u8> {
        let mut data = Vec::new();
        staged.read_to_end(&mut data).unwrap();
        data
    }
}
