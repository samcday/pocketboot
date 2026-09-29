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

#[cfg(test)]
mod tests {
    use super::*;

    const EXPRESS_DEVICES: [&str; 2] = [
        "qcom/msm8930-samsung-expressltexx",
        "qcom/msm8960-samsung-expressatt",
    ];

    #[test]
    fn express_devices_have_distinct_ci_artifacts_and_device_source_pins() {
        let root = workspace_root().unwrap();
        let matrix = ci_matrix(&root).unwrap();
        let sources = EXPRESS_DEVICES.map(|id| {
            let device = KernelDevice::parse(id).unwrap();
            let config = config::load_device_config(&root, &device).unwrap();
            let source = config.kernel_source.unwrap();
            assert_eq!(source.scope, config::KernelSourceScope::Device, "{id}");
            assert_eq!(source.identity.id, id);
            assert!(!source.remote.is_empty(), "{id}");
            assert!(!source.sha.is_empty(), "{id}");

            let entries: Vec<_> = matrix.include.iter().filter(|e| e.device == id).collect();
            assert_eq!(entries.len(), 1, "{id}");
            let entry = entries[0];
            assert_eq!(entry.name, id);
            assert_eq!(entry.artifact, format!("bootimg-{}", id.replace('/', "-")));
            assert_eq!(entry.rust_targets, "armv7-unknown-linux-musleabihf");
            assert!(entry.bootimg, "{id}");
            assert_eq!(entry.sha, source.sha, "{id}");
            source
        });
        assert_ne!(sources[0].remote, sources[1].remote);
        assert_ne!(sources[0].sha, sources[1].sha);
    }

    #[test]
    fn express_display_paths_include_their_driver_dependencies() {
        let root = workspace_root().unwrap();
        for (id, symbols) in [
            (EXPRESS_DEVICES[0], &["DRM_SIMPLEDRM"][..]),
            (
                EXPRESS_DEVICES[1],
                &[
                    "DRM_MSM",
                    "DRM_MSM_MDP4",
                    "DRM_MSM_DSI",
                    "DRM_MSM_DSI_28NM_8960_PHY",
                    "DRM_PANEL_MAGNACHIP_AMS452GP32",
                    "MSM_IOMMU",
                    "INTERCONNECT",
                    "INTERCONNECT_QCOM",
                    "INTERCONNECT_QCOM_MSM8960",
                ][..],
            ),
        ] {
            let device = KernelDevice::parse(id).unwrap();
            let config = config::load_device_config(&root, &device).unwrap();
            let contents = config.kconfig_contents().unwrap();
            for symbol in symbols {
                let expected = format!("CONFIG_{symbol}=y");
                assert!(
                    contents.lines().any(|line| line == expected),
                    "{id}: {expected}"
                );
            }
        }
    }

    #[test]
    fn express_devices_keep_their_arm32_appended_dtb_boot_layouts() {
        let root = workspace_root().unwrap();
        for (id, dtb, tags, ramdisk) in [
            (
                EXPRESS_DEVICES[0],
                "qcom-msm8930-samsung-expressltexx",
                0x02000000,
                0x02200000,
            ),
            (
                EXPRESS_DEVICES[1],
                "qcom-msm8960-samsung-expressatt",
                0x00000100,
                0x01500000,
            ),
        ] {
            let device = KernelDevice::parse(id).unwrap();
            let config = config::load_device_config(&root, &device).unwrap();
            assert_eq!(config.kernel.arch.as_deref(), Some("arm"), "{id}");
            assert_eq!(config.kernel.image.as_deref(), Some("zImage"), "{id}");
            assert_eq!(config.kernel.dtb_stem.as_deref(), Some(dtb), "{id}");
            let bootimg = config.bootimg.unwrap();
            assert_eq!(bootimg.kernel_image, "zImage", "{id}");
            assert_eq!(bootimg.header_version, 0, "{id}");
            assert_eq!(bootimg.page_size, 2048, "{id}");
            assert_eq!(bootimg.base, 0x80200000, "{id}");
            assert_eq!(bootimg.kernel_offset, 0x00008000, "{id}");
            assert_eq!(bootimg.tags_offset, tags, "{id}");
            assert_eq!(bootimg.ramdisk_offset, ramdisk, "{id}");
            assert_eq!(bootimg.ramdisk_size, u32::from(id == EXPRESS_DEVICES[1]));
            assert!(bootimg.append_dtb, "{id}");
            assert!(bootimg.preboot.is_none(), "{id}");
            assert!(bootimg.qcdt.is_none(), "{id}");
            assert!(bootimg.dtbh.is_none(), "{id}");
        }
    }

    #[test]
    fn express_merged_kconfig_keeps_boot_basics_without_arm64_or_smp() {
        let root = workspace_root().unwrap();
        for id in EXPRESS_DEVICES {
            let device = KernelDevice::parse(id).unwrap();
            let config = config::load_device_config(&root, &device).unwrap();
            let contents = config.kconfig_contents().unwrap();
            let lines: Vec<_> = contents.lines().collect();
            for symbol in [
                "ARCH_MULTI_V7",
                "ARCH_QCOM",
                "ARM_APPENDED_DTB",
                "ARM_ATAG_DTB_COMPAT",
                "BLK_DEV_INITRD",
                "DEVTMPFS",
                "KEXEC",
                "MMC",
                "MMC_BLOCK",
                "MMC_ARMMMCI",
                "MMC_QCOM_DML",
                "EXT4_FS",
                "USB_GADGET",
                "USB_CONFIGFS",
                "USB_CONFIGFS_F_FS",
                "USB_CONFIGFS_MASS_STORAGE",
                "USB_CHIPIDEA_UDC",
                "USB_CHIPIDEA_MSM",
                "PHY_QCOM_USB_HS",
                "INPUT",
                "INPUT_EVDEV",
                "KEYBOARD_GPIO",
                "INPUT_PMIC8XXX_PWRKEY",
                "TOUCHSCREEN_ATMEL_MXT",
                "KEYBOARD_TM2_TOUCHKEY",
            ] {
                let expected = format!("CONFIG_{symbol}=y");
                assert!(
                    lines.contains(&expected.as_str()),
                    "{id}: missing {expected}"
                );
            }
            assert!(lines.contains(&"# CONFIG_SMP is not set"), "{id}");
            for symbol in [
                "ARM64",
                "KEXEC_FILE",
                "RELOCATABLE",
                "RANDOMIZE_BASE",
                "HOTPLUG_CPU",
                "NR_CPUS",
                "DRM_PANIC_SCREEN_QR_CODE",
            ] {
                assert!(
                    !contents.contains(&format!("CONFIG_{symbol}")),
                    "{id}: unexpected {symbol}"
                );
            }
        }
    }
}
