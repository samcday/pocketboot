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

#[derive(Debug)]
struct PrebootArgs {
    device: KernelDevice,
    target: String,
    output: Option<PathBuf>,
    features: FeatureSet,
}

impl PrebootArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut device = None;
        let mut target = DEFAULT_TARGET.to_string();
        let mut output = None;
        let mut features = FeatureSet::default();
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
                "--features" => {
                    index += 1;
                    features.add(
                        args.get(index)
                            .ok_or_else(|| "--features requires a value".to_string())?,
                    )?;
                }
                value if value.starts_with("--target=") => {
                    target = value["--target=".len()..].to_string();
                }
                value if value.starts_with("--output=") => {
                    output = Some(PathBuf::from(&value["--output=".len()..]));
                }
                value if value.starts_with("--features=") => {
                    features.add(&value["--features=".len()..])?;
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown preboot option: {value}"));
                }
                value => {
                    if device.is_some() {
                        return Err(format!("unexpected positional argument: {value}"));
                    }
                    device = Some(KernelDevice::parse(value)?);
                }
            }
            index += 1;
        }

        let device = device.ok_or_else(|| {
            "usage: cargo xtask preboot <vendor/device> [--output PATH] [--target TARGET] [--features FEATURES]".to_string()
        })?;

        Ok(Self {
            device,
            target,
            output,
            features,
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
        preboot(PrebootArgs::parse(args)?)
    }
}

fn preboot(args: PrebootArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let device_config = config::load_device_config(&workspace_root, &args.device)?;
    let build = build_device_preboot(
        &workspace_root,
        &args.device,
        &args.target,
        args.output,
        args.features,
    )?;

    println!("wrote {}", build.binary.display());
    println!("elf {}", build.elf.display());
    println!("features {}", build.features);
    println!("config {}", device_config.device_path.display());
    Ok(())
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

fn print_usage() {
    println!(
        "usage: cargo xtask preboot <vendor/device> [--output PATH] [--target TARGET] [--features FEATURES]\n\nexample: cargo xtask preboot exynos/exynos7870-j7xelte\n\nbuilds: pocketpreboot for the device-derived soc-* and device-* Cargo features\ndefault target: {DEFAULT_TARGET}\ndefault output: target/preboot/<vendor>/<device>/pocketpreboot.bin"
    );
}
