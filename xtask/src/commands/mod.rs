pub(crate) mod bootimg;
pub(crate) mod busybox;
pub(crate) mod cpio;
pub(crate) mod kernel;
pub(crate) mod qemu;

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
};

use crate::Result;

const KERNEL_ARCH: &str = "arm64";

pub(crate) fn print_usage() {
    println!(
        "usage: cargo xtask <command>\n\ncommands:\n  busybox   build BusyBox for initrd use\n  cpio      build pocketboot and create an initrd cpio\n  kernel    build a pocketboot kernel image for one device\n  bootimg   package an already-built pocketboot kernel as boot.img\n  qemu      build and boot pocketboot under qemu-system-aarch64"
    );
}

#[derive(Clone, Debug, Default)]
pub(super) struct FeatureSet {
    values: Vec<String>,
}

impl FeatureSet {
    pub(super) fn qemu() -> Self {
        Self {
            values: vec!["qemu".to_string()],
        }
    }

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

#[derive(Debug)]
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

fn make_command(kernel_tree: &Path, out_dir: &Path) -> Command {
    let make = env::var_os("MAKE").unwrap_or_else(|| "make".into());
    let mut output = OsString::from("O=");
    output.push(out_dir.as_os_str());

    let mut command = Command::new(make);
    command
        .current_dir(kernel_tree)
        .env("ARCH", KERNEL_ARCH)
        .arg(output);
    if env::var_os("CROSS_COMPILE").is_none() {
        command.env("CROSS_COMPILE", "aarch64-linux-gnu-");
    }
    command
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
