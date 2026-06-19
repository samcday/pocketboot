use std::{
    ffi::CString,
    fs::{self, File},
    io,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

const ROOT_COMPATIBLE: &str = "/sys/firmware/devicetree/base/compatible";
const DEBUGFS: &str = "/sys/kernel/debug";
const REGMAP_DEBUGFS: &str = "/sys/kernel/debug/regmap";
const SPMI_DEVICES: &str = "/sys/bus/spmi/devices";

const QCOM_PON_SOFT_RB_SPARE: u32 = 0x8f;
const QCOM_PON_REASON_HARD_RESET: u8 = 1 << 0;
const QCOM_PON_REASON_SMPL: u8 = 1 << 1;
const QCOM_PON_REASON_RTC: u8 = 1 << 2;
const QCOM_PON_REASON_DC_CHARGER: u8 = 1 << 3;
const QCOM_PON_REASON_USB_CHARGER: u8 = 1 << 4;
const QCOM_PON_REASON_PON1: u8 = 1 << 5;
const QCOM_PON_REASON_CABLE: u8 = 1 << 6;
const QCOM_PON_REASON_KPD: u8 = 1 << 7;
const QCOM_PON_REASON_CHARGER_MASK: u8 = QCOM_PON_REASON_DC_CHARGER
    | QCOM_PON_REASON_USB_CHARGER
    | QCOM_PON_REASON_PON1
    | QCOM_PON_REASON_CABLE;

const QCOM_SPMI_REGMAP_LINE_LEN: usize = 9;

const QCOM_PON_TARGETS: [QcomPonTarget; 2] = [
    QcomPonTarget {
        soc_compatible: "qcom,msm8916",
        pmic_compatible: "qcom,pm8916",
        pon_compatible: "qcom,pm8916-pon",
        generation: QcomPonGeneration::Gen1,
    },
    QcomPonTarget {
        soc_compatible: "qcom,sdm845",
        pmic_compatible: "qcom,pm8998",
        pon_compatible: "qcom,pm8998-pon",
        generation: QcomPonGeneration::Gen2,
    },
];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BootState {
    pub(crate) reboot_mode: Option<RebootMode>,
    pub(crate) hard_reset: Option<bool>,
    pub(crate) power_key: Option<bool>,
    pub(crate) charger: Option<bool>,
    pub(crate) warm_reset: Option<bool>,
    pub(crate) source: Option<BootStateSource>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RebootMode {
    Recovery,
    Bootloader,
    Unknown(u8),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BootStateSource {
    pub(crate) backend: &'static str,
    pub(crate) detail: String,
}

pub(crate) fn detect() -> BootState {
    let root_compatibles = match read_fdt_strings(ROOT_COMPATIBLE) {
        Ok(compatibles) => compatibles,
        Err(err) => {
            return state_with_source(
                "devicetree",
                format!("root-compatible-unavailable path={ROOT_COMPATIBLE} error={err}"),
            );
        }
    };

    let Some(target) = qcom_target_for_root(&root_compatibles) else {
        return state_with_source(
            "devicetree",
            format!(
                "unsupported-root-compatible root={}",
                format_compatibles(&root_compatibles)
            ),
        );
    };

    match detect_qcom_pon(target, &root_compatibles) {
        Ok(state) => state,
        Err(err) => state_with_source(
            "qcom-pon",
            format!(
                "root={} soc={} pmic={} pon={} error={err}",
                format_compatibles(&root_compatibles),
                target.soc_compatible,
                target.pmic_compatible,
                target.pon_compatible
            ),
        ),
    }
}

fn state_with_source(backend: &'static str, detail: String) -> BootState {
    BootState {
        source: Some(BootStateSource { backend, detail }),
        ..BootState::default()
    }
}

fn detect_qcom_pon(
    target: QcomPonTarget,
    root_compatibles: &[String],
) -> Result<BootState, String> {
    ensure_debugfs().map_err(|err| format!("debugfs-unavailable path={DEBUGFS} error={err}"))?;

    let regmap = find_qcom_pon_regmap(target)?;
    let registers = File::open(regmap.regmap_path.join("registers")).map_err(|err| {
        format!(
            "open-regmap-registers spmi={} error={err}",
            regmap.spmi_device
        )
    })?;

    let pon_reason1 = read_qcom_pon_register(
        &registers,
        regmap.pon_base + target.generation.pon_reason1_offset(),
        "pon_reason1",
    )?;
    let warm_reset_reason1 = read_qcom_pon_register(
        &registers,
        regmap.pon_base + target.generation.warm_reset_reason1_offset(),
        "warm_reset_reason1",
    )?;
    let warm_reset_reason2 = target
        .generation
        .warm_reset_reason2_offset()
        .map(|offset| {
            read_qcom_pon_register(&registers, regmap.pon_base + offset, "warm_reset_reason2")
        })
        .transpose()?;
    let soft_rb_spare = read_qcom_pon_register(
        &registers,
        regmap.pon_base + QCOM_PON_SOFT_RB_SPARE,
        "soft_rb_spare",
    )?;

    let reboot_mode = decode_reboot_mode(soft_rb_spare, target.generation);
    let warm_reset = warm_reset_reason1 != 0 || warm_reset_reason2.unwrap_or(0) != 0;

    Ok(BootState {
        reboot_mode,
        hard_reset: Some(pon_reason1 & QCOM_PON_REASON_HARD_RESET != 0),
        power_key: Some(pon_reason1 & QCOM_PON_REASON_KPD != 0),
        charger: Some(pon_reason1 & QCOM_PON_REASON_CHARGER_MASK != 0),
        warm_reset: Some(warm_reset),
        source: Some(BootStateSource {
            backend: "qcom-pon",
            detail: qcom_pon_detail(
                target,
                root_compatibles,
                &regmap,
                QcomPonRaw {
                    pon_reason1,
                    warm_reset_reason1,
                    warm_reset_reason2,
                    soft_rb_spare,
                },
                reboot_mode,
            ),
        }),
    })
}

fn read_qcom_pon_register(file: &File, reg: u32, name: &str) -> Result<u8, String> {
    read_regmap_u8(file, reg).map_err(|err| format!("read-{name} reg=0x{reg:04x} error={err}"))
}

fn find_qcom_pon_regmap(target: QcomPonTarget) -> Result<QcomPonRegmap, String> {
    let entries = fs::read_dir(REGMAP_DEBUGFS)
        .map_err(|err| format!("read-regmap-debugfs path={REGMAP_DEBUGFS} error={err}"))?;
    let mut pmic_candidates = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|err| format!("read-regmap-entry error={err}"))?;
        let regmap_path = entry.path();
        let Some(spmi_device) = regmap_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };

        if !matches!(
            read_trimmed(regmap_path.join("name")).as_deref(),
            Ok("pmic-spmi")
        ) {
            continue;
        }

        let pmic_node = Path::new(SPMI_DEVICES).join(&spmi_device).join("of_node");
        let pmic_compatibles = match read_fdt_strings(pmic_node.join("compatible")) {
            Ok(compatibles) => compatibles,
            Err(err) => {
                pmic_candidates.push(format!("{spmi_device}:compatible-error={err}"));
                continue;
            }
        };

        if !has_compatible(&pmic_compatibles, target.pmic_compatible) {
            continue;
        }
        pmic_candidates.push(format!(
            "{}:{}",
            spmi_device,
            format_compatibles(&pmic_compatibles)
        ));

        let Some(pon_child) = find_pon_child(&pmic_node, target.pon_compatible)? else {
            continue;
        };
        let pon_base = read_first_fdt_u32(pon_child.path.join("reg"))
            .map_err(|err| format!("read-pon-reg path={} error={err}", pon_child.path.display()))?;

        return Ok(QcomPonRegmap {
            spmi_device,
            regmap_path,
            pmic_compatibles,
            pon_compatibles: pon_child.compatibles,
            pon_base,
        });
    }

    Err(format!(
        "no-pon-regmap pmic={} pon={} candidates={}",
        target.pmic_compatible,
        target.pon_compatible,
        if pmic_candidates.is_empty() {
            "none".to_string()
        } else {
            pmic_candidates.join("|")
        }
    ))
}

