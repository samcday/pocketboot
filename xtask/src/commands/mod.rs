pub(crate) mod bootimg;
pub(crate) mod busybox;
pub(crate) mod ci_workflows;
mod config;
pub(crate) mod cpio;
pub(crate) mod kernel;
pub(crate) mod kernel_matrix;
pub(crate) mod kernel_prime;
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
    Ok(())
}

fn arm_llvm_tool_dir(out_dir: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(out_dir) = out_dir else {
        return Ok(None);
    };
    if !path_command_exists("rust-lld") {
        return Ok(None);
    }
    let tool_dir = out_dir.join("pocketboot-toolchain");
    fs::create_dir_all(&tool_dir).map_err(|err| format!("create {}: {err}", tool_dir.display()))?;
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
