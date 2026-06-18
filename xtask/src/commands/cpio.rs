use std::{
    env,
    ffi::OsString,
    fs::{self, File},
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use sha2::{Digest, Sha256};

use crate::Result;

use super::{ensure_file, parallel_jobs, run_command, target_dir, workspace_root};

pub(super) const DEFAULT_TARGET: &str = "aarch64-unknown-linux-musl";
pub(super) const DEFAULT_INITRD: &str = "pocketboot-initrd.cpio";

const INIT_BINARY: &str = "pocketboot";
const BUSYBOX_VERSION: &str = "1.38.0";
const BUSYBOX_ARCHIVE_SHA256: &str =
    "34f9ea6ff8636f2c9241153b9114eefa9e65674a45318ae1ef95bb5f31c53bb2";
const BUSYBOX_SOURCE_URL: &str = "https://busybox.net/downloads/busybox-1.38.0.tar.bz2";

#[derive(Debug)]
struct CpioArgs {
    target: String,
    output: Option<PathBuf>,
    busybox: bool,
}

impl CpioArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut target = DEFAULT_TARGET.to_string();
        let mut output = None;
        let mut busybox = true;
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
                "--no-busybox" => busybox = false,
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

        Ok(Self {
            target,
            output,
            busybox,
        })
    }
}

pub(crate) fn run(args: Vec<String>) -> Result<()> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print_usage();
        Ok(())
    } else {
        cpio(CpioArgs::parse(args)?)
    }
}

fn cpio(args: CpioArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let output = build_initrd(&workspace_root, &args.target, args.output, args.busybox)?;
    println!("wrote {}", output.display());
    Ok(())
}

pub(super) fn build_initrd(
    workspace_root: &Path,
    target: &str,
    output: Option<PathBuf>,
    include_busybox: bool,
) -> Result<PathBuf> {
    build_release(workspace_root, target)?;

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
        .then(|| build_busybox(&target_dir, target))
        .transpose()?;
    write_initrd(&init, busybox.as_ref(), &output)?;
    Ok(output)
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

struct BusyBoxInstall {
    root: PathBuf,
    binary: PathBuf,
}

fn build_busybox(target_dir: &Path, target: &str) -> Result<BusyBoxInstall> {
    let root = target_dir.join("busybox");
    let archive = root.join(format!("busybox-{BUSYBOX_VERSION}.tar.bz2"));
    let source_parent = root.join("src");
    let source = source_parent.join(format!("busybox-{BUSYBOX_VERSION}"));
    let build = root.join(format!("build-{target}"));
    let install = root.join(format!("install-{target}"));

    ensure_busybox_archive(&archive)?;
    ensure_busybox_source(&archive, &source_parent, &source)?;

    fs::create_dir_all(&build).map_err(|err| format!("create {}: {err}", build.display()))?;
    ensure_busybox_toolchain(&build, target)?;
    run_busybox_make(&source, &build, target, &["allnoconfig"])?;
    configure_busybox(&build.join(".config"))?;
    run_busybox_make(&source, &build, target, &["oldconfig"])?;
    run_busybox_make(
        &source,
        &build,
        target,
        &[&format!("-j{}", parallel_jobs()), "busybox"],
    )?;

    let binary = build.join("busybox");
    ensure_file(&binary, "busybox binary")?;
    strip_busybox(&binary, target)?;

    if install.exists() {
        fs::remove_dir_all(&install)
            .map_err(|err| format!("remove {}: {err}", install.display()))?;
    }
    fs::create_dir_all(&install).map_err(|err| format!("create {}: {err}", install.display()))?;
    run_busybox_make(
        &source,
        &build,
        target,
        &[&format!("CONFIG_PREFIX={}", install.display()), "install"],
    )?;

    let installed_binary = install.join("bin/busybox");
    ensure_file(&installed_binary, "installed busybox binary")?;
    Ok(BusyBoxInstall {
        root: install,
        binary: installed_binary,
    })
}

fn ensure_busybox_archive(archive: &Path) -> Result<()> {
    if archive.is_file() {
        verify_busybox_archive(archive)?;
        return Ok(());
    }

    if let Some(parent) = archive
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }

    let tmp = archive.with_extension("tar.bz2.tmp");
    let curl = env::var_os("CURL").unwrap_or_else(|| "curl".into());
    let mut command = Command::new(curl);
    command
        .args(["--fail", "--location", "--output"])
        .arg(&tmp)
        .arg(BUSYBOX_SOURCE_URL);
    run_command(command, "download busybox")?;
    verify_busybox_archive(&tmp)?;
    fs::rename(&tmp, archive)
        .map_err(|err| format!("rename {} to {}: {err}", tmp.display(), archive.display()))
}