fn find_pon_child(pmic_node: &Path, pon_compatible: &str) -> Result<Option<PonChild>, String> {
    let entries = fs::read_dir(pmic_node)
        .map_err(|err| format!("read-pmic-of-node path={} error={err}", pmic_node.display()))?;

    for entry in entries {
        let entry = entry.map_err(|err| format!("read-pmic-child error={err}"))?;
        let path = entry.path();
        let compatibles = match read_fdt_strings(path.join("compatible")) {
            Ok(compatibles) => compatibles,
            Err(_) => continue,
        };

        if has_compatible(&compatibles, pon_compatible) {
            return Ok(Some(PonChild { path, compatibles }));
        }
    }

    Ok(None)
}

fn ensure_debugfs() -> io::Result<()> {
    fs::create_dir_all(DEBUGFS)?;
    if Path::new(REGMAP_DEBUGFS).exists() {
        return Ok(());
    }

    let source = cstring("debugfs")?;
    let target = cstring(DEBUGFS)?;
    let fstype = cstring("debugfs")?;
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null::<libc::c_void>(),
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EBUSY) {
            return Err(err);
        }
    }

    if Path::new(REGMAP_DEBUGFS).exists() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{REGMAP_DEBUGFS} not found after mounting debugfs"),
        ))
    }
}

