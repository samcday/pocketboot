use std::{
    collections::{BTreeMap, BTreeSet},
    env,
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
    pub(super) device: KernelDevice,
    #[arg(value_name = "KERNEL_TREE")]
    pub(super) kernel_tree: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    pub(super) initrd: Option<PathBuf>,
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
const PROCESSED_DTB: &str = "pocketboot.dtb";
const DTBO_MASK_FORMAT: u32 = 1;
const DTBO_MASK_VALIDATION_PROPERTY: &str = "pocketboot,dtbo-mask-target";
const MAINLINE_COMPATIBLE_PROPERTY: &str = "pocketboot,mainline-compatible";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DtboMaskManifest {
    format: u32,
    source: String,
    source_sha256: String,
    dtbo_entries: u32,
    symbols: Vec<String>,
}

#[derive(Debug)]
struct FdtContents {
    nodes: BTreeSet<String>,
    properties: BTreeMap<(String, String), Vec<u8>>,
}

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
    let dtb = target
        .build_dtb
        .then(|| built_kernel_dtb_path(&out_dir, &arch, device, &dtb_stem));
    ensure_file(&image, "kernel image")?;
    if let Some(dtb) = &dtb {
        ensure_file(dtb, "device tree blob")?;
    }
    let dtb = dtb
        .as_ref()
        .map(|dtb| {
            process_device_dtb(
                &workspace_root,
                kernel_tree,
                &out_dir,
                &arch,
                device,
                &config.kernel,
                dtb,
            )
        })
        .transpose()?;
    write_bootimg_kernel_artifact(config.bootimg.as_ref(), &out_dir, &arch, &image, &target)?;

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

pub(super) fn kernel_dtb_path(
    workspace_root: &Path,
    out_dir: &Path,
    arch: &str,
    device: &KernelDevice,
    config: &KernelConfig,
    dtb_stem: &str,
) -> PathBuf {
    if dt_overlay_source(workspace_root, device).is_file() || config.dtbo_mask_manifest.is_some() {
        processed_kernel_dtb_path(out_dir)
    } else {
        built_kernel_dtb_path(out_dir, arch, device, dtb_stem)
    }
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

fn process_device_dtb(
    workspace_root: &Path,
    kernel_tree: &Path,
    out_dir: &Path,
    arch: &str,
    device: &KernelDevice,
    config: &KernelConfig,
    base_dtb: &Path,
) -> Result<PathBuf> {
    let overlay_source = dt_overlay_source(workspace_root, device);
    let mask_manifest_path = dtbo_mask_manifest_path(workspace_root, config);
    let has_overlay = overlay_source.is_file();
    let has_mask = mask_manifest_path.is_some();
    if !has_overlay && !has_mask {
        return Ok(base_dtb.to_path_buf());
    }

    let processed_dtb = processed_kernel_dtb_path(out_dir);
    // Remove any previous output before doing fallible work so `cargo xtask
    // bootimg` cannot silently package a stale or unvalidated pocketboot.dtb
    // after this build fails.
    remove_file_if_exists(&processed_dtb)?;

    let overlay_dir = out_dir.join("dt-overlays");
    fs::create_dir_all(&overlay_dir)
        .map_err(|err| format!("create {}: {err}", overlay_dir.display()))?;

    build_dtc_tools(kernel_tree, out_dir, arch)?;

    let mut overlays = Vec::new();
    if has_overlay {
        let preprocessed = overlay_dir.join(format!("{}.dts", device.stem));
        let overlay_dtbo = overlay_dir.join(format!("{}.dtbo", device.stem));
        compile_dt_overlay(
            kernel_tree,
            out_dir,
            &overlay_source,
            &preprocessed,
            &overlay_dtbo,
            false,
        )?;
        overlays.push(overlay_dtbo);
    }

    let mask_manifest = mask_manifest_path
        .as_deref()
        .map(load_dtbo_mask_manifest)
        .transpose()?;

    let mainline_compatible = if mask_manifest.is_some() {
        let base = read_fdt(base_dtb)?;
        let compatible = root_compatible(&base, base_dtb)?.to_vec();
        let identity_source = overlay_dir.join("pocketboot-mainline-identity.dtso");
        let identity_preprocessed = overlay_dir.join("pocketboot-mainline-identity.dts");
        let identity_dtbo = overlay_dir.join("pocketboot-mainline-identity.dtbo");
        write_if_changed(
            &identity_source,
            render_mainline_identity_overlay(&compatible)?.as_bytes(),
        )?;
        compile_dt_overlay(
            kernel_tree,
            out_dir,
            &identity_source,
            &identity_preprocessed,
            &identity_dtbo,
            false,
        )?;
        // Stamp after any device overlay but before the generated mask. That
        // makes the marker describe the packaged mainline base and lets mask
        // validation prove that an ABL-selected DTBO cannot consume or alter it.
        overlays.push(identity_dtbo);
        Some(compatible)
    } else {
        None
    };

    if let Some((manifest, manifest_path)) =
        mask_manifest.as_ref().zip(mask_manifest_path.as_deref())
    {
        let source = overlay_dir.join(format!("{}-dtbo-mask.dtso", device.stem));
        let preprocessed = overlay_dir.join(format!("{}-dtbo-mask.dts", device.stem));
        let dtbo = overlay_dir.join(format!("{}-dtbo-mask.dtbo", device.stem));
        write_if_changed(
            &source,
            render_dtbo_mask_overlay(manifest, manifest_path).as_bytes(),
        )?;
        compile_dt_overlay(kernel_tree, out_dir, &source, &preprocessed, &dtbo, true)?;
        overlays.push(dtbo);
    }

    let candidate_dtb = mask_manifest
        .as_ref()
        .map(|_| overlay_dir.join("pocketboot-dtbo-mask-candidate.dtb"));
    if let Some(candidate_dtb) = &candidate_dtb {
        remove_file_if_exists(candidate_dtb)?;
    }
    let overlay_output = candidate_dtb.as_deref().unwrap_or(&processed_dtb);
    apply_dt_overlays(out_dir, base_dtb, &overlays, overlay_output)?;
    ensure_file(overlay_output, "processed device tree blob")?;

    if let Some(manifest) = &mask_manifest {
        let mainline_compatible = mainline_compatible
            .as_deref()
            .ok_or_else(|| "DTBO mask is missing its mainline identity".to_string())?;
        let processed = match read_fdt(overlay_output) {
            Ok(processed) => processed,
            Err(err) => {
                let _ = fs::remove_file(overlay_output);
                return Err(err);
            }
        };
        if let Err(err) =
            validate_mainline_identity(&processed, mainline_compatible, overlay_output)
        {
            let _ = fs::remove_file(overlay_output);
            return Err(err);
        }
        if let Err(err) = validate_dtbo_mask(
            kernel_tree,
            out_dir,
            &overlay_dir,
            base_dtb,
            overlay_output,
            manifest,
            mainline_compatible,
        ) {
            let _ = fs::remove_file(overlay_output);
            return Err(err);
        }
        fs::rename(overlay_output, &processed_dtb).map_err(|err| {
            format!(
                "promote validated DTBO mask {} to {}: {err}",
                overlay_output.display(),
                processed_dtb.display()
            )
        })?;
    }

    Ok(processed_dtb)
}

fn build_dtc_tools(kernel_tree: &Path, out_dir: &Path, arch: &str) -> Result<()> {
    let mut build = make_command_for_arch(kernel_tree, out_dir, arch)?;
    build.arg("scripts_dtc");
    run_command(build, "make scripts_dtc")?;

    ensure_file(&dtc_path(out_dir), "device tree compiler")?;
    ensure_file(&fdtoverlay_path(out_dir), "device tree overlay tool")
}

fn compile_dt_overlay(
    kernel_tree: &Path,
    out_dir: &Path,
    source: &Path,
    preprocessed: &Path,
    output: &Path,
    generate_symbols: bool,
) -> Result<()> {
    let include_prefixes = kernel_tree.join("scripts/dtc/include-prefixes");
    let source_dir = source.parent().unwrap_or_else(|| Path::new("."));
    let hostcc = env::var_os("HOSTCC").unwrap_or_else(|| "cc".into());

    let mut preprocess = Command::new(hostcc);
    preprocess
        .arg("-E")
        .arg("-nostdinc")
        .arg("-I")
        .arg(&include_prefixes)
        .arg("-I")
        .arg(source_dir)
        .arg("-undef")
        .arg("-D__DTS__")
        .arg("-x")
        .arg("assembler-with-cpp")
        .arg("-o")
        .arg(preprocessed)
        .arg(source);
    run_command(preprocess, "preprocess device tree overlay")?;

    let mut dtc = Command::new(dtc_path(out_dir));
    dtc.arg("-I")
        .arg("dts")
        .arg("-O")
        .arg("dtb")
        .arg("-o")
        .arg(output)
        .arg("-b")
        .arg("0")
        .arg("-i")
        .arg(source_dir)
        .arg("-i")
        .arg(&include_prefixes);
    if generate_symbols {
        dtc.arg("-@");
    }
    dtc.arg(preprocessed);
    run_command(dtc, "compile device tree overlay")
}

fn apply_dt_overlays(
    out_dir: &Path,
    base_dtb: &Path,
    overlays: &[PathBuf],
    output: &Path,
) -> Result<()> {
    if overlays.is_empty() {
        return Err("cannot apply an empty device tree overlay list".to_string());
    }

    let mut tmp_name = output.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("remove {}: {err}", tmp.display())),
    }

    let mut fdtoverlay = Command::new(fdtoverlay_path(out_dir));
    fdtoverlay
        .arg("-i")
        .arg(base_dtb)
        .arg("-o")
        .arg(&tmp)
        .args(overlays);
    run_command(fdtoverlay, "apply device tree overlay")?;

    fs::rename(&tmp, output)
        .map_err(|err| format!("rename {} to {}: {err}", tmp.display(), output.display()))
}

