use std::{
    env,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

const DEFAULT_TARGET: &str = "aarch64-unknown-linux-musl";
const INIT_BINARY: &str = "pocketboot";
const DEFAULT_INITRD: &str = "pocketboot-initrd.cpio";

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("cpio") => {
            let args = args.collect::<Vec<_>>();
            if args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
            {
                print_cpio_usage();
                Ok(())
            } else {
                cpio(CpioArgs::parse(args)?)
            }
        }
        Some("help" | "--help" | "-h") | None => {
            print_usage();
            Ok(())
        }
        Some(command) => Err(format!("unknown xtask command: {command}")),
    }
}

#[derive(Debug)]
struct CpioArgs {
    target: String,
    output: Option<PathBuf>,
}

impl CpioArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut target = DEFAULT_TARGET.to_string();
        let mut output = None;
        let mut index = 0;

        while index < args.len() {
            let arg = &args[index];
            match arg.as_str() {
                "--target" => {
                    index += 1;
                    target = args
                        .get(index)
                        .ok_or_else(|| "--target requires a value".to_string())?
                        .to_string();
                }
                "--output" | "-o" => {
                    index += 1;
                    output = Some(PathBuf::from(
                        args.get(index)
                            .ok_or_else(|| "--output requires a value".to_string())?,
                    ));
                }
                value if value.starts_with("--target=") => {
                    target = value["--target=".len()..].to_string();
                }
                value if value.starts_with("--output=") => {
                    output = Some(PathBuf::from(&value["--output=".len()..]));
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown cpio option: {value}"));
                }
                value => {
                    if output.is_some() {
                        return Err(format!("unexpected positional argument: {value}"));
                    }
                    output = Some(PathBuf::from(value));
                }
            }
            index += 1;
        }

        Ok(Self { target, output })
    }
}

fn cpio(args: CpioArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    build_release(&workspace_root, &args.target)?;

    let target_dir = target_dir(&workspace_root);
    let init = target_dir
        .join(&args.target)
        .join("release")
        .join(INIT_BINARY);
    if !init.is_file() {
        return Err(format!("release binary not found: {}", init.display()));
    }

    let output = args
        .output
        .unwrap_or_else(|| target_dir.join(DEFAULT_INITRD));
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }

    write_initrd(&init, &output)?;
    println!("wrote {}", output.display());
    Ok(())
}

fn build_release(workspace_root: &Path, target: &str) -> Result<()> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(workspace_root)
        .args(["build", "--release", "--target", target, "-p", INIT_BINARY])
        .status()
        .map_err(|err| format!("spawn cargo build: {err}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with {status}"));
    }
    Ok(())
}

fn write_initrd(init: &Path, output: &Path) -> Result<()> {
    let mut writer = NewcWriter::create(output)?;
    writer.dir("dev", 0o755)?;
    writer.char_dev("dev/console", 0o600, 5, 1)?;
    writer.dir("proc", 0o755)?;
    writer.dir("run", 0o755)?;
    writer.dir("sys", 0o755)?;
    writer.file("init", init, 0o755)?;
    writer.finish()
}

fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "xtask manifest directory has no parent".to_string())
}

fn target_dir(workspace_root: &Path) -> PathBuf {
    match env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        }
        None => workspace_root.join("target"),
    }
}

struct NewcWriter {
    file: File,
    offset: u64,
    ino: u32,
}

impl NewcWriter {
    fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).map_err(|err| format!("create {}: {err}", path.display()))?;
        Ok(Self {
            file,
            offset: 0,
            ino: 1,
        })
    }

    fn dir(&mut self, name: &str, mode: u32) -> Result<()> {
        self.entry(name, 0o040000 | mode, 2, 0, 0, &[])
    }

    fn file(&mut self, name: &str, path: &Path, mode: u32) -> Result<()> {
        let contents = fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        self.entry(name, 0o100000 | mode, 1, 0, 0, &contents)
    }

    fn char_dev(&mut self, name: &str, mode: u32, major: u32, minor: u32) -> Result<()> {
        self.entry(name, 0o020000 | mode, 1, major, minor, &[])
    }

    fn finish(mut self) -> Result<()> {
        self.entry("TRAILER!!!", 0, 1, 0, 0, &[])?;
        self.file
            .flush()
            .map_err(|err| format!("flush initrd: {err}"))
    }

    fn entry(
        &mut self,
        name: &str,
        mode: u32,
        nlink: u32,
        rdevmajor: u32,
        rdevminor: u32,
        contents: &[u8],
    ) -> Result<()> {
        if name.starts_with('/') {
            return Err(format!("cpio entry must be relative: {name}"));
        }

        let namesize = name.len() + 1;
        let filesize =
            u32::try_from(contents.len()).map_err(|_| format!("cpio entry too large: {name}"))?;
        let header = format!(
            "070701{ino:08x}{mode:08x}{uid:08x}{gid:08x}{nlink:08x}{mtime:08x}{filesize:08x}{devmajor:08x}{devminor:08x}{rdevmajor:08x}{rdevminor:08x}{namesize:08x}{check:08x}",
            ino = self.ino,
            mode = mode,
            uid = 0,
            gid = 0,
            nlink = nlink,
            mtime = source_date_epoch(),
            filesize = filesize,
            devmajor = 0,
            devminor = 0,
            rdevmajor = rdevmajor,
            rdevminor = rdevminor,
            namesize = namesize,
            check = 0,
        );

        self.write_all(header.as_bytes())?;
        self.write_all(name.as_bytes())?;
        self.write_all(&[0])?;
        self.pad_to_4()?;
        self.write_all(contents)?;
        self.pad_to_4()?;
        self.ino = self.ino.wrapping_add(1);
        Ok(())
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.file
            .write_all(bytes)
            .map_err(|err| format!("write initrd: {err}"))?;
        self.offset += bytes.len() as u64;
        Ok(())
    }

    fn pad_to_4(&mut self) -> Result<()> {
        let pad = (4 - (self.offset % 4)) % 4;
        if pad != 0 {
            self.write_all(&vec![0; pad as usize])?;
        }
        Ok(())
    }
}

fn source_date_epoch() -> u32 {
    env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
}

fn print_usage() {
    println!(
        "usage: cargo xtask <command>\n\ncommands:\n  cpio    build pocketboot and create an initrd cpio"
    );
}

fn print_cpio_usage() {
    println!(
        "usage: cargo xtask cpio [--target TRIPLE] [--output PATH]\n\ndefault target: {DEFAULT_TARGET}\ndefault output: target/{DEFAULT_INITRD}"
    );
}