fn cstring(value: &str) -> io::Result<CString> {
    CString::new(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("string contains NUL byte: {value:?}"),
        )
    })
}

fn read_regmap_u8(file: &File, reg: u32) -> io::Result<u8> {
    if reg > 0xffff {
        return Err(invalid_data(format!(
            "register 0x{reg:x} exceeds SPMI range"
        )));
    }

    let mut line = [0u8; QCOM_SPMI_REGMAP_LINE_LEN];
    read_exact_at(
        file,
        &mut line,
        u64::from(reg) * QCOM_SPMI_REGMAP_LINE_LEN as u64,
    )?;
    parse_regmap_u8_line(&line, reg)
}

fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<()> {
    let mut read = 0;
    while read < buffer.len() {
        match file.read_at(&mut buffer[read..], offset + read as u64) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short read from regmap debugfs",
                ));
            }
            Ok(bytes) => read += bytes,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn parse_regmap_u8_line(line: &[u8], reg: u32) -> io::Result<u8> {
    let text = std::str::from_utf8(line).map_err(invalid_data)?;
    let expected_prefix = format!("{reg:04x}: ");
    if !text.starts_with(&expected_prefix) {
        return Err(invalid_data(format!(
            "expected register prefix {expected_prefix:?}, got {:?}",
            text.trim_end()
        )));
    }

    let value_start = expected_prefix.len();
    let value_end = value_start + 2;
    let value = text
        .get(value_start..value_end)
        .ok_or_else(|| invalid_data(format!("short register line {text:?}")))?;
    if value.as_bytes().contains(&b'X') {
        return Err(invalid_data(format!("register 0x{reg:04x} is unreadable")));
    }

    u8::from_str_radix(value, 16).map_err(invalid_data)
}

fn read_trimmed(path: impl AsRef<Path>) -> io::Result<String> {
    fs::read_to_string(path).map(|value| value.trim().to_string())
}

fn read_fdt_strings(path: impl AsRef<Path>) -> io::Result<Vec<String>> {
    fs::read(path).map(|bytes| parse_fdt_strings(&bytes))
}

fn parse_fdt_strings(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .filter_map(|value| std::str::from_utf8(value).ok())
        .map(str::to_string)
        .collect()
}

fn read_first_fdt_u32(path: impl AsRef<Path>) -> io::Result<u32> {
    let bytes = fs::read(path)?;
    let cell = bytes
        .get(..4)
        .ok_or_else(|| invalid_data("missing u32 FDT cell"))?;
    Ok(u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]))
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn qcom_target_for_root(root_compatibles: &[String]) -> Option<QcomPonTarget> {
    QCOM_PON_TARGETS.iter().copied().find(|target| {
        root_compatibles
            .iter()
            .any(|compatible| compatible == target.soc_compatible)
    })
}

