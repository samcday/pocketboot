use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::Result;

use super::{KernelDevice, bootimg, kernel, workspace_root};

#[derive(clap::Args, Debug)]
pub(crate) struct BuildArgs {
    #[arg(value_name = "VENDOR/DEVICE")]
    device: Option<KernelDevice>,
    #[arg(value_name = "KERNEL_TREE")]
    kernel_tree: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    initrd: Option<PathBuf>,
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
}

pub(crate) fn run(args: BuildArgs) -> Result<()> {
    let Some(device) = args.device else {
        return list_devices();
    };

    kernel::run(kernel::KernelArgs {
        device: device.clone(),
        kernel_tree: args.kernel_tree,
        initrd: args.initrd,
    })?;
    bootimg::run(bootimg::BootImgArgs {
        device,
        output: args.output,
    })
}

fn list_devices() -> Result<()> {
    let workspace_root = workspace_root()?;
    let devices = configured_devices(&workspace_root)?;

    println!("available devices:");
    for device in devices {
        println!("  {device}");
    }
    Ok(())
}

fn configured_devices(workspace_root: &Path) -> Result<Vec<String>> {
    let device_root = workspace_root.join("configs/device");
    let mut devices = Vec::new();

    for vendor in fs::read_dir(&device_root)
        .map_err(|err| format!("read {}: {err}", device_root.display()))?
    {
        let vendor =
            vendor.map_err(|err| format!("read {} entry: {err}", device_root.display()))?;
        if !vendor
            .file_type()
            .map_err(|err| format!("stat {}: {err}", vendor.path().display()))?
            .is_dir()
        {
            continue;
        }

        let vendor_name = vendor.file_name();
        let vendor_name = vendor_name.to_str().ok_or_else(|| {
            format!(
                "device vendor is not valid UTF-8: {}",
                vendor.path().display()
            )
        })?;
        let vendor_path = vendor.path();
        for device in fs::read_dir(&vendor_path)
            .map_err(|err| format!("read {}: {err}", vendor_path.display()))?
        {
            let device =
                device.map_err(|err| format!("read {} entry: {err}", vendor_path.display()))?;
            let path = device.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
                continue;
            }

            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| format!("device config is not valid UTF-8: {}", path.display()))?;
            let id = format!("{vendor_name}/{stem}");
            KernelDevice::parse(&id)?;
            devices.push(id);
        }
    }

    devices.sort();
    Ok(devices)
}