fn dt_overlay_source(workspace_root: &Path, device: &KernelDevice) -> PathBuf {
    workspace_root
        .join("configs/dt-overlays")
        .join(&device.vendor)
        .join(format!("{}.dtso", device.stem))
}

fn dtbo_mask_manifest_path(workspace_root: &Path, config: &KernelConfig) -> Option<PathBuf> {
    config
        .dtbo_mask_manifest
        .as_ref()
        .map(|path| workspace_root.join(path))
}

fn load_dtbo_mask_manifest(path: &Path) -> Result<DtboMaskManifest> {
    let contents = fs::read_to_string(path)
        .map_err(|err| format!("read DTBO mask manifest {}: {err}", path.display()))?;
    let manifest = toml::from_str::<DtboMaskManifest>(&contents)
        .map_err(|err| format!("parse DTBO mask manifest {}: {err}", path.display()))?;
    validate_dtbo_mask_manifest(&manifest, path)?;
    Ok(manifest)
}

fn validate_dtbo_mask_manifest(manifest: &DtboMaskManifest, path: &Path) -> Result<()> {
    if manifest.format != DTBO_MASK_FORMAT {
        return Err(format!(
            "{}: unsupported DTBO mask format {}; expected {DTBO_MASK_FORMAT}",
            path.display(),
            manifest.format
        ));
    }
    if manifest.source.trim().is_empty() {
        return Err(format!("{}: DTBO mask source is empty", path.display()));
    }
    if manifest.source_sha256.len() != 64
        || !manifest
            .source_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(format!(
            "{}: DTBO mask source_sha256 must be 64 lowercase hexadecimal characters",
            path.display()
        ));
    }
    if manifest.dtbo_entries == 0 {
        return Err(format!(
            "{}: DTBO mask must describe at least one DTBO table entry",
            path.display()
        ));
    }
    if manifest.symbols.is_empty() {
        return Err(format!(
            "{}: DTBO mask symbol list is empty",
            path.display()
        ));
    }

    let mut previous: Option<&str> = None;
    for symbol in &manifest.symbols {
        if !valid_dtbo_symbol(symbol) {
            return Err(format!(
                "{}: invalid DTBO fixup symbol {symbol:?}",
                path.display()
            ));
        }
        if let Some(previous) = previous
            && symbol.as_str() <= previous
        {
            let reason = if symbol == previous {
                "duplicate"
            } else {
                "not sorted"
            };
            return Err(format!(
                "{}: DTBO fixup symbols are {reason} near {symbol:?}",
                path.display()
            ));
        }
        previous = Some(symbol);
    }
    Ok(())
}

