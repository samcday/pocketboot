use std::{
    env,
    fs::{self, File},
    io::Write,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use crate::Result;

use super::{
    FeatureSet,
    busybox::{BusyBoxInstall, build as build_busybox},
    feature_set, target_dir, workspace_root,
};

pub(super) const DEFAULT_TARGET: &str = "aarch64-unknown-linux-musl";
pub(super) const DEFAULT_INITRD: &str = "pocketboot-initrd.cpio";

const INIT_BINARY: &str = "pocketboot";

#[derive(clap::Args, Debug)]
pub(crate) struct CpioArgs {
    #[arg(long, default_value_t = DEFAULT_TARGET.to_string())]
    target: String,
    #[arg(short, long, value_name = "PATH", conflicts_with = "positional_output")]
    output: Option<PathBuf>,
    #[arg(long = "no-busybox", action = clap::ArgAction::SetFalse, default_value_t = true)]
    busybox: bool,
    #[arg(long, value_name = "FEATURES")]
    features: Vec<String>,
    #[arg(value_name = "OUTPUT")]
    positional_output: Option<PathBuf>,
}

pub(crate) fn run(args: CpioArgs) -> Result<()> {
    cpio(args)
}

fn cpio(args: CpioArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let mut features = feature_set(&args.features)?;
    if args.busybox {
        features.add("busybox")?;
    }
    let output = args.output.or(args.positional_output);
    let output = build_initrd(
        &workspace_root,
        &args.target,
        output,
        args.busybox,
        &features,
    )?;
    println!("wrote {}", output.display());
    Ok(())
}

pub(super) fn build_initrd(
    workspace_root: &Path,
    target: &str,
    output: Option<PathBuf>,
    include_busybox: bool,
    features: &FeatureSet,
) -> Result<PathBuf> {
    build_release(workspace_root, target, features)?;

    let target_dir = target_dir(workspace_root);
    let init = target_dir.join(target).join("release").join(INIT_BINARY);
    if !init.is_file() {
        return Err(format!("release binary not found: {}", init.display()));
    }

    let output = output.unwrap_or_else(|| target_dir.join(DEFAULT_INITRD));
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }

    let busybox = include_busybox
        .then(|| build_busybox(&target_dir, target, features))
        .transpose()?;
    write_initrd(&init, busybox.as_ref(), &output)?;
    Ok(output)
}

fn build_release(workspace_root: &Path, target: &str, features: &FeatureSet) -> Result<()> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(workspace_root).args([
        "build",
        "--release",
        "--target",
        target,
        "-p",
        INIT_BINARY,
    ]);
    if !features.is_empty() {
        command.arg("--features").arg(features.cargo_value());
    }

    let status = command
        .status()
        .map_err(|err| format!("spawn cargo build: {err}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with {status}"));
    }
    Ok(())
}

fn write_initrd(init: &Path, busybox: Option<&BusyBoxInstall>, output: &Path) -> Result<()> {
    let tmp = output.with_extension("cpio.tmp");
    let mut writer = NewcWriter::create(&tmp)?;
    writer.dir("dev", 0o755)?;
    writer.char_dev("dev/console", 0o600, 5, 1)?;
    writer.char_dev("dev/kmsg", 0o600, 1, 11)?;
    writer.char_dev("dev/null", 0o666, 1, 3)?;
    writer.dir("etc", 0o755)?;
    writer.dir("proc", 0o755)?;
    writer.dir("run", 0o755)?;
    writer.dir("sys", 0o755)?;
    writer.dir("tmp", 0o1777)?;
    if let Some(busybox) = busybox {
        writer.tree(&busybox.root)?;
        println!(
            "busybox {} ({} bytes)",
            busybox.binary.display(),
            fs::metadata(&busybox.binary)
                .map_err(|err| format!("stat {}: {err}", busybox.binary.display()))?
                .len()
        );
    }
    writer.file("init", init, 0o755)?;
    writer.finish()?;

    if same_contents(&tmp, output)? {
        fs::remove_file(&tmp).map_err(|err| format!("remove {}: {err}", tmp.display()))?;
    } else {
        fs::rename(&tmp, output)
            .map_err(|err| format!("rename {} to {}: {err}", tmp.display(), output.display()))?;
    }
    Ok(())
}

fn same_contents(left: &Path, right: &Path) -> Result<bool> {
    let left = fs::read(left).map_err(|err| format!("read {}: {err}", left.display()))?;
    let right = match fs::read(right) {
        Ok(right) => right,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(format!("read {}: {err}", right.display())),
    };
    Ok(left == right)
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

    fn file_with_mode(&mut self, name: &str, path: &Path, mode: u32) -> Result<()> {
        let contents = fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        self.entry(name, 0o100000 | mode, 1, 0, 0, &contents)
    }

    fn symlink(&mut self, name: &str, target: &Path) -> Result<()> {
        self.entry(
            name,
            0o120000 | 0o777,
            1,
            0,
            0,
            target.as_os_str().as_bytes(),
        )
    }

    fn char_dev(&mut self, name: &str, mode: u32, major: u32, minor: u32) -> Result<()> {
        self.entry(name, 0o020000 | mode, 1, major, minor, &[])
    }

    fn tree(&mut self, root: &Path) -> Result<()> {
        self.tree_dir(root, root)
    }

    fn tree_dir(&mut self, root: &Path, dir: &Path) -> Result<()> {
        let mut entries = fs::read_dir(dir)
            .map_err(|err| format!("read directory {}: {err}", dir.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| format!("read directory entry under {}: {err}", dir.display()))?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|err| {
                format!(
                    "strip {} prefix from {}: {err}",
                    root.display(),
                    path.display()
                )
            })?;
            let name = cpio_name(relative)?;
            let metadata = fs::symlink_metadata(&path)
                .map_err(|err| format!("stat {}: {err}", path.display()))?;
            let mode = metadata.permissions().mode() & 0o7777;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                self.dir(&name, mode)?;
                self.tree_dir(root, &path)?;
            } else if file_type.is_symlink() {
                let target = fs::read_link(&path)
                    .map_err(|err| format!("readlink {}: {err}", path.display()))?;
                self.symlink(&name, &target)?;
            } else if file_type.is_file() {
                self.file_with_mode(&name, &path, mode)?;
            } else {
                return Err(format!(
                    "unsupported file type in initrd tree: {}",
                    path.display()
                ));
            }
        }
        Ok(())
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

fn cpio_name(path: &Path) -> Result<String> {
    let name = path
        .to_str()
        .ok_or_else(|| format!("cpio path is not valid UTF-8: {}", path.display()))?;
    if name.is_empty()
        || name.starts_with('/')
        || name.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        Err(format!("invalid cpio path: {name}"))
    } else {
        Ok(name.to_string())
    }
}
