use std::{collections::BTreeMap, fs, path::Path};

use serde::Serialize;

use crate::Result;

use super::{
    KernelDevice,
    config::{self, KernelSource},
    cpio::DEFAULT_TARGET,
    workspace_root,
};

#[derive(clap::Args, Debug)]
pub(crate) struct KernelMatrixArgs {}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct KernelSourceKey {
    remote: String,
    sha: String,
}

#[derive(Debug)]
struct KernelMatrixGroup {
    name: String,
    remote: String,
    sha: String,
    cpio_targets: Vec<String>,
    devices: Vec<String>,
    bootimg_devices: Vec<String>,
}

#[derive(Serialize)]
struct KernelMatrix {
    include: Vec<KernelMatrixEntry>,
}

#[derive(Serialize)]
struct KernelMatrixEntry {
    name: String,
    artifact: String,
    remote: String,
    sha: String,
    cpio_targets: Vec<String>,
    devices: Vec<String>,
    bootimg_devices: Vec<String>,
}

pub(crate) fn run(_args: KernelMatrixArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let matrix = kernel_matrix(&workspace_root)?;
    let json =
        serde_json::to_string(&matrix).map_err(|err| format!("encode kernel matrix: {err}"))?;
    println!("{json}");
    Ok(())
}

fn kernel_matrix(workspace_root: &Path) -> Result<KernelMatrix> {
    let mut groups = BTreeMap::<KernelSourceKey, KernelMatrixGroup>::new();

    for device in configured_devices(workspace_root)? {
        let device_config = config::load_device_config(workspace_root, &device)?;
        let Some(source) = &device_config.kernel_source else {
            continue;
        };
        let key = kernel_source_key(source);
        let group = groups.entry(key).or_insert_with(|| KernelMatrixGroup {
            name: kernel_source_name(source),
            remote: source.remote.clone(),
            sha: source.sha.clone(),
            cpio_targets: Vec::new(),
            devices: Vec::new(),
            bootimg_devices: Vec::new(),
        });
        push_unique_sorted(
            &mut group.cpio_targets,
            device_config
                .cpio
                .target
                .clone()
                .unwrap_or_else(|| DEFAULT_TARGET.to_string()),
        );
        let device_id = device.id();
        group.devices.push(device_id.clone());
        if device_config.bootimg.is_some() {
            group.bootimg_devices.push(device_id);
        }
    }

    Ok(KernelMatrix {
        include: groups.into_values().map(kernel_matrix_entry).collect(),
    })
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

fn kernel_source_key(source: &KernelSource) -> KernelSourceKey {
    KernelSourceKey {
        remote: source.remote.clone(),
        sha: source.sha.clone(),
    }
}

fn kernel_matrix_entry(group: KernelMatrixGroup) -> KernelMatrixEntry {
    KernelMatrixEntry {
        artifact: format!("kernel-{}", sanitize(&group.name)),
        name: group.name,
        remote: group.remote,
        sha: group.sha,
        cpio_targets: group.cpio_targets,
        devices: group.devices,
        bootimg_devices: group.bootimg_devices,
    }
}

fn push_unique_sorted(values: &mut Vec<String>, value: String) {
    if values.iter().any(|existing| existing == &value) {
        return;
    }
    values.push(value);
    values.sort();
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

fn kernel_source_name(source: &KernelSource) -> String {
    format!(
        "{}@{}",
        remote_label(&source.remote),
        short_sha(&source.sha)
    )
}

fn remote_label(remote: &str) -> String {
    let remote = remote.trim_end_matches(".git");
    for separator in ["github.com/", "github.com:"] {
        if let Some((_, label)) = remote.split_once(separator) {
            return label.to_string();
        }
    }
    remote
        .rsplit_once('/')
        .map_or(remote, |(_, label)| label)
        .to_string()
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
