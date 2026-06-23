use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;

use super::{
    DEFAULT_KERNEL_ARCH, FeatureSet, KernelDevice, canonical_file,
    config::{self, CpioConfig, KernelConfig},
    cpio::{DEFAULT_INITRD, DEFAULT_TARGET, build_initrd},
    ensure_file, kconfig_string, kernel_tree, make_command_for_arch, parallel_jobs, run_command,
    set_default_kernel_toolchain, target_dir, workspace_root,
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
    let arch = kernel_arch(&config.kernel)?;
    let dtb_stem = kernel_dtb_stem(&config.kernel, device)?;
    let target = kernel_target(&config.kernel, &arch)?;
    let out_dir = target_dir
        .join("kernel")
        .join(&device.vendor)
        .join(&device.stem);
    fs::create_dir_all(&out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;

    let dts_source = kernel_tree
        .join(format!("arch/{arch}/boot/dts"))
        .join(&device.vendor)
        .join(format!("{dtb_stem}.dts"));

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
    ensure_kernel_config(
        kernel_tree,
        &out_dir,
        &merge_config,
        &config_fragments,
        &arch,
    )?;

    let dtb_target = target
        .build_dtb
        .then(|| format!("{}/{dtb_stem}.dtb", device.vendor));
    let mut build = make_command_for_arch(kernel_tree, &out_dir, &arch)?;
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
            .join(format!("arch/{arch}/boot/dts"))
            .join(&device.vendor)
            .join(format!("{dtb_stem}.dtb"))
    });
    ensure_file(&image, "kernel image")?;
    write_bootimg_kernel_artifact(config.bootimg.as_ref(), &out_dir, &arch, &image, &target)?;
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

pub(super) fn kernel_arch(config: &KernelConfig) -> Result<String> {
    let arch = config
        .arch
        .clone()
        .unwrap_or_else(|| DEFAULT_KERNEL_ARCH.to_string());
    if arch.is_empty()
        || !arch
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err(format!("invalid kernel arch: {arch}"));
    }
    Ok(arch)
}

pub(super) fn kernel_dtb_stem(config: &KernelConfig, device: &KernelDevice) -> Result<String> {
    let stem = config
        .dtb_stem
        .clone()
        .unwrap_or_else(|| device.stem.clone());
    if stem.is_empty()
        || stem.ends_with(".dts")
        || stem.ends_with(".dtb")
        || !stem
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(format!("invalid kernel DTB stem: {stem}"));
    }
    Ok(stem)
}

fn kernel_target(config: &KernelConfig, arch: &str) -> Result<KernelTarget> {
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
        .unwrap_or_else(|| PathBuf::from(format!("arch/{arch}/boot")).join(&image_make_target));

    Ok(KernelTarget {
        image_make_target,
        image_path,
        build_dtb: config.dtb.unwrap_or(true),
    })
}

fn write_bootimg_kernel_artifact(
    config: Option<&config::BootImgConfig>,
    out_dir: &Path,
    arch: &str,
    image: &Path,
    target: &KernelTarget,
) -> Result<()> {
    let Some(config) = config else {
        return Ok(());
    };
    if config.kernel_image != "Image.gz" || target.image_make_target != "Image" {
        return Ok(());
    }

    let output = out_dir.join(format!("arch/{arch}/boot/Image.gz"));
    let output_file =
        File::create(&output).map_err(|err| format!("create {}: {err}", output.display()))?;
    let mut gzip = Command::new("gzip");
    gzip.arg("-n")
        .arg("-c")
        .arg(image)
        .stdout(Stdio::from(output_file));
    run_command(gzip, "gzip kernel Image")
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
    arch: &str,
) -> Result<()> {
    let input = kernel_config_input(kernel_tree, merge_config, fragments, arch)?;
    let stamp = out_dir.join("pocketboot-config.toml");
    let config = out_dir.join(".config");
    if config.is_file() && kernel_config_stamp_matches(&stamp, &input)? {
        return Ok(());
    }

    let mut merge = Command::new(merge_config);
    merge
        .current_dir(kernel_tree)
        .env("ARCH", arch)
        .args(["-s", "-n", "-O"])
        .arg(out_dir)
        .args(fragments);
    set_default_kernel_toolchain(&mut merge, arch, Some(out_dir))?;
    run_command(merge, "merge kernel config")?;

    let mut olddefconfig = make_command_for_arch(kernel_tree, out_dir, arch)?;
    olddefconfig.arg("olddefconfig");
    run_command(olddefconfig, "make olddefconfig")?;

    write_kernel_config_stamp(&stamp, &input)
}

fn kernel_config_input(
    kernel_tree: &Path,
    merge_config: &Path,
    fragments: &[&Path],
    arch: &str,
) -> Result<KernelConfigInput> {
    Ok(KernelConfigInput {
        recipe_version: KERNEL_CONFIG_RECIPE_VERSION,
        arch: arch.to_string(),
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
