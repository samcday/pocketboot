pub(crate) mod bootimg;
pub(crate) mod build;
pub(crate) mod busybox;
pub(crate) mod ci_matrix;
mod config;
pub(crate) mod cpio;
pub(crate) mod kernel;
pub(crate) mod kernel_src;
pub(crate) mod preboot;
pub(crate) mod qemu;

use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, Permissions},
    path::{Path, PathBuf},
    process::Command,
    thread,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::Result;

const DEFAULT_KERNEL_ARCH: &str = "arm64";

#[derive(Clone, Debug, Default)]
pub(super) struct FeatureSet {
    values: Vec<String>,
}

impl FeatureSet {
    pub(super) fn add(&mut self, value: &str) -> Result<()> {
        for feature in value.split(|ch: char| ch == ',' || ch.is_ascii_whitespace()) {
            if feature.is_empty() {
                continue;
            }
            validate_feature(feature)?;
            if !self.values.iter().any(|existing| existing == feature) {
                self.values.push(feature.to_string());
            }
        }
        Ok(())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub(super) fn contains(&self, feature: &str) -> bool {
        self.values.iter().any(|value| value == feature)
    }

    pub(super) fn cargo_value(&self) -> String {
        self.values.join(",")
    }

    pub(super) fn values(&self) -> &[String] {
        &self.values
    }
}

fn feature_set(values: &[String]) -> Result<FeatureSet> {
    let mut features = FeatureSet::default();
    for value in values {
        features.add(value)?;
    }
    Ok(features)
}

fn validate_feature(feature: &str) -> Result<()> {
    if feature
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '/'))
    {
        Ok(())
    } else {
        Err(format!("invalid feature name: {feature}"))
    }
}

#[derive(Clone, Debug)]
struct KernelDevice {
    vendor: String,
    stem: String,
    soc: String,
}

impl KernelDevice {
    fn parse(value: &str) -> Result<Self> {
        let parts = value.split('/').collect::<Vec<_>>();
        if parts.len() != 2 {
            return Err(format!(
                "device ID must be a canonical DTB path without suffix, e.g. qcom/msm8916-samsung-a5u-eur: {value}"
            ));
        }

        let vendor = parts[0];
        let stem = parts[1];
        validate_device_component("vendor", vendor)?;
        validate_device_component("device", stem)?;
        if stem.ends_with(".dts") || stem.ends_with(".dtb") {
            return Err(format!("device ID must omit .dts/.dtb suffix: {value}"));
        }

        let soc = stem.split_once('-').map_or(stem, |(soc, _)| soc);
        Ok(Self {
            vendor: vendor.to_string(),
            stem: stem.to_string(),
            soc: soc.to_string(),
        })
    }

    fn id(&self) -> String {
        format!("{}/{}", self.vendor, self.stem)
    }
}

impl std::str::FromStr for KernelDevice {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(value)
    }
}

fn validate_device_component(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || matches!(value, "." | "..") {
        return Err(format!("invalid {kind} component in device ID: {value}"));
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Ok(())
    } else {
        Err(format!("invalid {kind} component in device ID: {value}"))
    }
}

fn kernel_tree(path: &Path) -> Result<PathBuf> {
    let path =
        fs::canonicalize(path).map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    ensure_file(&path.join("Makefile"), "kernel Makefile")?;
    ensure_file(
        &path.join("scripts/kconfig/merge_config.sh"),
        "merge_config.sh",
    )?;
    Ok(path)
}

fn canonical_file(path: &Path, description: &str) -> Result<PathBuf> {
    let path =
        fs::canonicalize(path).map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    ensure_file(&path, description)?;
    Ok(path)
}

fn make_command_for_arch(kernel_tree: &Path, out_dir: &Path, arch: &str) -> Result<Command> {
    let make = env::var_os("MAKE").unwrap_or_else(|| "make".into());
    let mut output = OsString::from("O=");
    output.push(out_dir.as_os_str());

    let mut command = Command::new(make);
    command
        .current_dir(kernel_tree)
        .env("ARCH", arch)
        .arg(output);
    set_default_kernel_toolchain(&mut command, arch, Some(out_dir))?;
    Ok(command)
}

fn set_default_kernel_toolchain(
    command: &mut Command,
    arch: &str,
    out_dir: Option<&Path>,
) -> Result<()> {
    if env::var_os("CROSS_COMPILE").is_none() && env::var_os("LLVM").is_none() {
        match arch {
            DEFAULT_KERNEL_ARCH => {
                command.env("CROSS_COMPILE", "aarch64-linux-gnu-");
            }
            "arm" => {
                command.env("LLVM", "1");
                if !path_command_exists("ld.lld") {
                    if let Some(tool_dir) = arm_llvm_tool_dir(out_dir)? {
                        prepend_command_path(command, &tool_dir)?;
                    }
                }
                if env::var_os("LLVM_IAS").is_none() {
                    command.env("LLVM_IAS", "1");
                }
            }
            _ => {}
        }
    }
    if let Some(out_dir) = out_dir {
        prepend_ccache_wrappers(command, out_dir)?;
    }
    Ok(())
}

