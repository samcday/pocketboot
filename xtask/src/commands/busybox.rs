use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs::{self, File},
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;

use super::{
    FeatureSet, ensure_file, feature_set, parallel_jobs, run_command, target_dir, workspace_root,
};

pub(super) const BUSYBOX_VERSION: &str = "1.38.0";
const BUSYBOX_RECIPE_VERSION: u32 = 1;
const BUSYBOX_ARCHIVE_SHA256: &str =
    "34f9ea6ff8636f2c9241153b9114eefa9e65674a45318ae1ef95bb5f31c53bb2";
const BUSYBOX_SOURCE_URL: &str = "https://busybox.net/downloads/busybox-1.38.0.tar.bz2";

#[derive(clap::Args, Debug)]
pub(crate) struct BusyBoxArgs {
    #[arg(long, default_value_t = super::cpio::DEFAULT_TARGET.to_string())]
    target: String,
    #[arg(long, value_name = "FEATURES")]
    features: Vec<String>,
}

#[derive(Debug)]
struct BusyBoxPaths {
    archive: PathBuf,
    source_parent: PathBuf,
    source: PathBuf,
    build: PathBuf,
    install: PathBuf,
    installed_binary: PathBuf,
    stamp: PathBuf,
}

#[derive(Debug)]
pub(super) struct BusyBoxInstall {
    pub(super) root: PathBuf,
    pub(super) binary: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct BusyBoxStamp {
    input: StampInput,
    output: StampOutput,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct StampInput {
    recipe_version: u32,
    busybox_version: String,
    archive_sha256: String,
    source_url: String,
    target: String,
    features: Vec<String>,
    env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct StampOutput {
    tree_sha256: String,
    binary_sha256: String,
    binary_len: u64,
}

impl BusyBoxPaths {
    fn new(target_dir: &Path, target: &str, features: &FeatureSet) -> Self {
        let root = target_dir.join("busybox");
        let variant = feature_variant(features);
        let cache_name = format!("{target}-{variant}");
        let install = root.join(format!("install-{cache_name}"));

        Self {
            archive: root.join(format!("busybox-{BUSYBOX_VERSION}.tar.bz2")),
            source_parent: root.join("src"),
            source: root.join("src").join(format!("busybox-{BUSYBOX_VERSION}")),
            build: root.join(format!("build-{cache_name}")),
            installed_binary: install.join("bin/busybox"),
            install,
            stamp: root.join("stamps").join(format!("{cache_name}.toml")),
        }
    }
}

pub(crate) fn run(args: BusyBoxArgs) -> Result<()> {
    busybox(args)
}

fn busybox(args: BusyBoxArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let target_dir = target_dir(&workspace_root);
    let features = feature_set(&args.features)?;
    let install = build(&target_dir, &args.target, &features)?;
    println!("install {}", install.root.display());
    println!(
        "busybox {} ({} bytes)",
        install.binary.display(),
        fs::metadata(&install.binary)
            .map_err(|err| format!("stat {}: {err}", install.binary.display()))?
            .len()
    );
    Ok(())
}

pub(super) fn build(
    target_dir: &Path,
    target: &str,
    features: &FeatureSet,
) -> Result<BusyBoxInstall> {
    let paths = BusyBoxPaths::new(target_dir, target, features);
    let input = stamp_input(target, features);

    if cache_hit(&paths, &input)? {
        return Ok(BusyBoxInstall {
            root: paths.install,
            binary: paths.installed_binary,
        });
    }

    ensure_busybox_archive(&paths.archive)?;
    ensure_busybox_source(&paths.archive, &paths.source_parent, &paths.source)?;

    fs::create_dir_all(&paths.build)
        .map_err(|err| format!("create {}: {err}", paths.build.display()))?;
    ensure_busybox_toolchain(&paths.build, target)?;
    run_busybox_make_quiet(
        &paths.source,
        &paths.build,
        target,
        &["allnoconfig"],
        "make busybox allnoconfig",
    )?;
    configure_busybox(&paths.build.join(".config"), features)?;
    run_busybox_oldconfig(&paths.source, &paths.build, target)?;
    run_busybox_make(
        &paths.source,
        &paths.build,
        target,
        &[&format!("-j{}", parallel_jobs()), "busybox"],
    )?;

    let binary = paths.build.join("busybox");
    ensure_file(&binary, "busybox binary")?;
    strip_busybox(&binary, target)?;

    if paths.install.exists() {
        fs::remove_dir_all(&paths.install)
            .map_err(|err| format!("remove {}: {err}", paths.install.display()))?;
    }
    fs::create_dir_all(&paths.install)
        .map_err(|err| format!("create {}: {err}", paths.install.display()))?;
    run_busybox_make(
        &paths.source,
        &paths.build,
        target,
        &[
            &format!("CONFIG_PREFIX={}", paths.install.display()),
            "install",
        ],
    )?;

    ensure_file(&paths.installed_binary, "installed busybox binary")?;
    write_stamp(
        &paths.stamp,
        &input,
        &paths.install,
        &paths.installed_binary,
    )?;

    Ok(BusyBoxInstall {
        root: paths.install,
        binary: paths.installed_binary,
    })
}

fn cache_hit(paths: &BusyBoxPaths, input: &StampInput) -> Result<bool> {
    let contents = match fs::read_to_string(&paths.stamp) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(format!("read {}: {err}", paths.stamp.display())),
    };
    let Ok(stamp) = toml::from_str::<BusyBoxStamp>(&contents) else {
        return Ok(false);
    };
    if stamp.input != *input || !paths.install.is_dir() || !paths.installed_binary.is_file() {
        return Ok(false);
    }

    let Ok(output) = output_stamp(&paths.install, &paths.installed_binary) else {
        return Ok(false);
    };
    Ok(stamp.output == output)
}

fn write_stamp(path: &Path, input: &StampInput, install: &Path, binary: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    let stamp = BusyBoxStamp {
        input: input.clone(),
        output: output_stamp(install, binary)?,
    };
    let contents =
        toml::to_string_pretty(&stamp).map_err(|err| format!("encode busybox stamp: {err}"))?;
    fs::write(path, contents).map_err(|err| format!("write {}: {err}", path.display()))
}

fn stamp_input(target: &str, features: &FeatureSet) -> StampInput {
    StampInput {
        recipe_version: BUSYBOX_RECIPE_VERSION,
        busybox_version: BUSYBOX_VERSION.to_string(),
        archive_sha256: BUSYBOX_ARCHIVE_SHA256.to_string(),
        source_url: BUSYBOX_SOURCE_URL.to_string(),
        target: target.to_string(),
        features: feature_values(features),
        env: stamp_env(),
    }
}

fn stamp_env() -> BTreeMap<String, String> {
    [
        "MAKE",
        "BUSYBOX_CC",
        "BUSYBOX_CFLAGS",
        "BUSYBOX_CROSS_COMPILE",
        "BUSYBOX_STRIP",
        "CROSS_COMPILE",
    ]
    .into_iter()
    .filter_map(|name| {
        env::var_os(name).map(|value| (name.to_string(), value.to_string_lossy().into_owned()))
    })
    .collect()
}

fn output_stamp(install: &Path, binary: &Path) -> Result<StampOutput> {
    let binary_len = fs::metadata(binary)
        .map_err(|err| format!("stat {}: {err}", binary.display()))?
        .len();
    Ok(StampOutput {
        tree_sha256: hash_install_tree(install)?,
        binary_sha256: hash_file(binary)?,
        binary_len,
    })
}

fn feature_variant(features: &FeatureSet) -> String {
    let values = feature_values(features);
    let label = if values.is_empty() {
        "default".to_string()
    } else {
        values.join("+")
    };
    let label = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    let mut hasher = Sha256::new();
    if values.is_empty() {
        update_hash_field(&mut hasher, b"default");
    } else {
        for value in values {
            update_hash_field(&mut hasher, value.as_bytes());
        }
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("{label}-{}", &digest[..12])
}

fn feature_values(features: &FeatureSet) -> Vec<String> {
    let mut values = features.values().to_vec();
    values.sort();
    values
}

fn hash_install_tree(root: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_tree_dir(root, root, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_tree_dir(root: &Path, dir: &Path, hasher: &mut Sha256) -> Result<()> {
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
        let metadata =
            fs::symlink_metadata(&path).map_err(|err| format!("stat {}: {err}", path.display()))?;
        let mode = metadata.permissions().mode() & 0o7777;
        let file_type = metadata.file_type();

        update_hash_field(hasher, relative.as_os_str().as_bytes());
        hasher.update(mode.to_le_bytes());
        if file_type.is_dir() {
            hasher.update(b"dir");
            hash_tree_dir(root, &path, hasher)?;
        } else if file_type.is_symlink() {
            hasher.update(b"symlink");
            let target = fs::read_link(&path)
                .map_err(|err| format!("readlink {}: {err}", path.display()))?;
            update_hash_field(hasher, target.as_os_str().as_bytes());
        } else if file_type.is_file() {
            hasher.update(b"file");
            hash_file_into(&path, hasher)?;
        } else {
            return Err(format!(
                "unsupported file type in busybox install tree: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_file_into(path, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_file_into(path: &Path, hasher: &mut Sha256) -> Result<()> {
    let mut file = File::open(path).map_err(|err| format!("open {}: {err}", path.display()))?;
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
    Ok(())
}

fn update_hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
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
    let digest = hash_file(path)?;
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

fn configure_busybox(config: &Path, features: &FeatureSet) -> Result<()> {
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
    if features.contains("qemu") {
        for name in ["IFCONFIG", "FEATURE_IFCONFIG_STATUS"] {
            set_kconfig_bool(&mut contents, name, true);
        }
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
    let mut command = busybox_make_command(source, build, target);
    command.stdin(Stdio::null());
    command.args(args);
    run_command(command, "make busybox")
}

fn run_busybox_make_quiet(
    source: &Path,
    build: &Path,
    target: &str,
    args: &[&str],
    action: &str,
) -> Result<()> {
    let mut command = busybox_make_command(source, build, target);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    let output = command
        .output()
        .map_err(|err| format!("spawn {action}: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_output_error(action, &output))
    }
}

fn run_busybox_oldconfig(source: &Path, build: &Path, target: &str) -> Result<()> {
    let mut command = busybox_make_command(source, build, target);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("oldconfig");

    let mut child = command
        .spawn()
        .map_err(|err| format!("spawn make busybox oldconfig: {err}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(err) = stdin.write_all(&[b'\n'; 4096]) {
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(format!("write busybox oldconfig defaults: {err}"));
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|err| format!("wait for busybox oldconfig: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_output_error("make busybox oldconfig", &output))
    }
}

fn command_output_error(action: &str, output: &std::process::Output) -> String {
    format!(
        "{action} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn busybox_make_command(source: &Path, build: &Path, target: &str) -> Command {
    let mut command = Command::new(env::var_os("MAKE").unwrap_or_else(|| "make".into()));
    command
        .current_dir(source)
        .env("CROSS_COMPILE", busybox_cross_compile(target))
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
    command
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