fn has_compatible(compatibles: &[String], compatible: &str) -> bool {
    compatibles.iter().any(|value| value == compatible)
}

fn decode_reboot_mode(spare: u8, generation: QcomPonGeneration) -> Option<RebootMode> {
    let value = match generation {
        QcomPonGeneration::Gen1 => (spare & 0xfc) >> 2,
        QcomPonGeneration::Gen2 => (spare & 0xfe) >> 1,
    };

    match value {
        0 => None,
        1 => Some(RebootMode::Recovery),
        2 => Some(RebootMode::Bootloader),
        value => Some(RebootMode::Unknown(value)),
    }
}

fn qcom_pon_detail(
    target: QcomPonTarget,
    root_compatibles: &[String],
    regmap: &QcomPonRegmap,
    raw: QcomPonRaw,
    reboot_mode: Option<RebootMode>,
) -> String {
    let warm_reset_reason2 = raw
        .warm_reset_reason2
        .map(|value| format!(" warm_reset_reason2=0x{value:02x}"))
        .unwrap_or_default();

    format!(
        "root={} spmi={} pmic={} pmic_node={} pon={} pon_node={} gen={} pon_base=0x{:03x} pon_reason1=0x{:02x} pon_reasons={} warm_reset_reason1=0x{:02x}{} soft_rb_spare=0x{:02x} reboot_mode={}",
        format_compatibles(root_compatibles),
        regmap.spmi_device,
        target.pmic_compatible,
        format_compatibles(&regmap.pmic_compatibles),
        target.pon_compatible,
        format_compatibles(&regmap.pon_compatibles),
        target.generation.label(),
        regmap.pon_base,
        raw.pon_reason1,
        format_pon_reason_labels(raw.pon_reason1),
        raw.warm_reset_reason1,
        warm_reset_reason2,
        raw.soft_rb_spare,
        format_reboot_mode(reboot_mode),
    )
}

fn format_compatibles(compatibles: &[String]) -> String {
    if compatibles.is_empty() {
        "none".to_string()
    } else {
        compatibles.join("|")
    }
}

fn format_pon_reason_labels(value: u8) -> String {
    let labels = pon_reason_labels(value);
    if labels.is_empty() {
        "none".to_string()
    } else {
        labels.join("|")
    }
}

fn pon_reason_labels(value: u8) -> Vec<&'static str> {
    let mut labels = Vec::new();
    if value & QCOM_PON_REASON_HARD_RESET != 0 {
        labels.push("hard-reset");
    }
    if value & QCOM_PON_REASON_SMPL != 0 {
        labels.push("smpl");
    }
    if value & QCOM_PON_REASON_RTC != 0 {
        labels.push("rtc");
    }
    if value & QCOM_PON_REASON_DC_CHARGER != 0 {
        labels.push("dc-charger");
    }
    if value & QCOM_PON_REASON_USB_CHARGER != 0 {
        labels.push("usb-charger");
    }
    if value & QCOM_PON_REASON_PON1 != 0 {
        labels.push("pon1/secondary-pmic");
    }
    if value & QCOM_PON_REASON_CABLE != 0 {
        labels.push("cable/external-power");
    }
    if value & QCOM_PON_REASON_KPD != 0 {
        labels.push("pwrkey");
    }
    labels
}

