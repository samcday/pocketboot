use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;

use super::{
    FeatureSet, KERNEL_ARCH, KernelDevice, canonical_file,
    config::{self, CpioConfig, KernelConfig},
    cpio::{DEFAULT_INITRD, DEFAULT_TARGET, build_initrd},
    ensure_file, kconfig_string, kernel_tree, make_command, parallel_jobs, run_command, target_dir,
    workspace_root,
};

#[derive(clap::Args, Debug)]
pub(crate) struct KernelArgs {
    #[arg(value_name = "VENDOR/DEVICE")]
    device: KernelDevice,
    #[arg(value_name = "KERNEL_TREE")]
    kernel_tree: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct KernelConfigStamp {
    input: KernelConfigInput,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct KernelConfigInput {
    recipe_version: u32,
    arch: String,
    kernel_tree: String,
    merge_config: KernelConfigFile,
    fragments: Vec<KernelConfigFile>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct KernelConfigFile {
    path: String,
    sha256: String,
}

const KERNEL_CONFIG_RECIPE_VERSION: u32 = 1;

pub(crate) fn run(args: KernelArgs) -> Result<()> {
    kernel(args)
}

fn kernel(args: KernelArgs) -> Result<()> {
    let KernelArgs {
        device,
        kernel_tree: kernel_tree_arg,
        initrd,
    } = args;
    let workspace_root = workspace_root()?;
    let kernel_tree = match kernel_tree_arg {
        Some(kernel_tree_arg) => kernel_tree(&kernel_tree_arg)?,
        None => {
            let tree = super::kernel_src::ensure_device_kernel_source(&workspace_root, &device)?;
            println!("kernel source {}", tree.path.display());
            kernel_tree(&tree.path)?
        }
    };
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
    let config = config::load_device_config(workspace_root, device)?;
    let target = kernel_target(&config.kernel)?;
    let out_dir = target_dir
        .join("kernel")
        .join(&device.vendor)
        .join(&device.stem);
    fs::create_dir_all(&out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;

    let dts_source = kernel_tree
        .join("arch/arm64/boot/dts")
        .join(&device.vendor)
        .join(format!("{}.dts", device.stem));

    if target.build_dtb {
        ensure_file(&dts_source, "device tree source")?;
    }

    let initrd = match initrd {
        Some(initrd) => canonical_file(&initrd, "initrd cpio")?,
        None => build_device_initrd(
            workspace_root,
            &target_dir,
            device,
            &config.cpio,
            &config.features,
        )?,
    };

    let pocketboot_config = out_dir.join("pocketboot.config");
    write_if_changed(&pocketboot_config, config.kconfig_contents()?.as_bytes())?;

    let initramfs_config = out_dir.join("pocketboot-initramfs.config");
    write_if_changed(
        &initramfs_config,
        format!("CONFIG_INITRAMFS_SOURCE=\"{}\"\n", kconfig_string(&initrd)?).as_bytes(),
    )?;

    let merge_config = kernel_tree.join("scripts/kconfig/merge_config.sh");
    ensure_file(&merge_config, "merge_config.sh")?;
    let config_fragments = [pocketboot_config.as_path(), initramfs_config.as_path()];
    ensure_kernel_config(kernel_tree, &out_dir, &merge_config, &config_fragments)?;

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

fn kernel_target(config: &KernelConfig) -> Result<KernelTarget> {
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
    config: &CpioConfig,
    features: &FeatureSet,
) -> Result<PathBuf> {
    let target = config.target.as_deref().unwrap_or(DEFAULT_TARGET);
    if target.is_empty() {
        return Err("cpio target must not be empty".to_string());
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
        features.contains("busybox"),
        features,
    )
}

fn ensure_kernel_config(
    kernel_tree: &Path,
    out_dir: &Path,
    merge_config: &Path,
    fragments: &[&Path],
) -> Result<()> {
    let input = kernel_config_input(kernel_tree, merge_config, fragments)?;
    let stamp = out_dir.join("pocketboot-config.toml");
    let config = out_dir.join(".config");
    if config.is_file() && kernel_config_stamp_matches(&stamp, &input)? {
        return Ok(());
    }

    let mut merge = Command::new(merge_config);
    merge
        .current_dir(kernel_tree)
        .env("ARCH", KERNEL_ARCH)
        .args(["-s", "-n", "-O"])
        .arg(out_dir)
        .args(fragments);
    run_command(merge, "merge kernel config")?;

    let mut olddefconfig = make_command(kernel_tree, out_dir);
    olddefconfig.arg("olddefconfig");
    run_command(olddefconfig, "make olddefconfig")?;

    write_kernel_config_stamp(&stamp, &input)
}

fn kernel_config_input(
    kernel_tree: &Path,
    merge_config: &Path,
    fragments: &[&Path],
) -> Result<KernelConfigInput> {
    Ok(KernelConfigInput {
        recipe_version: KERNEL_CONFIG_RECIPE_VERSION,
        arch: KERNEL_ARCH.to_string(),
        kernel_tree: path_stamp_value(kernel_tree),
        merge_config: kernel_config_file(merge_config)?,
        fragments: fragments
            .iter()
            .map(|fragment| kernel_config_file(fragment))
            .collect::<Result<Vec<_>>>()?,
    })
}

fn kernel_config_file(path: &Path) -> Result<KernelConfigFile> {
    Ok(KernelConfigFile {
        path: path_stamp_value(path),
        sha256: hash_file(path)?,
    })
}

fn kernel_config_stamp_matches(path: &Path, input: &KernelConfigInput) -> Result<bool> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(format!("read {}: {err}", path.display())),
    };
    let Ok(stamp) = toml::from_str::<KernelConfigStamp>(&contents) else {
        return Ok(false);
    };
    Ok(stamp.input == *input)
}

fn write_kernel_config_stamp(path: &Path, input: &KernelConfigInput) -> Result<()> {
    let stamp = KernelConfigStamp {
        input: input.clone(),
    };
    let contents = toml::to_string_pretty(&stamp)
        .map_err(|err| format!("encode kernel config stamp: {err}"))?;
    write_if_changed(path, contents.as_bytes())
}

fn write_if_changed(path: &Path, contents: &[u8]) -> Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == contents => return Ok(()),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("read {}: {err}", path.display())),
    }

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(path, contents).map_err(|err| format!("write {}: {err}", path.display()))
}

fn hash_file(path: &Path) -> Result<String> {
    let contents = fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(contents);
    Ok(format!("{:x}", hasher.finalize()))
}

fn path_stamp_value(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