fn valid_dtbo_symbol(symbol: &str) -> bool {
    let mut bytes = symbol.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn render_dtbo_mask_overlay(manifest: &DtboMaskManifest, manifest_path: &Path) -> String {
    let mut source = format!(
        "// Generated from {}. Do not edit this build artifact.\n\
         // DTBO source: {}\n\
         // DTBO source SHA-256: {}\n\
         /dts-v1/;\n\
         /plugin/;\n\n\
         / {{\n\
         \tfragment@0 {{\n\
         \t\ttarget-path = \"/\";\n\n\
         \t\t__overlay__ {{\n\
         \t\t\tmasked-devices {{\n",
        manifest_path.display(),
        manifest.source,
        manifest.source_sha256
    );
    for symbol in &manifest.symbols {
        source.push_str(&format!("\t\t\t\t{symbol}: {symbol} {{}};\n"));
    }
    source.push_str("\t\t\t};\n\t\t};\n\t};\n};\n");
    source
}

fn render_dtbo_mask_validation_overlay(manifest: &DtboMaskManifest) -> String {
    let mut source =
        String::from("// Generated DTBO sink validation overlay.\n/dts-v1/;\n/plugin/;\n\n");
    for symbol in &manifest.symbols {
        source.push_str(&format!(
            "&{symbol} {{\n\t{DTBO_MASK_VALIDATION_PROPERTY} = \"{symbol}\";\n}};\n\n"
        ));
    }
    source
}

fn render_mainline_identity_overlay(compatible: &[u8]) -> Result<String> {
    validate_fdt_string_list(compatible).map_err(|err| {
        format!("cannot stamp invalid root compatible as mainline identity: {err}")
    })?;
    let bytes = compatible
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "// Generated from the packaged mainline base DTB. Do not edit.\n\
         /dts-v1/;\n\
         /plugin/;\n\n\
         / {{\n\
         \tfragment@0 {{\n\
         \t\ttarget-path = \"/chosen\";\n\n\
         \t\t__overlay__ {{\n\
         \t\t\t{MAINLINE_COMPATIBLE_PROPERTY} = [{bytes}];\n\
         \t\t}};\n\
         \t}};\n\
         }};\n"
    ))
}

fn validate_dtbo_mask(
    kernel_tree: &Path,
    out_dir: &Path,
    overlay_dir: &Path,
    base_dtb: &Path,
    masked_dtb: &Path,
    manifest: &DtboMaskManifest,
    mainline_compatible: &[u8],
) -> Result<()> {
    let base = read_fdt(base_dtb)?;
    let masked = read_fdt(masked_dtb)?;
    validate_chosen_with_mainline_identity(
        &base,
        &masked,
        mainline_compatible,
        base_dtb,
        masked_dtb,
    )?;
    validate_mask_symbols(&masked, manifest, masked_dtb)?;

    let validation_source = overlay_dir.join("dtbo-mask-validation.dtso");
    let validation_preprocessed = overlay_dir.join("dtbo-mask-validation.dts");
    let validation_dtbo = overlay_dir.join("dtbo-mask-validation.dtbo");
    let validation_dtb = overlay_dir.join("dtbo-mask-validation.dtb");
    write_if_changed(
        &validation_source,
        render_dtbo_mask_validation_overlay(manifest).as_bytes(),
    )?;
    compile_dt_overlay(
        kernel_tree,
        out_dir,
        &validation_source,
        &validation_preprocessed,
        &validation_dtbo,
        false,
    )?;
    apply_dt_overlays(
        out_dir,
        masked_dtb,
        std::slice::from_ref(&validation_dtbo),
        &validation_dtb,
    )?;

    let validated = read_fdt(&validation_dtb)?;
    validate_chosen_unchanged(&masked, &validated, masked_dtb, &validation_dtb)?;
    validate_mainline_identity(&validated, mainline_compatible, &validation_dtb)?;
    validate_mask_targets(&validated, manifest, &validation_dtb)?;
    fs::remove_file(&validation_dtb)
        .map_err(|err| format!("remove {}: {err}", validation_dtb.display()))?;

    println!(
        "validated {} DTBO fixup targets under /masked-devices",
        manifest.symbols.len()
    );
    Ok(())
}