fn format_reboot_mode(mode: Option<RebootMode>) -> String {
    match mode {
        Some(RebootMode::Recovery) => "recovery".to_string(),
        Some(RebootMode::Bootloader) => "bootloader".to_string(),
        Some(RebootMode::Unknown(value)) => format!("unknown({value})"),
        None => "none".to_string(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct QcomPonTarget {
    soc_compatible: &'static str,
    pmic_compatible: &'static str,
    pon_compatible: &'static str,
    generation: QcomPonGeneration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QcomPonGeneration {
    Gen1,
    Gen2,
}

impl QcomPonGeneration {
    fn label(self) -> &'static str {
        match self {
            Self::Gen1 => "gen1",
            Self::Gen2 => "gen2",
        }
    }

    fn pon_reason1_offset(self) -> u32 {
        match self {
            Self::Gen1 => 0x08,
            Self::Gen2 => 0xc0,
        }
    }

    fn warm_reset_reason1_offset(self) -> u32 {
        match self {
            Self::Gen1 => 0x0a,
            Self::Gen2 => 0xc2,
        }
    }

    fn warm_reset_reason2_offset(self) -> Option<u32> {
        match self {
            Self::Gen1 => Some(0x0b),
            Self::Gen2 => None,
        }
    }
}

#[derive(Debug)]
struct QcomPonRegmap {
    spmi_device: String,
    regmap_path: PathBuf,
    pmic_compatibles: Vec<String>,
    pon_compatibles: Vec<String>,
    pon_base: u32,
}

#[derive(Debug)]
struct PonChild {
    path: PathBuf,
    compatibles: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
struct QcomPonRaw {
    pon_reason1: u8,
    warm_reset_reason1: u8,
    warm_reset_reason2: Option<u8>,
    soft_rb_spare: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fdt_strings() {
        assert_eq!(
            parse_fdt_strings(b"oneplus,fajita\0qcom,sdm845\0"),
            vec!["oneplus,fajita", "qcom,sdm845"]
        );
    }

    #[test]
    fn dispatches_msm8916_to_pm8916() {
        let root = vec!["samsung,a5u-eur".to_string(), "qcom,msm8916".to_string()];
        let target = qcom_target_for_root(&root).unwrap();

        assert_eq!(target.pmic_compatible, "qcom,pm8916");
        assert_eq!(target.pon_compatible, "qcom,pm8916-pon");
        assert_eq!(target.generation, QcomPonGeneration::Gen1);
    }

    #[test]
    fn dispatches_sdm845_to_pm8998() {
        let root = vec!["oneplus,fajita".to_string(), "qcom,sdm845".to_string()];
        let target = qcom_target_for_root(&root).unwrap();

        assert_eq!(target.pmic_compatible, "qcom,pm8998");
        assert_eq!(target.pon_compatible, "qcom,pm8998-pon");
        assert_eq!(target.generation, QcomPonGeneration::Gen2);
    }

    #[test]
    fn parses_regmap_debugfs_line() {
        assert_eq!(parse_regmap_u8_line(b"08c0: 20\n", 0x8c0).unwrap(), 0x20);
    }

    #[test]
    fn rejects_wrong_regmap_debugfs_line() {
        let err = parse_regmap_u8_line(b"0808: 20\n", 0x8c0).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn decodes_reboot_modes() {
        assert_eq!(
            decode_reboot_mode(0x04, QcomPonGeneration::Gen1),
            Some(RebootMode::Recovery)
        );
        assert_eq!(
            decode_reboot_mode(0x08, QcomPonGeneration::Gen1),
            Some(RebootMode::Bootloader)
        );
        assert_eq!(
            decode_reboot_mode(0x02, QcomPonGeneration::Gen2),
            Some(RebootMode::Recovery)
        );
        assert_eq!(
            decode_reboot_mode(0x04, QcomPonGeneration::Gen2),
            Some(RebootMode::Bootloader)
        );
    }

    #[test]
    fn labels_pon_reason_bits() {
        assert_eq!(
            pon_reason_labels(0x91),
            vec!["hard-reset", "usb-charger", "pwrkey"]
        );
        assert_eq!(pon_reason_labels(0x20), vec!["pon1/secondary-pmic"]);
    }

    #[test]
    fn classifies_a5u_usb_sample_bits() {
        let pon_reason1 = 0x11;

        assert!(pon_reason1 & QCOM_PON_REASON_HARD_RESET != 0);
        assert!(pon_reason1 & QCOM_PON_REASON_CHARGER_MASK != 0);
        assert_eq!(pon_reason1 & QCOM_PON_REASON_KPD, 0);
    }
}
