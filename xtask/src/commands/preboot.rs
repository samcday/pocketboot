use std::{env, fs, path::Path, path::PathBuf, process::Command};

use crate::Result;

use super::{FeatureSet, KernelDevice, config, ensure_file, target_dir, workspace_root};

pub(super) const DEFAULT_TARGET: &str = "aarch64-unknown-none";
const PACKAGE: &str = "pocketpreboot";

pub(super) struct PrebootBuild {
    pub(super) binary: PathBuf,
    pub(super) elf: PathBuf,
    pub(super) features: String,
}

#[derive(clap::Args, Debug)]
pub(crate) struct PrebootArgs {
    #[arg(value_name = "VENDOR/DEVICE")]
    device: KernelDevice,
    #[arg(long, default_value = DEFAULT_TARGET, value_name = "TARGET")]
    target: String,
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    #[arg(long, value_name = "FEATURES", value_parser = parse_features)]
    features: Vec<FeatureSet>,
}

pub(crate) fn run(args: PrebootArgs) -> Result<()> {
    preboot(args)
}

fn parse_features(value: &str) -> Result<FeatureSet> {
    let mut features = FeatureSet::default();
    features.add(value)?;
    Ok(features)
}

fn preboot(args: PrebootArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let device_config = config::load_device_config(&workspace_root, &args.device)?;
    let build = build_device_preboot(
        &workspace_root,
        &args.device,
        &args.target,
        args.output,
        merge_features(args.features)?,
    )?;

    println!("wrote {}", build.binary.display());
    println!("elf {}", build.elf.display());
    println!("features {}", build.features);
    println!("config {}", device_config.device_path.display());
    Ok(())
}

fn merge_features(values: Vec<FeatureSet>) -> Result<FeatureSet> {
    let mut features = FeatureSet::default();
    for value in values {
        for feature in value.values() {
            features.add(feature)?;
        }
    }
    Ok(features)
}

pub(super) fn build_device_preboot(
    workspace_root: &Path,
    device: &KernelDevice,
    target: &str,
    output: Option<PathBuf>,
    mut features: FeatureSet,
) -> Result<PrebootBuild> {
    let target_dir = target_dir(workspace_root);
    let out_dir = target_dir
        .join("preboot")
        .join(&device.vendor)
        .join(&device.stem);
    fs::create_dir_all(&out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;

    features.add(&format!("soc-{}", device.soc))?;
    features.add(&format!("device-{}", device.stem))?;
    let feature_list = features.cargo_value();

    build_preboot(workspace_root, target, &features)?;

    let elf = target_dir.join(target).join("release").join(PACKAGE);
    ensure_file(&elf, "pocketpreboot ELF")?;

    let binary = output.unwrap_or_else(|| out_dir.join(format!("{PACKAGE}.bin")));
    if let Some(parent) = binary
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    objcopy_binary(&elf, &binary)?;

    Ok(PrebootBuild {
        binary,
        elf,
        features: feature_list,
    })
}

fn build_preboot(workspace_root: &Path, target: &str, features: &FeatureSet) -> Result<()> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let cargo_features = features.cargo_value();
    let mut command = Command::new(cargo);
    command.current_dir(workspace_root).args([
        "build",
        "--release",
        "--target",
        target,
        "-p",
        PACKAGE,
        "--features",
        &cargo_features,
    ]);

    let status = command
        .status()
        .map_err(|err| format!("spawn cargo build: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo build failed with {status}"))
    }
}

fn objcopy_binary(elf: &Path, output: &Path) -> Result<()> {
    let mut candidates = Vec::new();
    if let Some(objcopy) = env::var_os("OBJCOPY") {
        candidates.push(objcopy);
    }
    candidates.push("llvm-objcopy".into());
    candidates.push("rust-objcopy".into());
    candidates.push("aarch64-linux-gnu-objcopy".into());
    candidates.push("objcopy".into());

    let mut errors = Vec::new();
    for candidate in candidates {
        let status = Command::new(&candidate)
            .arg("-O")
            .arg("binary")
            .arg(elf)
            .arg(output)
            .status();

        match status {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => errors.push(format!(
                "{} exited with {status}",
                candidate.to_string_lossy()
            )),
            Err(err) => errors.push(format!("{}: {err}", candidate.to_string_lossy())),
        }
    }

    Err(format!(
        "failed to objcopy {}: {}",
        elf.display(),
        errors.join("; ")
    ))
}