fn root_compatible<'a>(fdt: &'a FdtContents, path: &Path) -> Result<&'a [u8]> {
    let compatible = fdt
        .properties
        .get(&("/".to_string(), "compatible".to_string()))
        .ok_or_else(|| format!("{}: root compatible property is missing", path.display()))?;
    validate_fdt_string_list(compatible).map_err(|err| {
        format!(
            "{}: invalid root compatible property: {err}",
            path.display()
        )
    })?;
    Ok(compatible)
}

fn validate_mainline_identity(fdt: &FdtContents, expected: &[u8], path: &Path) -> Result<()> {
    let actual = fdt
        .properties
        .get(&(
            "/chosen".to_string(),
            MAINLINE_COMPATIBLE_PROPERTY.to_string(),
        ))
        .ok_or_else(|| {
            format!(
                "{}: /chosen/{MAINLINE_COMPATIBLE_PROPERTY} is missing",
                path.display()
            )
        })?;
    if actual != expected {
        return Err(format!(
            "{}: /chosen/{MAINLINE_COMPATIBLE_PROPERTY} does not exactly match the packaged base root compatible bytes",
            path.display()
        ));
    }
    validate_fdt_string_list(actual).map_err(|err| {
        format!(
            "{}: invalid /chosen/{MAINLINE_COMPATIBLE_PROPERTY}: {err}",
            path.display()
        )
    })?;
    Ok(())
}

fn validate_chosen_with_mainline_identity(
    before: &FdtContents,
    after: &FdtContents,
    mainline_compatible: &[u8],
    before_path: &Path,
    after_path: &Path,
) -> Result<()> {
    if !before.nodes.contains("/chosen") {
        return Err(format!(
            "{}: DTBO masking requires /chosen so ABL's androidboot.dtbo_idx receipt remains accessible",
            before_path.display()
        ));
    }
    let mut expected = properties_at_path(before, "/chosen");
    expected.insert(MAINLINE_COMPATIBLE_PROPERTY, mainline_compatible);
    let actual = properties_at_path(after, "/chosen");
    if expected != actual {
        return Err(format!(
            "DTBO masking changed /chosen beyond the validated {MAINLINE_COMPATIBLE_PROPERTY} injection between {} and {}; refusing the image",
            before_path.display(),
            after_path.display()
        ));
    }
    validate_mainline_identity(after, mainline_compatible, after_path)
}

fn validate_fdt_string_list(value: &[u8]) -> Result<Vec<&str>> {
    if value.is_empty() || value.last() != Some(&0) {
        return Err("value is not a non-empty NUL-terminated string list".to_string());
    }
    let mut strings = Vec::new();
    for bytes in value[..value.len() - 1].split(|byte| *byte == 0) {
        if bytes.is_empty() {
            return Err("value contains an empty string".to_string());
        }
        let string = std::str::from_utf8(bytes)
            .map_err(|err| format!("value contains invalid UTF-8: {err}"))?;
        strings.push(string);
    }
    if strings.is_empty() {
        return Err("value has no strings".to_string());
    }
    Ok(strings)
}

fn validate_chosen_unchanged(
    before: &FdtContents,
    after: &FdtContents,
    before_path: &Path,
    after_path: &Path,
) -> Result<()> {
    if !before.nodes.contains("/chosen") {
        return Err(format!(
            "{}: DTBO masking requires /chosen so ABL's androidboot.dtbo_idx receipt remains accessible",
            before_path.display()
        ));
    }
    if !after.nodes.contains("/chosen") {
        return Err(format!(
            "{}: DTBO masking removed /chosen and would lose ABL's androidboot.dtbo_idx receipt",
            after_path.display()
        ));
    }

    let before_chosen = properties_at_path(before, "/chosen");
    let after_chosen = properties_at_path(after, "/chosen");
    if before_chosen != after_chosen {
        return Err(format!(
            "DTBO masking changed /chosen between {} and {}; refusing an image that could lose androidboot.dtbo_idx",
            before_path.display(),
            after_path.display()
        ));
    }
    Ok(())
}

fn properties_at_path<'a>(fdt: &'a FdtContents, path: &str) -> BTreeMap<&'a str, &'a [u8]> {
    fdt.properties
        .iter()
        .filter_map(|((property_path, name), value)| {
            (property_path == path).then_some((name.as_str(), value.as_slice()))
        })
        .collect()
}