fn prepend_ccache_wrappers(command: &mut Command, out_dir: &Path) -> Result<()> {
    if env::var_os("CCACHE_DIR").is_none() {
        return Ok(());
    }

    let tool_dir = kernel_tool_dir(out_dir)?;
    for compiler in [
        "gcc",
        "g++",
        "cc",
        "c++",
        "aarch64-linux-gnu-gcc",
        "arm-linux-gnueabihf-gcc",
        "clang",
        "clang++",
    ] {
        if let Some(real_compiler) = compiler_path_command(compiler, &tool_dir) {
            write_ccache_wrapper(&tool_dir.join(compiler), &real_compiler)?;
        }
    }
    prepend_command_path(command, &tool_dir)
}

/// Resolve a compiler without selecting a distro ccache shim which would make
/// our own `ccache <compiler>` wrapper recurse back into ccache forever.
///
/// An explicit path remains exactly as supplied. Bare command names are
/// searched in PATH order, skipping the wrapper output directory, conventional
/// ccache shim directories, and candidates which resolve to the ccache binary.
fn compiler_path_command(command: &str, wrapper_dir: &Path) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    let search_paths = env::split_paths(&paths).collect::<Vec<_>>();
    let ccache = path_command("ccache");
    resolve_compiler_command(
        OsStr::new(command),
        &search_paths,
        ccache.as_deref(),
        wrapper_dir,
    )
}

fn resolve_compiler_command(
    command: &OsStr,
    search_paths: &[PathBuf],
    ccache: Option<&Path>,
    wrapper_dir: &Path,
) -> Option<PathBuf> {
    let requested = Path::new(command);
    if requested.is_absolute() || requested.components().count() > 1 {
        return requested.is_file().then(|| requested.to_path_buf());
    }

    search_paths
        .iter()
        .map(|dir| dir.join(command))
        .find(|candidate| {
            candidate.is_file()
                && !candidate.starts_with(wrapper_dir)
                && !is_ccache_shim(candidate, ccache)
        })
}

fn is_ccache_shim(candidate: &Path, ccache: Option<&Path>) -> bool {
    if candidate
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == OsStr::new("ccache"))
    {
        return true;
    }

    let Some(ccache) = ccache else {
        return false;
    };
    match (fs::canonicalize(candidate), fs::canonicalize(ccache)) {
        (Ok(candidate), Ok(ccache)) => candidate == ccache,
        _ => false,
    }
}

fn kernel_tool_dir(out_dir: &Path) -> Result<PathBuf> {
    let tool_dir = out_dir.join("pocketboot-toolchain");
    fs::create_dir_all(&tool_dir).map_err(|err| format!("create {}: {err}", tool_dir.display()))?;
    Ok(tool_dir)
}

fn write_ccache_wrapper(path: &Path, compiler: &Path) -> Result<()> {
    let compiler = compiler
        .to_str()
        .ok_or_else(|| format!("compiler path is not valid UTF-8: {}", compiler.display()))?;
    fs::write(
        path,
        format!("#!/bin/sh\nexec ccache '{}' \"$@\"\n", sh_quote(compiler)),
    )
    .map_err(|err| format!("write {}: {err}", path.display()))?;
    make_executable(path)
}

fn sh_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

fn arm_llvm_tool_dir(out_dir: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(out_dir) = out_dir else {
        return Ok(None);
    };
    if !path_command_exists("rust-lld") {
        return Ok(None);
    }
    let tool_dir = kernel_tool_dir(out_dir)?;
    let ld_lld = tool_dir.join("ld.lld");
    write_ld_lld_wrapper(&ld_lld)?;
    Ok(Some(tool_dir))
}