fn verify_busybox_archive(path: &Path) -> Result<()> {
    let mut file = File::open(path).map_err(|err| format!("open {}: {err}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("read {}: {err}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let digest = format!("{:x}", hasher.finalize());
    if digest == BUSYBOX_ARCHIVE_SHA256 {
        Ok(())
    } else {
        Err(format!(
            "busybox archive checksum mismatch for {}: expected {BUSYBOX_ARCHIVE_SHA256}, got {digest}",
            path.display()
        ))
    }
}

fn ensure_busybox_source(archive: &Path, source_parent: &Path, source: &Path) -> Result<()> {
    if source.is_dir() {
        return Ok(());
    }

    fs::create_dir_all(source_parent)
        .map_err(|err| format!("create {}: {err}", source_parent.display()))?;
    let mut command = Command::new("tar");
    command
        .arg("-xjf")
        .arg(archive)
        .arg("-C")
        .arg(source_parent);
    run_command(command, "extract busybox")?;
    if source.is_dir() {
        Ok(())
    } else {
        Err(format!(
            "busybox source was not extracted: {}",
            source.display()
        ))
    }
}

fn configure_busybox(config: &Path) -> Result<()> {
    const ENABLED: &[&str] = &[
        "STATIC",
        "FEATURE_INSTALLER",
        "INSTALL_APPLET_SYMLINKS",
        "ASH",
        "SH_IS_ASH",
        "ASH_OPTIMIZE_FOR_SIZE",
        "ASH_BASH_COMPAT",
        "ASH_JOB_CONTROL",
        "ASH_ALIAS",
        "ASH_EXPAND_PRMT",
        "ASH_ECHO",
        "ASH_PRINTF",
        "ASH_TEST",
        "ASH_GETOPTS",
        "ASH_CMDCMD",
        "CTTYHACK",
        "FEATURE_SH_MATH",
        "FEATURE_SH_STANDALONE",
        "FEATURE_SH_NOFORK",
        "FEATURE_EDITING",
        "FEATURE_EDITING_WINCH",
        "FEATURE_EDITING_FANCY_PROMPT",
        "FEATURE_PREFER_APPLETS",
        "BUSYBOX",
        "CAT",
        "CHMOD",
        "CHOWN",
        "CP",
        "CUT",
        "DATE",
        "DD",
        "DF",
        "DIRNAME",
        "DMESG",
        "DU",
        "ECHO",
        "ENV",
        "FALSE",
        "FIND",
        "FREE",
        "GREP",
        "EGREP",
        "FGREP",
        "HEAD",
        "HEXDUMP",
        "ID",
        "KILL",
        "LN",
        "LOSETUP",
        "LS",
        "MKDIR",
        "MKNOD",
        "MKTEMP",
        "MOUNT",
        "MOUNTPOINT",
        "MV",
        "PIDOF",
        "PRINTENV",
        "PRINTF",
        "PS",
        "PWD",
        "READLINK",
        "REALPATH",
        "REBOOT",
        "RESET",
        "RM",
        "RMDIR",
        "SED",
        "SETSID",
        "SLEEP",
        "SORT",
        "STAT",
        "STTY",
        "STRINGS",
        "SYNC",
        "TAIL",
        "TAR",
        "TEE",
        "TEST",
        "TEST1",
        "TEST2",
        "TOUCH",
        "TR",
        "TRUE",
        "UMOUNT",
        "UNAME",
        "UNIQ",
        "WC",
        "WHOAMI",
        "XARGS",
        "BLKDISCARD",
        "BLKID",
        "BLOCKDEV",
        "FDISK",
        "FEATURE_FDISK_WRITABLE",
        "FEATURE_GPT_LABEL",
        "FSTRIM",
        "LSBLK",
        "MKDOSFS",
        "MKFS_VFAT",
        "MKSWAP",
        "SWAPON",
        "SWAPOFF",
        "FEATURE_MOUNT_FLAGS",
        "FEATURE_MOUNT_FSTAB",
        "FEATURE_MOUNT_LABEL",
        "FEATURE_MOUNT_LOOP",
        "FEATURE_MOUNT_LOOP_CREATE",
        "FEATURE_VOLUMEID_EXT",
        "FEATURE_VOLUMEID_FAT",
        "FEATURE_VOLUMEID_F2FS",
        "FEATURE_VOLUMEID_LINUXSWAP",
        "FEATURE_VOLUMEID_SQUASHFS",
    ];
    const DISABLED: &[&str] = &["HUSH", "SH_IS_HUSH", "SH_IS_NONE", "BASH_IS_ASH"];

    let mut contents = fs::read_to_string(config)
        .map_err(|err| format!("read busybox config {}: {err}", config.display()))?;
    for name in ENABLED {
        set_kconfig_bool(&mut contents, name, true);
    }
    for name in DISABLED {
        set_kconfig_bool(&mut contents, name, false);
    }
    set_kconfig_string(&mut contents, "BUSYBOX_EXEC_PATH", "/bin/busybox");
    set_kconfig_string(&mut contents, "PREFIX", "./_install");
    set_kconfig_string(&mut contents, "CROSS_COMPILER_PREFIX", "");
    set_kconfig_int(&mut contents, "FEATURE_EDITING_MAX_LEN", 1024);
    set_kconfig_int(&mut contents, "FEATURE_EDITING_HISTORY", 64);
    fs::write(config, contents)
        .map_err(|err| format!("write busybox config {}: {err}", config.display()))
}

fn set_kconfig_bool(contents: &mut String, name: &str, enabled: bool) {
    let enabled_line = format!("CONFIG_{name}=y");
    let disabled_line = format!("# CONFIG_{name} is not set");
    let mut found = false;
    let mut lines = Vec::new();
    for line in contents.lines() {
        if line == enabled_line || line == disabled_line {
            found = true;
            lines.push(if enabled {
                enabled_line.clone()
            } else {
                disabled_line.clone()
            });
        } else {
            lines.push(line.to_string());
        }
    }
    if !found {
        lines.push(if enabled { enabled_line } else { disabled_line });
    }
    *contents = lines.join("\n");
    contents.push('\n');
}

fn set_kconfig_string(contents: &mut String, name: &str, value: &str) {
    let prefix = format!("CONFIG_{name}=");
    let line = format!("CONFIG_{name}=\"{value}\"");
    let mut found = false;
    let mut lines = Vec::new();
    for existing in contents.lines() {
        if existing.starts_with(&prefix) || existing == format!("# CONFIG_{name} is not set") {
            found = true;
            lines.push(line.clone());
        } else {
            lines.push(existing.to_string());
        }
    }
    if !found {
        lines.push(line);
    }
    *contents = lines.join("\n");
    contents.push('\n');
}

fn set_kconfig_int(contents: &mut String, name: &str, value: u32) {
    let prefix = format!("CONFIG_{name}=");
    let line = format!("CONFIG_{name}={value}");
    let mut found = false;
    let mut lines = Vec::new();
    for existing in contents.lines() {
        if existing.starts_with(&prefix) || existing == format!("# CONFIG_{name} is not set") {
            found = true;
            lines.push(line.clone());
        } else {
            lines.push(existing.to_string());
        }
    }
    if !found {
        lines.push(line);
    }
    *contents = lines.join("\n");
    contents.push('\n');
}

fn run_busybox_make(source: &Path, build: &Path, target: &str, args: &[&str]) -> Result<()> {
    let mut command = Command::new(env::var_os("MAKE").unwrap_or_else(|| "make".into()));
    command
        .current_dir(source)
        .env("CROSS_COMPILE", busybox_cross_compile(target))
        .stdin(Stdio::null())
        .arg(format!("O={}", build.display()))
        .arg({
            let mut value = OsString::from("CC=");
            value.push(busybox_cc(target));
            value
        });
    if let Some(cflags) = busybox_cflags() {
        command.arg({
            let mut value = OsString::from("EXTRA_CFLAGS=");
            value.push(cflags);
            value
        });
    }
    command.args(args);
    run_command(command, "make busybox")
}

fn ensure_busybox_toolchain(build: &Path, target: &str) -> Result<()> {
    let source = build.join("toolchain-check.c");
    let output = build.join("toolchain-check");
    fs::write(
        &source,
        "#include <byteswap.h>\n#include <linux/fs.h>\n#include <linux/version.h>\n#include <sys/types.h>\nint main(void) { return bswap_32(0x12345678) == 0 ? 1 : 0; }\n",
    )
    .map_err(|err| format!("write {}: {err}", source.display()))?;

    let compiler = busybox_cc(target);
    let cflags =
        busybox_cflag_args().map_err(|err| busybox_toolchain_error(target, &compiler, err))?;
    let mut command = Command::new(&compiler);
    command
        .arg("-Os")
        .arg("-static")
        .args(cflags)
        .arg(&source)
        .arg("-o")
        .arg(&output);

    let status = command.status().map_err(|err| {
        busybox_toolchain_error(target, &compiler, format!("run compiler: {err}"))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(busybox_toolchain_error(
            target,
            &compiler,
            format!("compiler check failed with {status}"),
        ))
    }
}

fn busybox_toolchain_error(target: &str, compiler: &OsString, reason: impl AsRef<str>) -> String {
    format!(
        "BusyBox requires a static libc-capable target C compiler for {target}; \
         tried {}; set BUSYBOX_CC, BUSYBOX_CFLAGS, or BUSYBOX_CROSS_COMPILE, install a \
         matching musl toolchain, or pass --no-busybox ({})",
        compiler.to_string_lossy(),
        reason.as_ref()
    )
}

fn busybox_cflags() -> Option<OsString> {
    env::var_os("BUSYBOX_CFLAGS").filter(|value| !value.as_os_str().is_empty())
}

fn busybox_cflag_args() -> Result<Vec<OsString>> {
    let Some(cflags) = busybox_cflags() else {
        return Ok(Vec::new());
    };
    let cflags = cflags
        .into_string()
        .map_err(|_| "BUSYBOX_CFLAGS must be valid UTF-8".to_string())?;
    Ok(cflags.split_whitespace().map(OsString::from).collect())
}

fn busybox_cc(target: &str) -> OsString {
    if let Some(value) = env::var_os("BUSYBOX_CC") {
        return value;
    }
    let mut value = busybox_cross_compile(target);
    value.push("gcc");
    value
}

fn busybox_cross_compile(target: &str) -> OsString {
    if let Some(value) = env::var_os("BUSYBOX_CROSS_COMPILE") {
        return value;
    }
    match target {
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl-".into(),
        _ => env::var_os("CROSS_COMPILE").unwrap_or_default(),
    }
}

fn strip_busybox(binary: &Path, target: &str) -> Result<()> {
    let strip = if let Some(strip) = env::var_os("BUSYBOX_STRIP") {
        strip
    } else {
        let mut value = busybox_cross_compile(target);
        value.push("strip");
        value
    };
    let mut command = Command::new(strip);
    command.arg("--strip-all").arg(binary);
    run_command(command, "strip busybox")
}

fn write_initrd(init: &Path, busybox: Option<&BusyBoxInstall>, output: &Path) -> Result<()> {
    let mut writer = NewcWriter::create(output)?;
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
    writer.finish()
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

fn print_usage() {
    println!(
        "usage: cargo xtask cpio [--target TRIPLE] [--output PATH] [--no-busybox]\n\ndefault target: {DEFAULT_TARGET}\ndefault output: target/{DEFAULT_INITRD}\nbusybox: official {BUSYBOX_VERSION} release, built statically unless --no-busybox is used"
    );
}