fn validate_mask_symbols(
    fdt: &FdtContents,
    manifest: &DtboMaskManifest,
    path: &Path,
) -> Result<()> {
    for symbol in &manifest.symbols {
        let symbol_key = ("/__symbols__".to_string(), symbol.clone());
        let value = fdt.properties.get(&symbol_key).ok_or_else(|| {
            format!(
                "{}: generated DTBO mask did not export symbol {symbol:?}",
                path.display()
            )
        })?;
        let value = fdt_string(value).map_err(|err| {
            format!(
                "{}: invalid /__symbols__/{symbol} value: {err}",
                path.display()
            )
        })?;
        let expected = format!("/masked-devices/{symbol}");
        if value != expected {
            return Err(format!(
                "{}: DTBO fixup symbol {symbol:?} resolves to {value:?}, expected {expected:?}",
                path.display()
            ));
        }
        if !fdt.nodes.contains(&expected) {
            return Err(format!(
                "{}: DTBO fixup symbol {symbol:?} resolves to missing node {expected}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn validate_mask_targets(
    fdt: &FdtContents,
    manifest: &DtboMaskManifest,
    path: &Path,
) -> Result<()> {
    let mut targets = BTreeMap::new();
    for ((node_path, name), value) in &fdt.properties {
        if name != DTBO_MASK_VALIDATION_PROPERTY {
            continue;
        }
        let symbol = fdt_string(value).map_err(|err| {
            format!(
                "{}: invalid {DTBO_MASK_VALIDATION_PROPERTY} at {node_path}: {err}",
                path.display()
            )
        })?;
        if targets
            .insert(symbol.to_string(), node_path.clone())
            .is_some()
        {
            return Err(format!(
                "{}: duplicate DTBO mask validation target for {symbol:?}",
                path.display()
            ));
        }
    }

    if targets.len() != manifest.symbols.len() {
        return Err(format!(
            "{}: only {} of {} DTBO fixups reached a validation target",
            path.display(),
            targets.len(),
            manifest.symbols.len()
        ));
    }
    for symbol in &manifest.symbols {
        let expected = format!("/masked-devices/{symbol}");
        match targets.get(symbol) {
            Some(actual) if actual == &expected => {}
            Some(actual) => {
                return Err(format!(
                    "{}: DTBO fixup {symbol:?} landed at {actual}, outside {expected}",
                    path.display()
                ));
            }
            None => {
                return Err(format!(
                    "{}: DTBO fixup {symbol:?} did not land in /masked-devices",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn read_fdt(path: &Path) -> Result<FdtContents> {
    let bytes = fs::read(path).map_err(|err| format!("read FDT {}: {err}", path.display()))?;
    parse_fdt(&bytes).map_err(|err| format!("parse FDT {}: {err}", path.display()))
}

fn parse_fdt(bytes: &[u8]) -> Result<FdtContents> {
    const FDT_MAGIC: u32 = 0xd00d_feed;
    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE: u32 = 2;
    const FDT_PROP: u32 = 3;
    const FDT_NOP: u32 = 4;
    const FDT_END: u32 = 9;

    if bytes.len() < 40 {
        return Err(format!("header is truncated: {} bytes", bytes.len()));
    }
    if fdt_u32(bytes, 0)? != FDT_MAGIC {
        return Err("bad FDT magic".to_string());
    }
    let total_size = fdt_usize(bytes, 4)?;
    if total_size > bytes.len() {
        return Err(format!(
            "declared size {total_size} exceeds file size {}",
            bytes.len()
        ));
    }
    let structure_offset = fdt_usize(bytes, 8)?;
    let strings_offset = fdt_usize(bytes, 12)?;
    let strings_size = fdt_usize(bytes, 32)?;
    let structure_size = fdt_usize(bytes, 36)?;
    let structure_end = structure_offset
        .checked_add(structure_size)
        .filter(|end| *end <= total_size)
        .ok_or_else(|| "FDT structure block is out of bounds".to_string())?;
    let strings_end = strings_offset
        .checked_add(strings_size)
        .filter(|end| *end <= total_size)
        .ok_or_else(|| "FDT strings block is out of bounds".to_string())?;

    let mut cursor = structure_offset;
    let mut stack: Vec<String> = Vec::new();
    let mut nodes = BTreeSet::new();
    let mut properties = BTreeMap::new();
    let mut saw_end = false;
    while cursor < structure_end {
        let token = fdt_u32(bytes, cursor)?;
        cursor += 4;
        match token {
            FDT_BEGIN_NODE => {
                let (name, next) = fdt_c_string_at(bytes, cursor, structure_end)?;
                cursor = align_fdt(next)?;
                if cursor > structure_end {
                    return Err("FDT node name padding is out of bounds".to_string());
                }
                let path = match stack.last() {
                    None if name.is_empty() => "/".to_string(),
                    None => return Err("FDT root node has a non-empty name".to_string()),
                    Some(parent) if name.is_empty() => {
                        return Err(format!("empty child node name under {parent}"));
                    }
                    Some(parent) if parent == "/" => format!("/{name}"),
                    Some(parent) => format!("{parent}/{name}"),
                };
                if !nodes.insert(path.clone()) {
                    return Err(format!("duplicate FDT node path {path}"));
                }
                stack.push(path);
            }
            FDT_END_NODE => {
                stack
                    .pop()
                    .ok_or_else(|| "unmatched FDT_END_NODE".to_string())?;
            }
            FDT_PROP => {
                let value_len = fdt_usize(bytes, cursor)?;
                let name_offset = fdt_usize(bytes, cursor + 4)?;
                cursor += 8;
                let value_end = cursor
                    .checked_add(value_len)
                    .filter(|end| *end <= structure_end)
                    .ok_or_else(|| "FDT property value is out of bounds".to_string())?;
                let value = bytes[cursor..value_end].to_vec();
                cursor = align_fdt(value_end)?;
                if cursor > structure_end {
                    return Err("FDT property padding is out of bounds".to_string());
                }
                let name_start = strings_offset
                    .checked_add(name_offset)
                    .filter(|start| *start < strings_end)
                    .ok_or_else(|| "FDT property name offset is out of bounds".to_string())?;
                let (name, _) = fdt_c_string_at(bytes, name_start, strings_end)?;
                let node_path = stack
                    .last()
                    .ok_or_else(|| "FDT property appears outside a node".to_string())?;
                if properties
                    .insert((node_path.clone(), name.to_string()), value)
                    .is_some()
                {
                    return Err(format!("duplicate FDT property {node_path}/{name}"));
                }
            }
            FDT_NOP => {}
            FDT_END => {
                if !stack.is_empty() {
                    return Err("FDT_END encountered before all nodes closed".to_string());
                }
                saw_end = true;
                break;
            }
            token => return Err(format!("unknown FDT structure token {token}")),
        }
    }
    if !saw_end {
        return Err("FDT structure is missing FDT_END".to_string());
    }

    Ok(FdtContents { nodes, properties })
}

fn fdt_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| format!("FDT u32 at offset {offset} is out of bounds"))?;
    Ok(u32::from_be_bytes(bytes[offset..end].try_into().unwrap()))
}

fn fdt_usize(bytes: &[u8], offset: usize) -> Result<usize> {
    usize::try_from(fdt_u32(bytes, offset)?)
        .map_err(|_| format!("FDT u32 at offset {offset} does not fit usize"))
}

fn fdt_c_string_at(bytes: &[u8], offset: usize, end: usize) -> Result<(&str, usize)> {
    if offset >= end || end > bytes.len() {
        return Err(format!("FDT string offset {offset} is out of bounds"));
    }
    let relative_end = bytes[offset..end]
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| format!("unterminated FDT string at offset {offset}"))?;
    let string_end = offset + relative_end;
    let value = std::str::from_utf8(&bytes[offset..string_end])
        .map_err(|err| format!("FDT string at offset {offset} is not UTF-8: {err}"))?;
    Ok((value, string_end + 1))
}

fn fdt_string(value: &[u8]) -> Result<&str> {
    let value_len = value.len();
    let (value, next) = fdt_c_string_at(value, 0, value.len())?;
    if next != value_len {
        return Err("value is not exactly one NUL-terminated string".to_string());
    }
    Ok(value)
}

fn align_fdt(value: usize) -> Result<usize> {
    value
        .checked_add(3)
        .map(|value| value & !3)
        .ok_or_else(|| "FDT alignment overflow".to_string())
}

fn built_kernel_dtb_path(
    out_dir: &Path,
    arch: &str,
    device: &KernelDevice,
    dtb_stem: &str,
) -> PathBuf {
    out_dir
        .join(format!("arch/{arch}/boot/dts"))
        .join(&device.vendor)
        .join(format!("{dtb_stem}.dtb"))
}

fn processed_kernel_dtb_path(out_dir: &Path) -> PathBuf {
    out_dir.join(PROCESSED_DTB)
}

fn dtc_path(out_dir: &Path) -> PathBuf {
    out_dir.join("scripts/dtc/dtc")
}

fn fdtoverlay_path(out_dir: &Path) -> PathBuf {
    out_dir.join("scripts/dtc/fdtoverlay")
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

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("remove {}: {err}", path.display())),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crosshatch_mask_manifest_records_pinned_official_dtbo_fixups() {
        let workspace_root = super::super::workspace_root().unwrap();
        let device = KernelDevice::parse("qcom/sdm845-google-crosshatch").unwrap();
        let config = config::load_device_config(&workspace_root, &device).unwrap();
        let path = dtbo_mask_manifest_path(&workspace_root, &config.kernel).unwrap();
        let manifest = load_dtbo_mask_manifest(&path).unwrap();

        assert_eq!(manifest.dtbo_entries, 14);
        assert_eq!(manifest.symbols.len(), 121);
        assert_eq!(
            manifest.source_sha256,
            "aa376a69f53759dedd67975ec211715655060a64d4aaa5b82911cb81a0048a81"
        );
        assert!(manifest.symbols.iter().any(|symbol| symbol == "aliases"));
        assert!(manifest.symbols.iter().any(|symbol| symbol == "usb0"));
        assert!(!manifest.symbols.iter().any(|symbol| symbol == "chosen"));

        let out_dir = Path::new("test-kernel-output");
        assert_eq!(
            kernel_dtb_path(
                &workspace_root,
                out_dir,
                "arm64",
                &device,
                &config.kernel,
                "sdm845-google-crosshatch"
            ),
            out_dir.join(PROCESSED_DTB)
        );
    }

    #[test]
    fn unmasked_device_keeps_the_kernel_built_dtb_path() {
        let workspace_root = super::super::workspace_root().unwrap();
        let device = KernelDevice::parse("qcom/sdm670-google-sargo").unwrap();
        let config = config::load_device_config(&workspace_root, &device).unwrap();
        let out_dir = Path::new("test-kernel-output");

        assert_eq!(
            kernel_dtb_path(
                &workspace_root,
                out_dir,
                "arm64",
                &device,
                &config.kernel,
                "sdm670-google-sargo"
            ),
            out_dir.join("arch/arm64/boot/dts/qcom/sdm670-google-sargo.dtb")
        );
    }

    #[test]
    fn generated_mask_and_validation_cover_every_manifest_symbol() {
        let manifest = test_manifest();
        let mask = render_dtbo_mask_overlay(&manifest, Path::new("mask.toml"));
        let validation = render_dtbo_mask_validation_overlay(&manifest);

        assert_eq!(
            mask,
            render_dtbo_mask_overlay(&manifest, Path::new("mask.toml"))
        );
        assert_eq!(validation, render_dtbo_mask_validation_overlay(&manifest));

        for symbol in &manifest.symbols {
            assert!(mask.contains(&format!("{symbol}: {symbol} {{}};")));
            assert!(validation.contains(&format!("&{symbol} {{")));
            assert!(
                validation.contains(&format!("{DTBO_MASK_VALIDATION_PROPERTY} = \"{symbol}\";"))
            );
        }
        assert!(!mask.contains("/chosen"));
        assert!(!validation.contains("/chosen"));
    }

    #[test]
    fn mainline_identity_preserves_exact_base_compatible_bytes_through_masking() {
        let compatible = b"google,crosshatch\0qcom,sdm845\0";
        let base = parse_fdt(&test_identity_fdt(None)).unwrap();
        let masked = parse_fdt(&test_identity_fdt(Some(compatible))).unwrap();

        assert_eq!(
            root_compatible(&base, Path::new("base.dtb")).unwrap(),
            compatible
        );
        validate_chosen_with_mainline_identity(
            &base,
            &masked,
            compatible,
            Path::new("base.dtb"),
            Path::new("masked.dtb"),
        )
        .unwrap();
        validate_mainline_identity(&masked, compatible, Path::new("masked.dtb")).unwrap();

        let overlay = render_mainline_identity_overlay(compatible).unwrap();
        assert!(overlay.contains(
            "pocketboot,mainline-compatible = [67 6f 6f 67 6c 65 2c 63 72 6f 73 73 68 61 74 63 68 00 71 63 6f 6d 2c 73 64 6d 38 34 35 00];"
        ));
    }

    #[test]
    fn mainline_identity_validation_rejects_changed_or_malformed_bytes() {
        let compatible = b"google,crosshatch\0qcom,sdm845\0";
        let changed = parse_fdt(&test_identity_fdt(Some(
            b"google,b1c1-sdm845\0qcom,sdm845\0",
        )))
        .unwrap();
        assert!(
            validate_mainline_identity(&changed, compatible, Path::new("changed.dtb"))
                .unwrap_err()
                .contains("does not exactly match")
        );
        assert!(render_mainline_identity_overlay(b"google,crosshatch").is_err());
        assert!(render_mainline_identity_overlay(b"google,crosshatch\0\0").is_err());
    }

    #[test]
    fn stale_mask_output_is_removed_fail_closed() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "pocketboot-mask-stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, b"stale unvalidated DTB").unwrap();

        remove_file_if_exists(&path).unwrap();
        assert!(!path.exists());
        remove_file_if_exists(&path).unwrap();
    }

    #[test]
    fn fdt_validation_accepts_only_masked_fixup_targets_and_preserves_receipt() {
        let manifest = test_manifest();
        let before = parse_fdt(&test_fdt(false, false)).unwrap();
        let masked = parse_fdt(&test_fdt(true, false)).unwrap();
        let validated = parse_fdt(&test_fdt(true, true)).unwrap();

        validate_chosen_unchanged(
            &before,
            &masked,
            Path::new("base.dtb"),
            Path::new("masked.dtb"),
        )
        .unwrap();
        validate_mask_symbols(&masked, &manifest, Path::new("masked.dtb")).unwrap();
        validate_mask_targets(&validated, &manifest, Path::new("validated.dtb")).unwrap();

        let receipt = properties_at_path(&validated, "/chosen")
            .get("bootargs")
            .copied()
            .unwrap();
        assert_eq!(
            fdt_string(receipt).unwrap(),
            "console=ttyMSM0 androidboot.dtbo_idx=3"
        );
    }

    #[test]
    fn fdt_validation_rejects_a_fixup_outside_masked_devices() {
        let manifest = test_manifest();
        let fdt = parse_fdt(&test_fdt_with_bad_target()).unwrap();
        let error = validate_mask_targets(&fdt, &manifest, Path::new("bad.dtb")).unwrap_err();

        assert!(error.contains("outside /masked-devices/beta"));
    }

    #[test]
    fn manifest_validation_rejects_unsorted_or_unsafe_symbols() {
        let mut manifest = test_manifest();
        manifest.symbols.swap(0, 1);
        assert!(
            validate_dtbo_mask_manifest(&manifest, Path::new("bad.toml"))
                .unwrap_err()
                .contains("not sorted")
        );

        manifest.symbols = vec!["bad-symbol".to_string()];
        assert!(
            validate_dtbo_mask_manifest(&manifest, Path::new("bad.toml"))
                .unwrap_err()
                .contains("invalid DTBO fixup symbol")
        );
    }

    fn test_manifest() -> DtboMaskManifest {
        DtboMaskManifest {
            format: DTBO_MASK_FORMAT,
            source: "test fixture".to_string(),
            source_sha256: "0".repeat(64),
            dtbo_entries: 1,
            symbols: vec!["alpha".to_string(), "beta".to_string()],
        }
    }

    fn test_fdt(include_mask: bool, include_targets: bool) -> Vec<u8> {
        let mut fdt = TestFdt::new();
        fdt.begin_node("");
        fdt.begin_node("chosen");
        fdt.property("bootargs", b"console=ttyMSM0 androidboot.dtbo_idx=3\0");
        fdt.end_node();
        if include_mask {
            fdt.begin_node("__symbols__");
            fdt.property("alpha", b"/masked-devices/alpha\0");
            fdt.property("beta", b"/masked-devices/beta\0");
            fdt.end_node();
            fdt.begin_node("masked-devices");
            fdt.begin_node("alpha");
            if include_targets {
                fdt.property(DTBO_MASK_VALIDATION_PROPERTY, b"alpha\0");
            }
            fdt.end_node();
            fdt.begin_node("beta");
            if include_targets {
                fdt.property(DTBO_MASK_VALIDATION_PROPERTY, b"beta\0");
            }
            fdt.end_node();
            fdt.end_node();
        }
        fdt.end_node();
        fdt.finish()
    }

    fn test_fdt_with_bad_target() -> Vec<u8> {
        let mut fdt = TestFdt::new();
        fdt.begin_node("");
        fdt.begin_node("chosen");
        fdt.property("bootargs", b"androidboot.dtbo_idx=3\0");
        fdt.end_node();
        fdt.begin_node("masked-devices");
        fdt.begin_node("alpha");
        fdt.property(DTBO_MASK_VALIDATION_PROPERTY, b"alpha\0");
        fdt.end_node();
        fdt.end_node();
        fdt.begin_node("real-device");
        fdt.property(DTBO_MASK_VALIDATION_PROPERTY, b"beta\0");
        fdt.end_node();
        fdt.end_node();
        fdt.finish()
    }

    fn test_identity_fdt(identity: Option<&[u8]>) -> Vec<u8> {
        let mut fdt = TestFdt::new();
        fdt.begin_node("");
        fdt.property("compatible", b"google,crosshatch\0qcom,sdm845\0");
        fdt.begin_node("chosen");
        fdt.property("bootargs", b"console=ttyMSM0 androidboot.dtbo_idx=13\0");
        if let Some(identity) = identity {
            fdt.property(MAINLINE_COMPATIBLE_PROPERTY, identity);
        }
        fdt.end_node();
        fdt.end_node();
        fdt.finish()
    }

    struct TestFdt {
        structure: Vec<u8>,
        strings: Vec<u8>,
        string_offsets: BTreeMap<String, u32>,
    }

    impl TestFdt {
        fn new() -> Self {
            Self {
                structure: Vec::new(),
                strings: Vec::new(),
                string_offsets: BTreeMap::new(),
            }
        }

        fn begin_node(&mut self, name: &str) {
            push_fdt_u32(&mut self.structure, 1);
            self.structure.extend_from_slice(name.as_bytes());
            self.structure.push(0);
            pad_fdt(&mut self.structure);
        }

        fn end_node(&mut self) {
            push_fdt_u32(&mut self.structure, 2);
        }

        fn property(&mut self, name: &str, value: &[u8]) {
            let name_offset = match self.string_offsets.get(name) {
                Some(offset) => *offset,
                None => {
                    let offset = u32::try_from(self.strings.len()).unwrap();
                    self.strings.extend_from_slice(name.as_bytes());
                    self.strings.push(0);
                    self.string_offsets.insert(name.to_string(), offset);
                    offset
                }
            };
            push_fdt_u32(&mut self.structure, 3);
            push_fdt_u32(&mut self.structure, u32::try_from(value.len()).unwrap());
            push_fdt_u32(&mut self.structure, name_offset);
            self.structure.extend_from_slice(value);
            pad_fdt(&mut self.structure);
        }

        fn finish(mut self) -> Vec<u8> {
            push_fdt_u32(&mut self.structure, 9);
            let header_size = 40_u32;
            let reserve_size = 16_u32;
            let structure_offset = header_size + reserve_size;
            let structure_size = u32::try_from(self.structure.len()).unwrap();
            let strings_offset = structure_offset + structure_size;
            let strings_size = u32::try_from(self.strings.len()).unwrap();
            let total_size = strings_offset + strings_size;

            let mut bytes = Vec::new();
            for value in [
                0xd00d_feed,
                total_size,
                structure_offset,
                strings_offset,
                header_size,
                17,
                16,
                0,
                strings_size,
                structure_size,
            ] {
                push_fdt_u32(&mut bytes, value);
            }
            bytes.extend_from_slice(&[0; 16]);
            bytes.extend_from_slice(&self.structure);
            bytes.extend_from_slice(&self.strings);
            bytes
        }
    }

    fn push_fdt_u32(output: &mut Vec<u8>, value: u32) {
        output.extend_from_slice(&value.to_be_bytes());
    }

    fn pad_fdt(output: &mut Vec<u8>) {
        while !output.len().is_multiple_of(4) {
            output.push(0);
        }
    }
}
