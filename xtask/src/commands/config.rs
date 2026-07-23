use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use toml::{Value, map::Map};

use crate::Result;

use super::{
    FeatureSet, KernelDevice, cpio::validate_slint_scale_factor, ensure_file, validate_feature,
};

pub(super) const DEFAULT_BOOTIMG_KERNEL_IMAGE: &str = "Image.gz";

const DEFAULT_BOOTIMG_HEADER_VERSION: u32 = 0;
const DEFAULT_BOOTIMG_PAGE_SIZE: u32 = 2048;
const DEFAULT_BOOTIMG_BASE: u64 = 0x10000000;
const DEFAULT_BOOTIMG_KERNEL_OFFSET: u64 = 0x00008000;
const DEFAULT_BOOTIMG_RAMDISK_OFFSET: u64 = 0x01000000;
const DEFAULT_BOOTIMG_SECOND_OFFSET: u64 = 0x00f00000;
const DEFAULT_BOOTIMG_TAGS_OFFSET: u64 = 0x00000100;
const DEFAULT_BOOTIMG_DTB_OFFSET: u64 = 0x01f00000;
const DTBH_PLATFORM_CODE: u32 = 0x50a6;
const DTBH_SUBTYPE_CODE: u32 = 0x217584da;

#[derive(Debug)]
pub(super) struct DeviceConfig {
    pub(super) device_path: PathBuf,
    pub(super) features: FeatureSet,
    pub(super) kernel_source: Option<KernelSource>,
    pub(super) kernel: KernelConfig,
    pub(super) cpio: CpioConfig,
    pub(super) bootimg: Option<BootImgConfig>,
    kconfig: BTreeMap<String, KconfigValue>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum KernelSourceScope {
    Default,
    Soc,
    Device,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct KernelSource {
    pub(super) scope: KernelSourceScope,
    pub(super) identity: KernelSourceIdentity,
    pub(super) remote: String,
    pub(super) sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct KernelSourceIdentity {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) tree_name: String,
    pub(super) tree_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KernelSourceLayer {
    remote: String,
    sha: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct KernelConfig {
    pub(super) arch: Option<String>,
    pub(super) image: Option<String>,
    pub(super) image_path: Option<PathBuf>,
    pub(super) dtb_stem: Option<String>,
    pub(super) dtb: Option<bool>,
    pub(super) dtbo_mask_manifest: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CpioConfig {
    pub(super) target: Option<String>,
    pub(super) slint_scale_factor: Option<f32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct BootImgConfig {
    pub(super) header_version: u32,
    pub(super) page_size: u32,
    pub(super) kernel_image: String,
    pub(super) base: u64,
    pub(super) kernel_offset: u64,
    pub(super) ramdisk_offset: u64,
    pub(super) second_offset: u64,
    pub(super) tags_offset: u64,
    pub(super) dtb_offset: u64,
    #[serde(default)]
    pub(super) board: String,
    #[serde(default)]
    pub(super) cmdline: String,
    #[serde(default)]
    pub(super) ramdisk_size: u32,
    #[serde(default)]
    pub(super) append_seandroid_enforce: bool,
    #[serde(default)]
    pub(super) append_dtb: bool,
    pub(super) preboot: Option<PrebootConfig>,
    pub(super) qcdt: Option<QcdtConfig>,
    pub(super) dtbh: Option<DtbhConfig>,
}

impl Default for BootImgConfig {
    fn default() -> Self {
        Self {
            header_version: DEFAULT_BOOTIMG_HEADER_VERSION,
            page_size: DEFAULT_BOOTIMG_PAGE_SIZE,
            kernel_image: default_bootimg_kernel_image(),
            base: DEFAULT_BOOTIMG_BASE,
            kernel_offset: DEFAULT_BOOTIMG_KERNEL_OFFSET,
            ramdisk_offset: DEFAULT_BOOTIMG_RAMDISK_OFFSET,
            second_offset: DEFAULT_BOOTIMG_SECOND_OFFSET,
            tags_offset: DEFAULT_BOOTIMG_TAGS_OFFSET,
            dtb_offset: DEFAULT_BOOTIMG_DTB_OFFSET,
            board: String::new(),
            cmdline: String::new(),
            ramdisk_size: 0,
            append_seandroid_enforce: false,
            append_dtb: false,
            preboot: None,
            qcdt: None,
            dtbh: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PrebootConfig {
    pub(super) load_addr: u64,
    #[serde(default = "default_preboot_payload_align")]
    pub(super) payload_align: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QcdtConfig {
    pub(super) entries: Vec<QcdtEntry>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QcdtEntry {
    pub(super) msm_id: [u32; 2],
    pub(super) board_id: [u32; 2],
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DtbhConfig {
    #[serde(default = "default_dtbh_platform")]
    pub(super) platform: u32,
    #[serde(default = "default_dtbh_subtype")]
    pub(super) subtype: u32,
    pub(super) entries: Vec<DtbhEntry>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DtbhEntry {
    pub(super) chip: u32,
    pub(super) hw_rev: u32,
    pub(super) hw_rev_end: u32,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigLayer {
    #[serde(default)]
    #[serde(rename = "name")]
    _name: Option<String>,
    #[serde(default)]
    features: BTreeMap<String, bool>,
    #[serde(default)]
    kconfig: BTreeMap<String, Value>,
    #[serde(default)]
    kernel: KernelConfig,
    #[serde(default, rename = "kernel-source")]
    kernel_source: Option<KernelSourceLayer>,
    #[serde(default)]
    cpio: CpioConfig,
    bootimg: Option<BootImgConfig>,
}

struct ConfigLayerEntry {
    scope: KernelSourceScope,
    identity: KernelSourceIdentity,
    layer: ConfigLayer,
}

#[derive(Clone, Debug, Default)]
struct LayerKconfig {
    base: BTreeMap<String, KconfigValue>,
    features: BTreeMap<String, BTreeMap<String, KconfigValue>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum KconfigValue {
    Bool(bool),
    Module,
    Integer(i64),
    Hex(i64),
    Omit,
    Raw(String),
    String(String),
}

impl DeviceConfig {
    pub(super) fn kconfig_contents(&self) -> Result<String> {
        kconfig_contents(&self.kconfig)
    }
}

pub(super) fn load_device_config(
    workspace_root: &Path,
    device: &KernelDevice,
) -> Result<DeviceConfig> {
    let common_path = workspace_root.join("configs/pocketboot.toml");
    let soc_path = workspace_root
        .join("configs/soc")
        .join(&device.vendor)
        .join(format!("{}.toml", device.soc));
    let device_path = workspace_root
        .join("configs/device")
        .join(&device.vendor)
        .join(format!("{}.toml", device.stem));

    let common = load_config_layer(&common_path, "common pocketboot config")?;
    let soc_layer = load_config_layer(&soc_path, "SoC config")?;
    let device_layer = load_config_layer(&device_path, "device config")?;
    let layers = [
        ConfigLayerEntry {
            scope: KernelSourceScope::Default,
            identity: default_kernel_source_identity(),
            layer: common,
        },
        ConfigLayerEntry {
            scope: KernelSourceScope::Soc,
            identity: soc_kernel_source_identity(&soc_path, &device.vendor, &device.soc)?,
            layer: soc_layer,
        },
        ConfigLayerEntry {
            scope: KernelSourceScope::Device,
            identity: device_kernel_source_identity(device),
            layer: device_layer,
        },
    ];

    let merged = merge_layers(&layers)?;

    Ok(DeviceConfig {
        device_path,
        features: merged.features,
        kernel_source: merged.kernel_source,
        kernel: merged.kernel,
        cpio: merged.cpio,
        bootimg: merged.bootimg,
        kconfig: merged.kconfig,
    })
}

struct MergedConfig {
    features: FeatureSet,
    kernel_source: Option<KernelSource>,
    kernel: KernelConfig,
    cpio: CpioConfig,
    bootimg: Option<BootImgConfig>,
    kconfig: BTreeMap<String, KconfigValue>,
}

fn merge_layers(layers: &[ConfigLayerEntry]) -> Result<MergedConfig> {
    let enabled_features = merge_features(layers)?;
    let mut features = FeatureSet::default();
    for feature in &enabled_features {
        features.add(feature)?;
    }

    let mut kernel = KernelConfig::default();
    let mut kernel_source = None;
    let mut cpio = CpioConfig::default();
    let mut bootimg = None;
    let mut kconfig = BTreeMap::new();
    for entry in layers {
        if let Some(source) = &entry.layer.kernel_source {
            kernel_source = Some(parse_kernel_source(
                entry.scope,
                entry.identity.clone(),
                source,
            )?);
        }
        merge_kernel(&mut kernel, &entry.layer.kernel);
        merge_cpio(&mut cpio, &entry.layer.cpio);
        if let Some(layer_bootimg) = &entry.layer.bootimg {
            bootimg = Some(layer_bootimg.clone());
        }

        let layer_kconfig = parse_kconfig_layer(&entry.layer)?;
        for (symbol, value) in layer_kconfig.base {
            kconfig.insert(symbol, value);
        }
        for feature in &enabled_features {
            if let Some(feature_kconfig) = layer_kconfig.features.get(feature) {
                for (symbol, value) in feature_kconfig {
                    kconfig.insert(symbol.clone(), value.clone());
                }
            }
        }
    }
    validate_slint_scale_factor(cpio.slint_scale_factor)?;

    Ok(MergedConfig {
        features,
        kernel_source,
        kernel,
        cpio,
        bootimg,
        kconfig,
    })
}

fn load_config_layer(path: &Path, description: &str) -> Result<ConfigLayer> {
    ensure_file(path, description)?;
    let contents =
        fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    toml::from_str(&contents).map_err(|err| format!("parse {}: {err}", path.display()))
}

fn merge_features(layers: &[ConfigLayerEntry]) -> Result<BTreeSet<String>> {
    let mut merged = BTreeMap::new();
    for entry in layers {
        for (feature, enabled) in &entry.layer.features {
            validate_feature(feature)?;
            merged.insert(feature.clone(), *enabled);
        }
    }
    Ok(merged
        .into_iter()
        .filter_map(|(feature, enabled)| enabled.then_some(feature))
        .collect())
}

fn kconfig_contents(kconfig: &BTreeMap<String, KconfigValue>) -> Result<String> {
    let mut contents = String::new();
    contents.push_str("# Generated by cargo xtask kernel from pocketboot TOML configs.\n");
    for (symbol, value) in kconfig {
        write_kconfig_line(&mut contents, symbol, value)?;
    }
    Ok(contents)
}

fn parse_kernel_source(
    scope: KernelSourceScope,
    identity: KernelSourceIdentity,
    source: &KernelSourceLayer,
) -> Result<KernelSource> {
    if source.remote.is_empty() {
        return Err("kernel-source remote must not be empty".to_string());
    }
    if source.sha.is_empty() {
        return Err("kernel-source sha must not be empty".to_string());
    }
    if !source.sha.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(format!(
            "kernel-source sha must be hexadecimal: {}",
            source.sha
        ));
    }

    Ok(KernelSource {
        scope,
        identity,
        remote: source.remote.clone(),
        sha: source.sha.clone(),
    })
}

fn default_kernel_source_identity() -> KernelSourceIdentity {
    KernelSourceIdentity {
        id: "default".to_string(),
        label: "default devices".to_string(),
        tree_name: "pocketboot".to_string(),
        tree_path: PathBuf::from("pocketboot"),
    }
}

fn soc_kernel_source_identity(
    soc_path: &Path,
    vendor: &str,
    configured_soc: &str,
) -> Result<KernelSourceIdentity> {
    let soc = canonical_config_stem(soc_path)?.unwrap_or_else(|| configured_soc.to_string());
    validate_feature(vendor)?;
    validate_feature(&soc)?;
    let id = format!("{vendor}/{soc}");
    Ok(KernelSourceIdentity {
        id: id.clone(),
        label: format!("{id} devices"),
        tree_name: soc.clone(),
        tree_path: PathBuf::from(soc),
    })
}

fn device_kernel_source_identity(device: &KernelDevice) -> KernelSourceIdentity {
    let id = device.id();
    KernelSourceIdentity {
        id: id.clone(),
        label: id,
        tree_name: device.stem.clone(),
        tree_path: PathBuf::from(&device.soc).join(&device.stem),
    }
}

fn canonical_config_stem(path: &Path) -> Result<Option<String>> {
    let canonical = fs::canonicalize(path)
        .map_err(|err| format!("canonicalize config path {}: {err}", path.display()))?;
    canonical
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| Ok(stem.to_string()))
        .transpose()
}

fn merge_kernel(merged: &mut KernelConfig, layer: &KernelConfig) {
    if layer.arch.is_some() {
        merged.arch = layer.arch.clone();
    }
    if layer.image.is_some() {
        merged.image = layer.image.clone();
    }
    if layer.image_path.is_some() {
        merged.image_path = layer.image_path.clone();
    }
    if layer.dtb_stem.is_some() {
        merged.dtb_stem = layer.dtb_stem.clone();
    }
    if layer.dtb.is_some() {
        merged.dtb = layer.dtb;
    }
    if layer.dtbo_mask_manifest.is_some() {
        merged.dtbo_mask_manifest = layer.dtbo_mask_manifest.clone();
    }
}

fn merge_cpio(merged: &mut CpioConfig, layer: &CpioConfig) {
    if layer.target.is_some() {
        merged.target = layer.target.clone();
    }
    if layer.slint_scale_factor.is_some() {
        merged.slint_scale_factor = layer.slint_scale_factor;
    }
}

fn parse_kconfig_layer(layer: &ConfigLayer) -> Result<LayerKconfig> {
    let mut parsed = LayerKconfig::default();
    for (key, value) in &layer.kconfig {
        if let Value::Table(table) = value {
            if !is_structured_kconfig_value(table) {
                validate_feature(key)?;
                parsed
                    .features
                    .insert(key.clone(), parse_kconfig_symbols(key, table)?);
                continue;
            }
        }

        validate_kconfig_symbol(key)?;
        parsed
            .base
            .insert(key.clone(), parse_kconfig_value(key, value)?);
    }
    Ok(parsed)
}

fn parse_kconfig_symbols(
    feature: &str,
    table: &Map<String, Value>,
) -> Result<BTreeMap<String, KconfigValue>> {
    let mut parsed = BTreeMap::new();
    for (symbol, value) in table {
        validate_kconfig_symbol(symbol)?;
        parsed.insert(
            symbol.clone(),
            parse_kconfig_value(symbol, value)
                .map_err(|err| format!("[kconfig.{feature}] {err}"))?,
        );
    }
    Ok(parsed)
}

fn parse_kconfig_value(symbol: &str, value: &Value) -> Result<KconfigValue> {
    match value {
        Value::Boolean(value) => Ok(KconfigValue::Bool(*value)),
        Value::Integer(value) => Ok(KconfigValue::Integer(*value)),
        Value::String(value) if matches!(value.as_str(), "m" | "mod") => Ok(KconfigValue::Module),
        Value::String(value) if value == "omit" => Ok(KconfigValue::Omit),
        Value::String(_) => Err(format!(
            "{symbol}: string kconfig values must be \"m\", \"mod\", \"omit\", or an inline table like {{ string = \"...\" }} / {{ raw = \"...\" }}"
        )),
        Value::Table(table) => parse_structured_kconfig_value(symbol, table),
        Value::Float(_) | Value::Datetime(_) | Value::Array(_) => Err(format!(
            "{symbol}: unsupported kconfig value type; expected bool, integer, \"mod\", or a structured inline table"
        )),
    }
}

fn parse_structured_kconfig_value(
    symbol: &str,
    table: &Map<String, Value>,
) -> Result<KconfigValue> {
    if table.len() != 1 {
        return Err(format!(
            "{symbol}: structured kconfig values must contain exactly one of string, raw, or hex"
        ));
    }
    let (kind, value) = table.iter().next().expect("checked len");
    match (kind.as_str(), value) {
        ("string", Value::String(value)) => Ok(KconfigValue::String(value.clone())),
        ("raw", Value::String(value)) => Ok(KconfigValue::Raw(value.clone())),
        ("hex", Value::Integer(value)) if *value >= 0 => Ok(KconfigValue::Hex(*value)),
        ("string", _) => Err(format!(
            "{symbol}: string kconfig value must be a TOML string"
        )),
        ("raw", _) => Err(format!("{symbol}: raw kconfig value must be a TOML string")),
        ("hex", _) => Err(format!(
            "{symbol}: hex kconfig value must be a non-negative integer"
        )),
        _ => Err(format!(
            "{symbol}: unknown structured kconfig value kind {kind}; expected string, raw, or hex"
        )),
    }
}

fn is_structured_kconfig_value(table: &Map<String, Value>) -> bool {
    table.len() == 1
        && table
            .keys()
            .next()
            .is_some_and(|key| matches!(key.as_str(), "string" | "raw" | "hex"))
}

fn validate_kconfig_symbol(symbol: &str) -> Result<()> {
    if symbol.is_empty() || symbol.starts_with("CONFIG_") {
        return Err(format!(
            "invalid kconfig symbol {symbol}; use the bare symbol name without CONFIG_"
        ));
    }
    if symbol
        .chars()
        .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    {
        Ok(())
    } else {
        Err(format!("invalid kconfig symbol {symbol}"))
    }
}

fn write_kconfig_line(contents: &mut String, symbol: &str, value: &KconfigValue) -> Result<()> {
    match value {
        KconfigValue::Bool(true) => contents.push_str(&format!("CONFIG_{symbol}=y\n")),
        KconfigValue::Bool(false) => contents.push_str(&format!("# CONFIG_{symbol} is not set\n")),
        KconfigValue::Module => contents.push_str(&format!("CONFIG_{symbol}=m\n")),
        KconfigValue::Integer(value) => contents.push_str(&format!("CONFIG_{symbol}={value}\n")),
        KconfigValue::Hex(value) => contents.push_str(&format!("CONFIG_{symbol}=0x{value:x}\n")),
        KconfigValue::Omit => {}
        KconfigValue::Raw(value) => contents.push_str(&format!("CONFIG_{symbol}={value}\n")),
        KconfigValue::String(value) => contents.push_str(&format!(
            "CONFIG_{symbol}=\"{}\"\n",
            escaped_kconfig_string(value)
        )),
    }
    Ok(())
}

fn escaped_kconfig_string(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn default_bootimg_kernel_image() -> String {
    DEFAULT_BOOTIMG_KERNEL_IMAGE.to_string()
}

fn default_preboot_payload_align() -> u64 {
    2 * 1024 * 1024
}

fn default_dtbh_platform() -> u32 {
    DTBH_PLATFORM_CODE
}

fn default_dtbh_subtype() -> u32 {
    DTBH_SUBTYPE_CODE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kconfig_values_render_to_kernel_fragment_syntax() {
        let mut contents = String::new();
        write_kconfig_line(&mut contents, "BLOCK", &KconfigValue::Bool(true)).unwrap();
        write_kconfig_line(&mut contents, "DEBUG_INFO", &KconfigValue::Bool(false)).unwrap();
        write_kconfig_line(&mut contents, "DRM_MSM", &KconfigValue::Module).unwrap();
        write_kconfig_line(&mut contents, "NR_CPUS", &KconfigValue::Integer(8)).unwrap();
        write_kconfig_line(
            &mut contents,
            "MAGIC_SYSRQ_DEFAULT_ENABLE",
            &KconfigValue::Hex(0x80),
        )
        .unwrap();
        write_kconfig_line(
            &mut contents,
            "DRM_PANIC_SCREEN",
            &KconfigValue::String("qr_code".to_string()),
        )
        .unwrap();

        assert_eq!(
            contents,
            "CONFIG_BLOCK=y\n# CONFIG_DEBUG_INFO is not set\nCONFIG_DRM_MSM=m\nCONFIG_NR_CPUS=8\nCONFIG_MAGIC_SYSRQ_DEFAULT_ENABLE=0x80\nCONFIG_DRM_PANIC_SCREEN=\"qr_code\"\n"
        );
    }

    #[test]
    fn empty_bootimg_table_uses_aosp_mkbootimg_defaults() {
        let layer: ConfigLayer = toml::from_str("[bootimg]\n").unwrap();
        let config = layer.bootimg.unwrap();

        assert_eq!(config.header_version, DEFAULT_BOOTIMG_HEADER_VERSION);
        assert_eq!(config.page_size, DEFAULT_BOOTIMG_PAGE_SIZE);
        assert_eq!(config.kernel_image, DEFAULT_BOOTIMG_KERNEL_IMAGE);
        assert_eq!(config.base, DEFAULT_BOOTIMG_BASE);
        assert_eq!(config.kernel_offset, DEFAULT_BOOTIMG_KERNEL_OFFSET);
        assert_eq!(config.ramdisk_offset, DEFAULT_BOOTIMG_RAMDISK_OFFSET);
        assert_eq!(config.second_offset, DEFAULT_BOOTIMG_SECOND_OFFSET);
        assert_eq!(config.tags_offset, DEFAULT_BOOTIMG_TAGS_OFFSET);
        assert_eq!(config.dtb_offset, DEFAULT_BOOTIMG_DTB_OFFSET);
        assert!(config.board.is_empty());
        assert!(config.cmdline.is_empty());
        assert_eq!(config.ramdisk_size, 0);
        assert!(!config.append_seandroid_enforce);
        assert!(!config.append_dtb);
        assert!(config.qcdt.is_none());
        assert!(config.dtbh.is_none());
    }

    #[test]
    fn cpio_layers_merge_target_and_slint_scale_factor_independently() {
        let mut merged = CpioConfig::default();
        merge_cpio(
            &mut merged,
            &CpioConfig {
                target: Some("aarch64-unknown-linux-musl".to_string()),
                slint_scale_factor: Some(1.5),
            },
        );
        merge_cpio(
            &mut merged,
            &CpioConfig {
                target: None,
                slint_scale_factor: Some(2.0),
            },
        );

        assert_eq!(merged.target.as_deref(), Some("aarch64-unknown-linux-musl"));
        assert_eq!(merged.slint_scale_factor, Some(2.0));
    }

    #[test]
    fn all_checked_in_device_configs_load() {
        let workspace_root = super::super::workspace_root().unwrap();
        for device_id in [
            "exynos/exynos7870-j7xelte",
            "qcom/apq8016-sbc",
            "qcom/msm8926-sony-xperia-yukon-eagle",
            "qcom/msm8930-samsung-expressltexx",
            "qcom/msm8916-samsung-a5u-eur",
            "qcom/msm8916-samsung-gt510",
            "qcom/msm8953-xiaomi-daisy",
            "qcom/sdm670-google-sargo",
            "qcom/sdm845-google-crosshatch",
            "qcom/sdm845-oneplus-fajita",
            "qemu/aarch64-virt",
        ] {
            let device = KernelDevice::parse(device_id).unwrap();
            let config = load_device_config(&workspace_root, &device).unwrap();
            let kconfig = config.kconfig_contents().unwrap();
            assert!(kconfig.contains("CONFIG_BLK_DEV_INITRD=y"));
            if device_id == "qcom/apq8016-sbc" {
                assert_eq!(
                    config.kernel_source.as_ref().unwrap().identity.id,
                    "qcom/msm8916"
                );
            }
            if device_id == "qemu/aarch64-virt" {
                assert!(config.features.contains("qemu"));
                assert!(kconfig.contains("CONFIG_USBIP_VUDC=y"));
            }
            if device_id == "qcom/sdm670-google-sargo" {
                assert!(config.features.contains("blob-wrangler"));
                for symbol in ["FW_LOADER", "MD", "BLK_DEV_DM", "DM_ZERO"] {
                    assert!(
                        kconfig.contains(&format!("CONFIG_{symbol}=y")),
                        "missing firmware Kconfig symbol {symbol}"
                    );
                }
            }
        }
    }

    #[test]
    fn a5u_enables_firmware_independent_mdp5_kms() {
        let workspace_root = super::super::workspace_root().unwrap();
        let device = KernelDevice::parse("qcom/msm8916-samsung-a5u-eur").unwrap();
        let config = load_device_config(&workspace_root, &device).unwrap();
        let kconfig = config.kconfig_contents().unwrap();

        for symbol in [
            "IOMMU_SUPPORT",
            "QCOM_IOMMU",
            "DRM_MSM",
            "DRM_MSM_MDP5",
            "DRM_MSM_DSI",
            "DRM_MSM_DSI_28NM_PHY",
            "DRM_PANEL_SAMSUNG_EA8061V_AMS497EE01",
        ] {
            assert!(
                kconfig.contains(&format!("CONFIG_{symbol}=y\n")),
                "missing built-in CONFIG_{symbol}:\n{kconfig}"
            );
        }
        assert!(
            kconfig.contains("# CONFIG_DRM_SIMPLEDRM is not set\n"),
            "CONFIG_DRM_SIMPLEDRM must be disabled:\n{kconfig}"
        );
    }

    #[test]
    fn a5u_overlay_keeps_display_and_gpu_isolation_boundaries() {
        let workspace_root = super::super::workspace_root().unwrap();
        let overlay = fs::read_to_string(
            workspace_root.join("configs/dt-overlays/qcom/msm8916-samsung-a5u-eur.dtso"),
        )
        .unwrap();

        for enabled_path in ["/soc@0/display-subsystem@1a00000", "/soc@0/iommu@1ef0000"] {
            assert!(
                !overlay.contains(enabled_path),
                "display path must no longer be disabled: {enabled_path}"
            );
        }
        for disabled_path in ["/soc@0/iommu@1f08000", "/soc@0/gpu@1c00000"] {
            let fragment = format!("&{{{disabled_path}}} {{\n\tstatus = \"disabled\";\n}};");
            assert!(
                overlay.contains(&fragment),
                "GPU isolation path must remain disabled: {disabled_path}"
            );
        }
    }

    #[test]
    fn gt510_enables_firmware_independent_mdp5_kms() {
        let workspace_root = super::super::workspace_root().unwrap();
        let device = KernelDevice::parse("qcom/msm8916-samsung-gt510").unwrap();
        let config = load_device_config(&workspace_root, &device).unwrap();
        let kconfig = config.kconfig_contents().unwrap();

        for symbol in [
            "IOMMU_SUPPORT",
            "QCOM_IOMMU",
            "DRM_MSM",
            "DRM_MSM_MDP5",
            "DRM_MSM_DSI",
            "DRM_MSM_DSI_28NM_PHY",
            "BACKLIGHT_CLASS_DEVICE",
            "DRM_PANEL_SAMSUNG_S6D7AA0",
        ] {
            assert!(
                kconfig.contains(&format!("CONFIG_{symbol}=y\n")),
                "missing built-in CONFIG_{symbol}:\n{kconfig}"
            );
        }
        assert!(
            kconfig.contains("# CONFIG_DRM_SIMPLEDRM is not set\n"),
            "CONFIG_DRM_SIMPLEDRM must be disabled:\n{kconfig}"
        );
        let bootimg = config.bootimg.as_ref().unwrap();
        for argument in ["msm.skip_gpu=1", "msm.separate_gpu_kms=1"] {
            assert!(
                bootimg
                    .cmdline
                    .split_ascii_whitespace()
                    .any(|arg| arg == argument),
                "missing kernel argument {argument}: {}",
                bootimg.cmdline
            );
        }
    }

    #[test]
    fn gt510_overlay_keeps_display_and_gpu_isolation_boundaries() {
        let workspace_root = super::super::workspace_root().unwrap();
        let overlay = fs::read_to_string(
            workspace_root.join("configs/dt-overlays/qcom/msm8916-samsung-gt510.dtso"),
        )
        .unwrap();

        for enabled_path in ["/soc@0/display-subsystem@1a00000", "/soc@0/iommu@1ef0000"] {
            assert!(
                !overlay.contains(enabled_path),
                "display path must not be disabled: {enabled_path}"
            );
        }
        for disabled_path in ["/soc@0/iommu@1f08000", "/soc@0/gpu@1c00000"] {
            let fragment = format!("&{{{disabled_path}}} {{\n\tstatus = \"disabled\";\n}};");
            assert!(
                overlay.contains(&fragment),
                "GPU isolation path must remain disabled: {disabled_path}"
            );
        }
    }

    #[test]
    fn crosshatch_enables_firmware_independent_msm_kms_and_touch() {
        let workspace_root = super::super::workspace_root().unwrap();
        let device = KernelDevice::parse("qcom/sdm845-google-crosshatch").unwrap();
        let config = load_device_config(&workspace_root, &device).unwrap();
        let kconfig = config.kconfig_contents().unwrap();

        assert_eq!(config.cpio.slint_scale_factor, Some(2.0));

        for symbol in [
            "DRM_MSM",
            "DRM_MSM_DPU",
            "DRM_MSM_DSI",
            "DRM_MSM_DSI_10NM_PHY",
            "DRM_MIPI_DSI",
            "DRM_DISPLAY_DSC_HELPER",
            "SDM_DISPCC_845",
            "REGULATOR_QCOM_REFGEN",
            "BACKLIGHT_CLASS_DEVICE",
            "DRM_PANEL_SAMSUNG_S6E3HA8",
            "TOUCHSCREEN_S6SY761",
            // Existing boot/storage paths must survive the display override.
            "USB_DWC3",
            "USB_DWC3_GADGET",
            "USB_DWC3_QCOM",
            "PHY_QCOM_QUSB2",
            "PHY_QCOM_QMP_COMBO",
            "SCSI_UFSHCD",
            "SCSI_UFSHCD_PLATFORM",
            "SCSI_UFS_QCOM",
        ] {
            assert!(
                kconfig.contains(&format!("CONFIG_{symbol}=y\n")),
                "missing built-in CONFIG_{symbol}:\n{kconfig}"
            );
        }
        for symbol in [
            "DRM_SIMPLEDRM",
            "SDM_GPUCC_845",
            "USB_QCOM_EUD",
            "REGULATOR_QCOM_LABIBB",
        ] {
            assert!(
                kconfig.contains(&format!("# CONFIG_{symbol} is not set\n")),
                "CONFIG_{symbol} must be disabled:\n{kconfig}"
            );
        }
        assert!(kconfig.contains("CONFIG_MAGIC_SYSRQ_DEFAULT_ENABLE=0x88\n"));

        let cmdline = &config.bootimg.as_ref().unwrap().cmdline;
        assert!(
            cmdline
                .split_ascii_whitespace()
                .any(|arg| arg == "msm.skip_gpu=1")
        );
        assert!(
            cmdline
                .split_ascii_whitespace()
                .any(|arg| arg == "msm.separate_gpu_kms=1")
        );
        for expected in [
            "earlycon",
            "console=ttyMSM0,115200n8",
            "pocketboot.log=info",
            "pocketboot.drm_page_flips=16",
            "loglevel=8",
            "ignore_loglevel",
            "drm.debug=0x6",
        ] {
            assert!(cmdline.split_ascii_whitespace().any(|arg| arg == expected));
        }
    }

    #[test]
    fn crosshatch_overlay_keeps_display_and_gpu_isolation_boundaries() {
        let workspace_root = super::super::workspace_root().unwrap();
        let overlay = fs::read_to_string(
            workspace_root.join("configs/dt-overlays/qcom/sdm845-google-crosshatch.dtso"),
        )
        .unwrap();

        for enabled_path in [
            "/soc@0/display-subsystem@ae00000",
            "/soc@0/clock-controller@af00000",
        ] {
            assert!(
                !overlay.contains(enabled_path),
                "display path must no longer be disabled: {enabled_path}"
            );
        }
        for disabled_path in [
            "/soc@0/clock-controller@5090000",
            "/soc@0/gpu@5000000",
            "/soc@0/iommu@5040000",
            "/soc@0/gmu@506a000",
        ] {
            let fragment = format!("&{{{disabled_path}}} {{\n\tstatus = \"disabled\";\n}};");
            assert!(
                overlay.contains(&fragment),
                "GPU isolation path must remain disabled: {disabled_path}"
            );
        }
    }
}
