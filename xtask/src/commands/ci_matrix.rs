use std::{fs, path::Path};

use serde::Serialize;

use crate::Result;

use super::{
    KernelDevice, config, cpio::DEFAULT_TARGET, preboot::DEFAULT_TARGET as PREBOOT_TARGET,
    workspace_root,
};

#[derive(clap::Args, Debug)]
pub(crate) struct CiMatrixArgs {}

#[derive(Serialize)]
struct CiMatrix {
    include: Vec<CiMatrixEntry>,
}

#[derive(Serialize)]
struct CiMatrixEntry {
    name: String,
    device: String,
    artifact: String,
    cache: String,
    sha: String,
    rust_targets: String,
    rust_cache: String,
    bootimg: bool,
}

pub(crate) fn run(_args: CiMatrixArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let matrix = ci_matrix(&workspace_root)?;
    let json = serde_json::to_string(&matrix).map_err(|err| format!("encode CI matrix: {err}"))?;
    println!("{json}");
    Ok(())
}

fn ci_matrix(workspace_root: &Path) -> Result<CiMatrix> {
    let mut include = Vec::new();

    for device in configured_devices(workspace_root)? {
        let device_config = config::load_device_config(workspace_root, &device)?;
        let Some(source) = &device_config.kernel_source else {
            continue;
        };
        let device_id = device.id();
        let cpio_target = device_config
            .cpio
            .target
            .clone()
            .unwrap_or_else(|| DEFAULT_TARGET.to_string());
        let preboot = device_config
            .bootimg
            .as_ref()
            .and_then(|bootimg| bootimg.preboot.as_ref())
            .is_some();
        let rust_targets = rust_targets(&cpio_target, preboot);
        let rust_cache = sanitize(&rust_targets);
        include.push(CiMatrixEntry {
            name: device_id.clone(),
            device: device_id.clone(),
            artifact: format!("bootimg-{}", sanitize(&device_id)),
            cache: sanitize(&device_id),
            sha: source.sha.clone(),
            rust_targets,
            rust_cache,
            bootimg: device_config.bootimg.is_some(),
        });
    }

    Ok(CiMatrix { include })
}

fn rust_targets(cpio_target: &str, preboot: bool) -> String {
    if preboot && cpio_target != PREBOOT_TARGET {
        format!("{cpio_target},{PREBOOT_TARGET}")
    } else {
        cpio_target.to_string()
    }
}

fn configured_devices(workspace_root: &Path) -> Result<Vec<KernelDevice>> {
    let root = workspace_root.join("configs/device");
    let mut devices = Vec::new();
    for vendor in read_dir(&root, "device config vendor directory")? {
        if !vendor
            .file_type()
            .map_err(|err| format!("read file type for {}: {err}", vendor.path().display()))?
            .is_dir()
        {
            continue;
        }
        let vendor_name = vendor
            .file_name()
            .into_string()
            .map_err(|name| format!("device vendor is not valid UTF-8: {name:?}"))?;
        for device in read_dir(&vendor.path(), "device config directory")? {
            if !device
                .file_type()
                .map_err(|err| format!("read file type for {}: {err}", device.path().display()))?
                .is_file()
            {
                continue;
            }
            let path = device.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| {
                    format!("device config path is not valid UTF-8: {}", path.display())
                })?;
            devices.push(KernelDevice::parse(&format!("{vendor_name}/{stem}"))?);
        }
    }
    devices.sort_by_key(KernelDevice::id);
    Ok(devices)
}

fn read_dir(path: &Path, description: &str) -> Result<Vec<fs::DirEntry>> {
    let entries = fs::read_dir(path)
        .map_err(|err| format!("read {description} {}: {err}", path.display()))?;
    entries
        .map(|entry| entry.map_err(|err| format!("read {description} {}: {err}", path.display())))
        .collect()
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}
