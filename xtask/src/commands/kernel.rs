use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;

use crate::Result;

use super::{
    FeatureSet, KERNEL_ARCH, KernelDevice, canonical_file,
    cpio::{DEFAULT_INITRD, DEFAULT_TARGET, build_initrd},
    ensure_file, kconfig_string, kernel_tree, make_command, parallel_jobs, run_command, target_dir,
    workspace_root,
};

#[derive(Debug)]
struct KernelArgs {
    device: KernelDevice,
    kernel_tree: PathBuf,
    initrd: Option<PathBuf>,
}

#[derive(Debug)]
pub(super) struct KernelBuild {
    pub(super) image: PathBuf,
    pub(super) dtb: Option<PathBuf>,
    pub(super) config: PathBuf,
    pub(super) initrd: PathBuf,
}

struct KernelTarget {
    image_make_target: String,
    image_path: PathBuf,
    build_dtb: bool,
}

#[derive(Debug, Default, Deserialize)]
struct DeviceMetadata {
    #[serde(default)]
    kernel: KernelMetadata,
    #[serde(default)]
    cpio: CpioMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct KernelMetadata {
    image: Option<String>,
    image_path: Option<PathBuf>,
    dtb: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct CpioMetadata {
    target: Option<String>,
    busybox: Option<bool>,
    #[serde(default)]
    features: Vec<String>,
}

impl KernelArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut initrd = None;
        let mut positionals = Vec::new();
        let mut index = 0;

        while index < args.len() {
            let arg = &args[index];
            match arg.as_str() {
                "--initrd" => {
                    index += 1;
                    initrd = Some(PathBuf::from(
                        args.get(index)
                            .ok_or_else(|| "--initrd requires a value".to_string())?,
                    ));
                }
                value if value.starts_with("--initrd=") => {
                    initrd = Some(PathBuf::from(&value["--initrd=".len()..]));
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown kernel option: {value}"));
                }
                value => positionals.push(value.to_string()),
            }
            index += 1;
        }

        if positionals.len() != 2 {
            return Err(
                "usage: cargo xtask kernel [--initrd PATH] <vendor/device> <kernel-tree>"
                    .to_string(),
            );
        }

        Ok(Self {
            device: KernelDevice::parse(&positionals[0])?,
            kernel_tree: PathBuf::from(&positionals[1]),
            initrd,
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
        kernel(KernelArgs::parse(args)?)
    }
}

fn kernel(args: KernelArgs) -> Result<()> {
    let KernelArgs {
        device,
        kernel_tree: kernel_tree_arg,
        initrd,
    } = args;
    let workspace_root = workspace_root()?;
    let kernel_tree = kernel_tree(&kernel_tree_arg)?;
    let built_initrd = initrd.is_none();
    let build = build_device_kernel(&workspace_root, &kernel_tree, &device, initrd)?;

    if built_initrd {
        println!("wrote {}", build.initrd.display());
    }
    println!("image {}", build.image.display());
    if let Some(dtb) = &build.dtb {
        println!("dtb {}", dtb.display());
    }
    println!("config {}", build.config.display());
    Ok(())
}

pub(super) fn build_device_kernel_id(
    workspace_root: &Path,
    kernel_tree: &Path,
    device_id: &str,
    initrd: Option<PathBuf>,
) -> Result<KernelBuild> {
    let device = KernelDevice::parse(device_id)?;
    build_device_kernel(workspace_root, kernel_tree, &device, initrd)
}

fn build_device_kernel(
    workspace_root: &Path,
    kernel_tree: &Path,
    device: &KernelDevice,
    initrd: Option<PathBuf>,
) -> Result<KernelBuild> {
    let target_dir = target_dir(workspace_root);
    let metadata = device_metadata(workspace_root, device)?;
    let target = kernel_target(&metadata.kernel)?;
    let out_dir = target_dir
        .join("kernel")
        .join(&device.vendor)
        .join(&device.stem);
    fs::create_dir_all(&out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;

    let common_config = workspace_root.join("configs/pocketboot.config");
    let soc_config = workspace_root
        .join("configs/soc")
        .join(&device.vendor)
        .join(format!("{}.config", device.soc));
    let device_config = workspace_root
        .join("configs/device")
        .join(&device.vendor)
        .join(format!("{}.config", device.stem));
    let dts_source = kernel_tree
        .join("arch/arm64/boot/dts")
        .join(&device.vendor)
        .join(format!("{}.dts", device.stem));

    ensure_file(&common_config, "common pocketboot config")?;
    ensure_file(&soc_config, "SoC config")?;
    ensure_file(&device_config, "device config")?;
    if target.build_dtb {
        ensure_file(&dts_source, "device tree source")?;
    }

    let initrd = match initrd {
        Some(initrd) => canonical_file(&initrd, "initrd cpio")?,
        None => build_device_initrd(workspace_root, &target_dir, device, &metadata.cpio)?,
    };

    let initramfs_config = out_dir.join("pocketboot-initramfs.config");
    fs::write(
        &initramfs_config,
        format!("CONFIG_INITRAMFS_SOURCE=\"{}\"\n", kconfig_string(&initrd)?),
    )
    .map_err(|err| format!("write {}: {err}", initramfs_config.display()))?;

    let merge_config = kernel_tree.join("scripts/kconfig/merge_config.sh");
    ensure_file(&merge_config, "merge_config.sh")?;
    let mut merge = Command::new(&merge_config);
    merge
        .current_dir(kernel_tree)
        .env("ARCH", KERNEL_ARCH)
        .args(["-s", "-n", "-O"])
        .arg(&out_dir)
        .arg(&common_config)
        .arg(&soc_config)
        .arg(&device_config)
        .arg(&initramfs_config);
    run_command(merge, "merge kernel config")?;

    let mut olddefconfig = make_command(kernel_tree, &out_dir);
    olddefconfig.arg("olddefconfig");
    run_command(olddefconfig, "make olddefconfig")?;

    let dtb_target = target
        .build_dtb
        .then(|| format!("{}/{}.dtb", device.vendor, device.stem));
    let mut build = make_command(kernel_tree, &out_dir);
    build
        .arg(format!("-j{}", parallel_jobs()))
        .arg(&target.image_make_target);
    if let Some(dtb_target) = &dtb_target {
        build.arg(dtb_target);
    }
    let action = if target.build_dtb {
        "make kernel image and dtb"
    } else {
        "make kernel image"
    };
    run_command(build, action)?;

    let image = out_dir.join(&target.image_path);
    let dtb = target.build_dtb.then(|| {
        out_dir
            .join("arch/arm64/boot/dts")
            .join(&device.vendor)
            .join(format!("{}.dtb", device.stem))
    });
    ensure_file(&image, "kernel image")?;
    if let Some(dtb) = &dtb {
        ensure_file(dtb, "device tree blob")?;
    }

    Ok(KernelBuild {
        image,
        dtb,
        config: out_dir.join(".config"),
        initrd,
    })
}

fn device_metadata(workspace_root: &Path, device: &KernelDevice) -> Result<DeviceMetadata> {
    let path = workspace_root
        .join("configs/device")
        .join(&device.vendor)
        .join(format!("{}.toml", device.stem));
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DeviceMetadata::default());
        }
        Err(err) => return Err(format!("read {}: {err}", path.display())),
    };
    toml::from_str(&contents).map_err(|err| format!("parse {}: {err}", path.display()))
}