fn write_ld_lld_wrapper(path: &Path) -> Result<()> {
    if path.symlink_metadata().is_ok() {
        fs::remove_file(path).map_err(|err| format!("remove {}: {err}", path.display()))?;
    }
    fs::write(path, b"#!/bin/sh\nexec rust-lld -flavor gnu \"$@\"\n")
        .map_err(|err| format!("write {}: {err}", path.display()))?;
    make_executable(path)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    fs::set_permissions(path, Permissions::from_mode(0o755))
        .map_err(|err| format!("chmod {}: {err}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn prepend_command_path(command: &mut Command, dir: &Path) -> Result<()> {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    let path = env::join_paths(paths).map_err(|err| format!("join PATH: {err}"))?;
    command.env("PATH", path);
    Ok(())
}

fn path_command(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|dir| dir.join(OsStr::new(name)))
        .find(|path| path.is_file())
}

fn path_command_exists(name: &str) -> bool {
    path_command(name).is_some()
}

fn run_command(mut command: Command, action: &str) -> Result<()> {
    let status = command
        .status()
        .map_err(|err| format!("spawn {action}: {err}"))?;
    if !status.success() {
        return Err(format!("{action} failed with {status}"));
    }
    Ok(())
}

fn parallel_jobs() -> usize {
    thread::available_parallelism().map_or(1, usize::from)
}

fn ensure_file(path: &Path, description: &str) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("missing {description}: {}", path.display()))
    }
}

fn kconfig_string(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?;
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(ch),
        }
    }
    Ok(escaped)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_resolution_skips_fedora_ccache_shim_directory() {
        let root = test_dir("ccache-fedora");
        let shim_dir = root.join("usr/lib64/ccache");
        let real_dir = root.join("usr/bin");
        let wrapper_dir = root.join("out/pocketboot-toolchain");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::create_dir_all(&real_dir).unwrap();
        fs::create_dir_all(&wrapper_dir).unwrap();
        fs::write(shim_dir.join("aarch64-linux-gnu-gcc"), b"ccache shim").unwrap();
        fs::write(real_dir.join("aarch64-linux-gnu-gcc"), b"real compiler").unwrap();

        let selected = resolve_compiler_command(
            OsStr::new("aarch64-linux-gnu-gcc"),
            &[shim_dir, real_dir.clone()],
            None,
            &wrapper_dir,
        )
        .unwrap();

        assert_eq!(selected, real_dir.join("aarch64-linux-gnu-gcc"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn compiler_resolution_skips_symlink_to_ccache_outside_named_shim_directory() {
        use std::os::unix::fs::symlink;

        let root = test_dir("ccache-symlink");
        let shim_dir = root.join("wrappers");
        let real_dir = root.join("bin");
        let wrapper_dir = root.join("out/pocketboot-toolchain");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::create_dir_all(&real_dir).unwrap();
        fs::create_dir_all(&wrapper_dir).unwrap();
        let ccache = real_dir.join("ccache");
        fs::write(&ccache, b"ccache").unwrap();
        symlink(&ccache, shim_dir.join("gcc")).unwrap();
        fs::write(real_dir.join("gcc"), b"real compiler").unwrap();

        let selected = resolve_compiler_command(
            OsStr::new("gcc"),
            &[shim_dir, real_dir.clone()],
            Some(&ccache),
            &wrapper_dir,
        )
        .unwrap();

        assert_eq!(selected, real_dir.join("gcc"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_compiler_path_and_shell_quoting_are_preserved() {
        let root = test_dir("ccache-explicit");
        let wrapper_dir = root.join("wrapper output");
        let compiler = root.join("toolchain with spaces/compiler's gcc");
        fs::create_dir_all(compiler.parent().unwrap()).unwrap();
        fs::create_dir_all(&wrapper_dir).unwrap();
        fs::write(&compiler, b"real compiler").unwrap();

        let selected =
            resolve_compiler_command(compiler.as_os_str(), &[], None, &wrapper_dir).unwrap();
        assert_eq!(selected, compiler);

        let wrapper = wrapper_dir.join("gcc");
        write_ccache_wrapper(&wrapper, &selected).unwrap();
        let contents = fs::read_to_string(&wrapper).unwrap();
        let compiler = selected.to_str().unwrap();
        assert_eq!(
            contents,
            format!("#!/bin/sh\nexec ccache '{}' \"$@\"\n", sh_quote(compiler))
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compiler_resolution_does_not_reuse_generated_wrapper() {
        let root = test_dir("ccache-wrapper-loop");
        let wrapper_dir = root.join("pocketboot-toolchain");
        let real_dir = root.join("bin");
        fs::create_dir_all(&wrapper_dir).unwrap();
        fs::create_dir_all(&real_dir).unwrap();
        fs::write(wrapper_dir.join("clang"), b"old generated wrapper").unwrap();
        fs::write(real_dir.join("clang"), b"real compiler").unwrap();

        let selected = resolve_compiler_command(
            OsStr::new("clang"),
            &[wrapper_dir.clone(), real_dir.clone()],
            None,
            &wrapper_dir,
        )
        .unwrap();

        assert_eq!(selected, real_dir.join("clang"));
        fs::remove_dir_all(root).unwrap();
    }

    fn test_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "pocketboot-xtask-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
}
