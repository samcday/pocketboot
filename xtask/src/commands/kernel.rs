use std::{fs, path::PathBuf, process::Command};

use crate::Result;

use super::{
    KERNEL_ARCH, KernelDevice, canonical_file,
    cpio::{DEFAULT_INITRD, DEFAULT_TARGET, FeatureSet, build_initrd},
    ensure_file, kconfig_string, kernel_tree, make_command, parallel_jobs, run_command, target_dir,
    workspace_root,
};

#[derive(Debug)]
struct KernelArgs {
    device: KernelDevice,
    kernel_tree: PathBuf,
    initrd: Option<PathBuf>,
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
    let workspace_root = workspace_root()?;
    let kernel_tree = kernel_tree(&args.kernel_tree)?;
    let target_dir = target_dir(&workspace_root);
    let out_dir = target_dir
        .join("kernel")
        .join(&args.device.vendor)
        .join(&args.device.stem);
    fs::create_dir_all(&out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;

    let common_config = workspace_root.join("configs/pocketboot.config");
    let soc_config = workspace_root
        .join("configs/soc")
        .join(&args.device.vendor)
        .join(format!("{}.config", args.device.soc));
    let device_config = workspace_root
        .join("configs/device")
        .join(&args.device.vendor)
        .join(format!("{}.config", args.device.stem));
    let dts_source = kernel_tree
        .join("arch/arm64/boot/dts")
        .join(&args.device.vendor)
        .join(format!("{}.dts", args.device.stem));

    ensure_file(&common_config, "common pocketboot config")?;
    ensure_file(&soc_config, "SoC config")?;
    ensure_file(&device_config, "device config")?;
    ensure_file(&dts_source, "device tree source")?;

    let initrd = match args.initrd {
        Some(initrd) => canonical_file(&initrd, "initrd cpio")?,
        None => {
            let initrd = build_initrd(
                &workspace_root,
                DEFAULT_TARGET,
                None,
                true,
                &FeatureSet::default(),
            )?;
            println!("wrote {}", initrd.display());
            initrd
        }
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
        .current_dir(&kernel_tree)
        .env("ARCH", KERNEL_ARCH)
        .args(["-s", "-n", "-O"])
        .arg(&out_dir)
        .arg(&common_config)
        .arg(&soc_config)
        .arg(&device_config)
        .arg(&initramfs_config);
    run_command(merge, "merge kernel config")?;

    let mut olddefconfig = make_command(&kernel_tree, &out_dir);
    olddefconfig.arg("olddefconfig");
    run_command(olddefconfig, "make olddefconfig")?;

    let dtb_target = format!("{}/{}.dtb", args.device.vendor, args.device.stem);
    let mut build = make_command(&kernel_tree, &out_dir);
    build
        .arg(format!("-j{}", parallel_jobs()))
        .arg("Image.gz")
        .arg(&dtb_target);
    run_command(build, "make kernel image and dtb")?;

    let image = out_dir.join("arch/arm64/boot/Image.gz");
    let dtb = out_dir
        .join("arch/arm64/boot/dts")
        .join(&args.device.vendor)
        .join(format!("{}.dtb", args.device.stem));
    ensure_file(&image, "kernel image")?;
    ensure_file(&dtb, "device tree blob")?;

    println!("image {}", image.display());
    println!("dtb {}", dtb.display());
    println!("config {}", out_dir.join(".config").display());
    Ok(())
}

fn print_usage() {
    println!(
        "usage: cargo xtask kernel [--initrd PATH] <vendor/device> <kernel-tree>\n\nexample: cargo xtask kernel qcom/msm8916-samsung-a5u-eur ./linux\n\nwhen --initrd is omitted, target/{DEFAULT_INITRD} is rebuilt automatically\noutputs: target/kernel/<vendor>/<device>/arch/arm64/boot/Image.gz, the uncompressed Image prerequisite, and the inferred DTB"
    );
}