fn kernel_target(config: &KernelMetadata) -> Result<KernelTarget> {
    let image_make_target = config
        .image
        .clone()
        .unwrap_or_else(|| "Image.gz".to_string());
    if image_make_target.is_empty() {
        return Err("kernel image target must not be empty".to_string());
    }
    let image_path = config
        .image_path
        .clone()
        .unwrap_or_else(|| PathBuf::from("arch/arm64/boot").join(&image_make_target));

    Ok(KernelTarget {
        image_make_target,
        image_path,
        build_dtb: config.dtb.unwrap_or(true),
    })
}

fn build_device_initrd(
    workspace_root: &Path,
    target_dir: &Path,
    device: &KernelDevice,
    config: &CpioMetadata,
) -> Result<PathBuf> {
    let target = config.target.as_deref().unwrap_or(DEFAULT_TARGET);
    if target.is_empty() {
        return Err("cpio target must not be empty".to_string());
    }

    let mut features = FeatureSet::default();
    for feature in &config.features {
        features.add(feature)?;
    }

    build_initrd(
        workspace_root,
        target,
        Some(
            target_dir
                .join("cpio")
                .join(&device.vendor)
                .join(&device.stem)
                .join(DEFAULT_INITRD),
        ),
        config.busybox.unwrap_or(true),
        &features,
    )
}

fn print_usage() {
    println!(
        "usage: cargo xtask kernel [--initrd PATH] <vendor/device> <kernel-tree>\n\nexample: cargo xtask kernel qcom/msm8916-samsung-a5u-eur ./linux\n\nwhen --initrd is omitted, target/cpio/<vendor>/<device>/{DEFAULT_INITRD} is rebuilt automatically\noutputs: target/kernel/<vendor>/<device>/arch/arm64/boot/Image.gz and the inferred DTB by default; optional configs/device/<vendor>/<device>.toml can tailor kernel artifacts and cpio features"
    );
}
