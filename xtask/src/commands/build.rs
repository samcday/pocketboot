use std::path::PathBuf;

use crate::Result;

use super::{KernelDevice, bootimg, config, cpio, kernel, kernel_tree, target_dir, workspace_root};

#[derive(clap::Args, Debug)]
pub(crate) struct BuildArgs {
    #[arg(long, value_name = "PATH")]
    kernel: Option<PathBuf>,
    #[arg(value_name = "VENDOR/DEVICE")]
    device: KernelDevice,
}

pub(crate) fn run(args: BuildArgs) -> Result<()> {
    build(args)
}

fn build(args: BuildArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let target_dir = target_dir(&workspace_root);
    let device_config = config::load_device_config(&workspace_root, &args.device)?;
    let kernel_tree = match args.kernel {
        Some(kernel_tree_arg) => kernel_tree(&kernel_tree_arg)?,
        None => kernel::prepare_device_kernel_source(&workspace_root, &args.device)?,
    };

    let initrd_target = cpio::device_initrd_target(&device_config.cpio)?;
    cpio::build_release(&workspace_root, &initrd_target, &device_config.features)?;

    let kernel_config =
        kernel::configure_device_kernel(&workspace_root, &kernel_tree, &args.device, None)?;
    println!("config {}", kernel_config.config.display());
    println!("initrd {}", kernel_config.initrd.display());

    let modules = kernel::build_device_modules(&workspace_root, &kernel_tree, &args.device)?;
    println!("modules {}", modules.modules.display());

    let initrd = cpio::build_device_initrd(
        &workspace_root,
        &target_dir,
        &args.device,
        &device_config.cpio,
        &device_config.features,
        None,
        false,
    )?;
    println!("wrote {}", initrd.display());

    let kernel_image =
        kernel::build_device_image(&workspace_root, &kernel_tree, &args.device, None)?;
    println!("image {}", kernel_image.image.display());
    if let Some(dtb) = &kernel_image.dtb {
        println!("dtb {}", dtb.display());
    }

    if device_config.bootimg.is_some() {
        bootimg::build_device_bootimg(&workspace_root, &args.device, None)?;
    } else {
        println!("bootimg skipped: missing [bootimg] table");
    }

    Ok(())
}
