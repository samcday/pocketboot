use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    thread,
};

use abootimg_oxide::{HeaderV0, HeaderV0Versioned, OsVersionPatch};
use serde::Deserialize;
use sha1::{Digest as Sha1Digest, Sha1};
use sha2::{Digest as Sha2Digest, Sha256};

const DEFAULT_TARGET: &str = "aarch64-unknown-linux-musl";
const INIT_BINARY: &str = "pocketboot";
const DEFAULT_INITRD: &str = "pocketboot-initrd.cpio";
const KERNEL_ARCH: &str = "arm64";
const QEMU_TARGET: &str = "aarch64-virt";
const QEMU_DISK_SIZE: u64 = 64 * 1024 * 1024;
const ANDROID_BOOT_MAGIC: &[u8; 8] = b"ANDROID!";
const SEANDROID_ENFORCE: &[u8] = b"SEANDROIDENFORCE";
const BUSYBOX_VERSION: &str = "1.38.0";
const BUSYBOX_ARCHIVE_SHA256: &str =
    "34f9ea6ff8636f2c9241153b9114eefa9e65674a45318ae1ef95bb5f31c53bb2";
const BUSYBOX_SOURCE_URL: &str = "https://busybox.net/downloads/busybox-1.38.0.tar.bz2";

type Result<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("cpio") => {
            let args = args.collect::<Vec<_>>();
            if args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
            {
                print_cpio_usage();
                Ok(())
            } else {
                cpio(CpioArgs::parse(args)?)
            }
        }
        Some("kernel") => {
            let args = args.collect::<Vec<_>>();
            if args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
            {
                print_kernel_usage();
                Ok(())
            } else {
                kernel(KernelArgs::parse(args)?)
            }
        }
        Some("bootimg") => {
            let args = args.collect::<Vec<_>>();
            if args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
            {
                print_bootimg_usage();
                Ok(())
            } else {
                bootimg(BootImgArgs::parse(args)?)
            }
        }
        Some("qemu") => {
            let args = args.collect::<Vec<_>>();
            if args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
            {
                print_qemu_usage();
                Ok(())
            } else {
                qemu(QemuArgs::parse(args)?)
            }
        }
        Some("help" | "--help" | "-h") | None => {
            print_usage();
            Ok(())
        }
        Some(command) => Err(format!("unknown xtask command: {command}")),
    }
}

#[derive(Debug)]
struct CpioArgs {
    target: String,
    output: Option<PathBuf>,
    busybox: bool,
}

impl CpioArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut target = DEFAULT_TARGET.to_string();
        let mut output = None;
        let mut busybox = true;
        let mut index = 0;

        while index < args.len() {
            let arg = &args[index];
            match arg.as_str() {
                "--target" => {
                    index += 1;
                    target = args
                        .get(index)
                        .ok_or_else(|| "--target requires a value".to_string())?
                        .to_string();
                }
                "--output" | "-o" => {
                    index += 1;
                    output = Some(PathBuf::from(
                        args.get(index)
                            .ok_or_else(|| "--output requires a value".to_string())?,
                    ));
                }
                "--no-busybox" => busybox = false,
                value if value.starts_with("--target=") => {
                    target = value["--target=".len()..].to_string();
                }
                value if value.starts_with("--output=") => {
                    output = Some(PathBuf::from(&value["--output=".len()..]));
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown cpio option: {value}"));
                }
                value => {
                    if output.is_some() {
                        return Err(format!("unexpected positional argument: {value}"));
                    }
                    output = Some(PathBuf::from(value));
                }
            }
            index += 1;
        }

        Ok(Self {
            target,
            output,
            busybox,
        })
    }
}

fn cpio(args: CpioArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let output = build_initrd(&workspace_root, &args.target, args.output, args.busybox)?;
    println!("wrote {}", output.display());
    Ok(())
}

fn build_initrd(
    workspace_root: &Path,
    target: &str,
    output: Option<PathBuf>,
    include_busybox: bool,
) -> Result<PathBuf> {
    build_release(workspace_root, target)?;

    let target_dir = target_dir(workspace_root);
    let init = target_dir.join(target).join("release").join(INIT_BINARY);
    if !init.is_file() {
        return Err(format!("release binary not found: {}", init.display()));
    }

    let output = output.unwrap_or_else(|| target_dir.join(DEFAULT_INITRD));
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }

    let busybox = include_busybox
        .then(|| build_busybox(&target_dir, target))
        .transpose()?;
    write_initrd(&init, busybox.as_ref(), &output)?;
    Ok(output)
}

fn build_release(workspace_root: &Path, target: &str) -> Result<()> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(workspace_root)
        .args(["build", "--release", "--target", target, "-p", INIT_BINARY])
        .status()
        .map_err(|err| format!("spawn cargo build: {err}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with {status}"));
    }
    Ok(())
}

#[derive(Debug)]
struct QemuArgs {
    kernel_tree: PathBuf,
    build_only: bool,
    qemu_args: Vec<String>,
}

impl QemuArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut kernel_tree = None;
        let mut build_only = false;
        let mut qemu_args = Vec::new();
        let mut index = 0;

        while index < args.len() {
            let arg = &args[index];
            match arg.as_str() {
                "--build-only" => build_only = true,
                "--" => {
                    qemu_args.extend(args[index + 1..].iter().cloned());
                    break;
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown qemu option: {value}"));
                }
                value => {
                    if kernel_tree.is_some() {
                        return Err(format!("unexpected positional argument: {value}"));
                    }
                    kernel_tree = Some(PathBuf::from(value));
                }
            }
            index += 1;
        }

        let kernel_tree = kernel_tree.ok_or_else(|| {
            "usage: cargo xtask qemu [--build-only] <kernel-tree> [-- QEMU-ARG...]".to_string()
        })?;
        Ok(Self {
            kernel_tree,
            build_only,
            qemu_args,
        })
    }
}

fn qemu(args: QemuArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let kernel_tree = kernel_tree(&args.kernel_tree)?;
    let target_dir = target_dir(&workspace_root);
    let out_dir = target_dir.join("kernel").join("qemu").join(QEMU_TARGET);
    fs::create_dir_all(&out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;

    let initrd = build_initrd(&workspace_root, DEFAULT_TARGET, None, true)?;
    println!("initrd {}", initrd.display());

    let image = build_qemu_kernel(&workspace_root, &kernel_tree, &out_dir)?;
    let disk = qemu_disk(&target_dir)?;

    println!("image {}", image.display());
    println!("disk {}", disk.display());
    println!("config {}", out_dir.join(".config").display());

    if args.build_only {
        return Ok(());
    }

    run_qemu(&workspace_root, &image, &initrd, &disk, &args.qemu_args)
}

fn build_qemu_kernel(workspace_root: &Path, kernel_tree: &Path, out_dir: &Path) -> Result<PathBuf> {
    let common_config = workspace_root.join("configs/pocketboot.config");
    let qemu_config = workspace_root
        .join("configs/qemu")
        .join(format!("{QEMU_TARGET}.config"));
    ensure_file(&common_config, "common pocketboot config")?;
    ensure_file(&qemu_config, "QEMU config")?;

    let merge_config = kernel_tree.join("scripts/kconfig/merge_config.sh");
    ensure_file(&merge_config, "merge_config.sh")?;
    let mut merge = Command::new(&merge_config);
    merge
        .current_dir(kernel_tree)
        .env("ARCH", KERNEL_ARCH)
        .args(["-s", "-n", "-O"])
        .arg(out_dir)
        .arg(&common_config)
        .arg(&qemu_config);
    run_command(merge, "merge QEMU kernel config")?;

    let mut olddefconfig = make_command(kernel_tree, out_dir);
    olddefconfig.arg("olddefconfig");
    run_command(olddefconfig, "make olddefconfig")?;

    let mut build = make_command(kernel_tree, out_dir);
    build.arg(format!("-j{}", parallel_jobs())).arg("Image");
    run_command(build, "make QEMU kernel image")?;

    let image = out_dir.join("arch/arm64/boot/Image");
    ensure_file(&image, "QEMU kernel image")?;
    Ok(image)
}

fn qemu_disk(target_dir: &Path) -> Result<PathBuf> {
    let disk = target_dir.join("qemu").join(format!("{QEMU_TARGET}.raw"));
    if disk.exists() {
        ensure_file(&disk, "QEMU disk image")?;
        return Ok(disk);
    }

    if let Some(parent) = disk
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&disk)
        .map_err(|err| format!("create {}: {err}", disk.display()))?;
    file.set_len(QEMU_DISK_SIZE)
        .map_err(|err| format!("size {}: {err}", disk.display()))?;
    Ok(disk)
}

fn run_qemu(
    workspace_root: &Path,
    image: &Path,
    initrd: &Path,
    disk: &Path,
    extra_args: &[String],
) -> Result<()> {
    let qemu = env::var_os("QEMU").unwrap_or_else(|| "qemu-system-aarch64".into());
    let drive = format!("if=none,id=pocketboot,format=raw,file={}", disk.display());
    let append =
        "console=ttyAMA0 earlycon=pl011,mmio32,0x09000000 loglevel=7 panic=1 pocketboot.log=info";

    let mut command = Command::new(qemu);
    command
        .current_dir(workspace_root)
        .args([
            "-machine",
            "virt",
            "-cpu",
            "max",
            "-smp",
            "2",
            "-m",
            "512M",
            "-nographic",
            "-no-reboot",
            "-kernel",
        ])
        .arg(image)
        .arg("-initrd")
        .arg(initrd)
        .args(["-append", append, "-drive"])
        .arg(drive)
        .args(["-device", "virtio-blk-device,drive=pocketboot"])
        .args(extra_args);
    run_command(command, "qemu")
}

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

#[derive(Debug)]
struct BootImgArgs {
    device: KernelDevice,
    output: Option<PathBuf>,
}

impl BootImgArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut device = None;
        let mut output = None;
        let mut index = 0;

        while index < args.len() {
            let arg = &args[index];
            match arg.as_str() {
                "--output" | "-o" => {
                    index += 1;
                    output = Some(PathBuf::from(
                        args.get(index)
                            .ok_or_else(|| "--output requires a value".to_string())?,
                    ));
                }
                value if value.starts_with("--output=") => {
                    output = Some(PathBuf::from(&value["--output=".len()..]));
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown bootimg option: {value}"));
                }
                value => {
                    if device.is_some() {
                        return Err(format!("unexpected positional argument: {value}"));
                    }
                    device = Some(KernelDevice::parse(value)?);
                }
            }
            index += 1;
        }

        let device = device.ok_or_else(|| {
            "usage: cargo xtask bootimg <vendor/device> [--output PATH]".to_string()
        })?;
        Ok(Self { device, output })
    }
}

#[derive(Debug)]
struct KernelDevice {
    vendor: String,
    stem: String,
    soc: String,
}

impl KernelDevice {
    fn parse(value: &str) -> Result<Self> {
        let parts = value.split('/').collect::<Vec<_>>();
        if parts.len() != 2 {
            return Err(format!(
                "device ID must be a canonical DTB path without suffix, e.g. qcom/msm8916-samsung-a5u-eur: {value}"
            ));
        }

        let vendor = parts[0];
        let stem = parts[1];
        validate_device_component("vendor", vendor)?;
        validate_device_component("device", stem)?;
        if stem.ends_with(".dts") || stem.ends_with(".dtb") {
            return Err(format!("device ID must omit .dts/.dtb suffix: {value}"));
        }

        let soc = stem.split_once('-').map_or(stem, |(soc, _)| soc);
        Ok(Self {
            vendor: vendor.to_string(),
            stem: stem.to_string(),
            soc: soc.to_string(),
        })
    }
}

fn validate_device_component(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || matches!(value, "." | "..") {
        return Err(format!("invalid {kind} component in device ID: {value}"));
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Ok(())
    } else {
        Err(format!("invalid {kind} component in device ID: {value}"))
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
            let initrd = build_initrd(&workspace_root, DEFAULT_TARGET, None, true)?;
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

fn bootimg(args: BootImgArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let target_dir = target_dir(&workspace_root);
    let out_dir = target_dir
        .join("kernel")
        .join(&args.device.vendor)
        .join(&args.device.stem);
    let image = out_dir.join("arch/arm64/boot/Image.gz");
    let dtb = out_dir
        .join("arch/arm64/boot/dts")
        .join(&args.device.vendor)
        .join(format!("{}.dtb", args.device.stem));
    let config_path = workspace_root
        .join("configs/bootimg")
        .join(&args.device.vendor)
        .join(format!("{}.toml", args.device.stem));
    let output = args.output.unwrap_or_else(|| out_dir.join("boot.img"));

    ensure_file(&image, "kernel image")?;
    ensure_file(&dtb, "device tree blob")?;
    ensure_file(&config_path, "boot image config")?;

    let config = load_bootimg_config(&config_path)?;
    write_bootimg(&config, &config_path, &image, &dtb, &output)?;

    println!("wrote {}", output.display());
    println!("image {}", image.display());
    println!("dtb {}", dtb.display());
    println!("config {}", config_path.display());
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BootImgConfig {
    header_version: u32,
    page_size: u32,
    base: u64,
    kernel_offset: u64,
    ramdisk_offset: u64,
    second_offset: u64,
    tags_offset: u64,
    dtb_offset: u64,
    #[serde(default)]
    board: String,
    #[serde(default)]
    cmdline: String,
    #[serde(default)]
    ramdisk_size: u32,
    #[serde(default)]
    append_seandroid_enforce: bool,
    qcdt: Option<QcdtConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QcdtConfig {
    entries: Vec<QcdtEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QcdtEntry {
    msm_id: [u32; 2],
    board_id: [u32; 2],
}

fn load_bootimg_config(path: &Path) -> Result<BootImgConfig> {
    let contents =
        fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    toml::from_str(&contents).map_err(|err| format!("parse {}: {err}", path.display()))
}

fn write_bootimg(
    config: &BootImgConfig,
    config_path: &Path,
    image: &Path,
    dtb: &Path,
    output: &Path,
) -> Result<()> {
    if config.page_size == 0 || !config.page_size.is_power_of_two() {
        return Err("boot image page_size must be a non-zero power of two".to_string());
    }

    match config.header_version {
        0 => write_bootimg_v0(config, config_path, image, dtb, output),
        2 => write_bootimg_v2(config, config_path, image, dtb, output),
        version => Err(format!(
            "boot image header version {version} is not supported yet; expected 0 or 2"
        )),
    }
}

fn write_bootimg_v2(
    config: &BootImgConfig,
    config_path: &Path,
    image: &Path,
    dtb: &Path,
    output: &Path,
) -> Result<()> {
    if config.qcdt.is_some() {
        return Err(format!(
            "{}: qcdt requires boot image header_version = 0",
            config_path.display()
        ));
    }
    if config.ramdisk_size != 0 {
        return Err(format!(
            "{}: ramdisk_size is only supported for header_version = 0",
            config_path.display()
        ));
    }
    if config.append_seandroid_enforce {
        return Err(format!(
            "{}: append_seandroid_enforce is only supported for header_version = 0",
            config_path.display()
        ));
    }

    let kernel_size = file_size_u32(image, "kernel image")?;
    let dtb_size = file_size_u32(dtb, "device tree blob")?;
    let hash_digest = bootimg_hash_digest(image, dtb)?;
    let kernel_addr = boot_addr_u32(config.base, config.kernel_offset, "kernel_addr")?;
    let ramdisk_addr = boot_addr_u32(config.base, config.ramdisk_offset, "ramdisk_addr")?;
    let second_bootloader_addr =
        boot_addr_u32(config.base, config.second_offset, "second_bootloader_addr")?;
    let tags_addr = boot_addr_u32(config.base, config.tags_offset, "tags_addr")?;
    let dtb_addr = config
        .base
        .checked_add(config.dtb_offset)
        .ok_or_else(|| "dtb_addr overflows u64".to_string())?;

    let header = HeaderV0 {
        kernel_size,
        kernel_addr,
        ramdisk_size: 0,
        ramdisk_addr,
        second_bootloader_size: 0,
        second_bootloader_addr,
        tags_addr,
        page_size: config.page_size,
        osversionpatch: OsVersionPatch(0),
        board_name: fixed_bytes(&config.board, "board", config_path)?,
        hash_digest,
        cmdline: Box::new(fixed_bytes(&config.cmdline, "cmdline", config_path)?),
        versioned: HeaderV0Versioned::V2 {
            recovery_dtbo_size: 0,
            recovery_dtbo_addr: 0,
            dtb_size,
            dtb_addr,
        },
    };

    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }

    let mut output_file =
        File::create(output).map_err(|err| format!("create {}: {err}", output.display()))?;
    let mut kernel_file =
        File::open(image).map_err(|err| format!("open {}: {err}", image.display()))?;
    let mut dtb_file = File::open(dtb).map_err(|err| format!("open {}: {err}", dtb.display()))?;
    header
        .full_write(
            &mut output_file,
            Some(&mut kernel_file),
            None::<&mut File>,
            None::<&mut File>,
            None::<&mut File>,
            Some(&mut dtb_file),
        )
        .map_err(|err| format!("write {}: {err}", output.display()))?;
    output_file
        .set_len(header.boot_image_size() as u64)
        .map_err(|err| format!("truncate {}: {err}", output.display()))?;
    output_file
        .flush()
        .map_err(|err| format!("flush {}: {err}", output.display()))?;
    Ok(())
}

fn write_bootimg_v0(
    config: &BootImgConfig,
    config_path: &Path,
    image: &Path,
    dtb: &Path,
    output: &Path,
) -> Result<()> {
    let kernel = fs::read(image).map_err(|err| format!("read {}: {err}", image.display()))?;
    let dtb = fs::read(dtb).map_err(|err| format!("read {}: {err}", dtb.display()))?;
    let ramdisk = vec![0; config.ramdisk_size as usize];
    let vendor_dt = match &config.qcdt {
        Some(qcdt) => build_qcdt(qcdt, config.page_size, &dtb, config_path)?,
        None => Vec::new(),
    };

    let kernel_size = u32_len(kernel.len(), "kernel image", image)?;
    let ramdisk_size = u32_len(ramdisk.len(), "ramdisk", config_path)?;
    let qcdt_size = u32_len(vendor_dt.len(), "QCDT", config_path)?;
    let kernel_addr = boot_addr_u32(config.base, config.kernel_offset, "kernel_addr")?;
    let ramdisk_addr = boot_addr_u32(config.base, config.ramdisk_offset, "ramdisk_addr")?;
    let second_bootloader_addr =
        boot_addr_u32(config.base, config.second_offset, "second_bootloader_addr")?;
    let tags_addr = boot_addr_u32(config.base, config.tags_offset, "tags_addr")?;
    let page_size = usize::try_from(config.page_size)
        .map_err(|_| format!("{}: page_size does not fit usize", config_path.display()))?;

    let mut header = Vec::new();
    header.extend_from_slice(ANDROID_BOOT_MAGIC);
    write_u32_le(&mut header, kernel_size);
    write_u32_le(&mut header, kernel_addr);
    write_u32_le(&mut header, ramdisk_size);
    write_u32_le(&mut header, ramdisk_addr);
    write_u32_le(&mut header, 0);
    write_u32_le(&mut header, second_bootloader_addr);
    write_u32_le(&mut header, tags_addr);
    write_u32_le(&mut header, config.page_size);
    write_u32_le(&mut header, qcdt_size);
    write_u32_le(&mut header, 0);
    header.extend_from_slice(&fixed_bytes::<16>(&config.board, "board", config_path)?);

    let cmdline = fixed_bytes::<{ 512 + 1024 }>(&config.cmdline, "cmdline", config_path)?;
    header.extend_from_slice(&cmdline[..512]);
    header.extend_from_slice(&bootimg_hash_digest_v0(&kernel, &ramdisk, &vendor_dt));
    header.extend_from_slice(&cmdline[512..]);
    pad_vec_to(&mut header, page_size)?;

    let mut bootimg = header;
    append_padded(&mut bootimg, &kernel, page_size)?;
    append_padded(&mut bootimg, &ramdisk, page_size)?;
    append_padded(&mut bootimg, &vendor_dt, page_size)?;
    if config.append_seandroid_enforce {
        bootimg.extend_from_slice(SEANDROID_ENFORCE);
    }

    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(output, bootimg).map_err(|err| format!("write {}: {err}", output.display()))
}

fn build_qcdt(
    config: &QcdtConfig,
    page_size: u32,
    dtb: &[u8],
    config_path: &Path,
) -> Result<Vec<u8>> {
    if config.entries.is_empty() {
        return Err(format!("{}: qcdt.entries is empty", config_path.display()));
    }

    let page_size = usize::try_from(page_size)
        .map_err(|_| format!("{}: page_size does not fit usize", config_path.display()))?;
    let record_size = 24usize;
    let header_size = 12usize
        .checked_add(
            record_size
                .checked_mul(config.entries.len())
                .ok_or_else(|| "QCDT record table size overflows usize".to_string())?,
        )
        .ok_or_else(|| "QCDT header size overflows usize".to_string())?;
    let dtb_offset = align_up_usize(header_size, page_size)?;
    let dtb_size = align_up_usize(dtb.len(), page_size)?;
    let dtb_offset_u32 = u32_len(dtb_offset, "QCDT DTB offset", config_path)?;
    let dtb_size_u32 = u32_len(dtb_size, "QCDT DTB size", config_path)?;

    let mut qcdt = Vec::with_capacity(
        dtb_offset
            .checked_add(dtb_size)
            .ok_or_else(|| "QCDT size overflows usize".to_string())?,
    );
    qcdt.extend_from_slice(b"QCDT");
    write_u32_le(&mut qcdt, 2);
    write_u32_le(
        &mut qcdt,
        u32_len(config.entries.len(), "QCDT entry count", config_path)?,
    );

    for entry in &config.entries {
        write_u32_le(&mut qcdt, entry.msm_id[0]);
        write_u32_le(&mut qcdt, entry.board_id[0]);
        write_u32_le(&mut qcdt, entry.board_id[1]);
        write_u32_le(&mut qcdt, entry.msm_id[1]);
        write_u32_le(&mut qcdt, dtb_offset_u32);
        write_u32_le(&mut qcdt, dtb_size_u32);
    }

    pad_vec_to(&mut qcdt, page_size)?;
    qcdt.extend_from_slice(dtb);
    pad_vec_to(&mut qcdt, page_size)?;
    Ok(qcdt)
}

fn file_size_u32(path: &Path, description: &str) -> Result<u32> {
    let size = fs::metadata(path)
        .map_err(|err| format!("stat {}: {err}", path.display()))?
        .len();
    u32::try_from(size).map_err(|_| format!("{description} is too large: {}", path.display()))
}

fn bootimg_hash_digest(image: &Path, dtb: &Path) -> Result<[u8; 32]> {
    let mut kernel_file =
        File::open(image).map_err(|err| format!("open {}: {err}", image.display()))?;
    let mut dtb_file = File::open(dtb).map_err(|err| format!("open {}: {err}", dtb.display()))?;

    HeaderV0::compute_hash_digest::<File, Sha1>(
        Some(&mut kernel_file),
        None::<&mut File>,
        None::<&mut File>,
        None::<&mut File>,
        Some(&mut dtb_file),
    )
    .map_err(|err| format!("compute boot image hash digest: {err}"))
}

fn bootimg_hash_digest_v0(kernel: &[u8], ramdisk: &[u8], vendor_dt: &[u8]) -> [u8; 32] {
    let mut hasher = Sha1::new();
    update_bootimg_hash(&mut hasher, kernel);
    update_bootimg_hash(&mut hasher, ramdisk);
    update_bootimg_hash(&mut hasher, &[]);
    if !vendor_dt.is_empty() {
        update_bootimg_hash(&mut hasher, vendor_dt);
    }

    let digest = hasher.finalize();
    let mut output = [0; 32];
    output[..digest.len()].copy_from_slice(&digest);
    output
}

fn update_bootimg_hash(hasher: &mut Sha1, payload: &[u8]) {
    hasher.update(payload);
    hasher.update((payload.len() as u32).to_le_bytes());
}

fn append_padded(output: &mut Vec<u8>, payload: &[u8], alignment: usize) -> Result<()> {
    if payload.is_empty() {
        return Ok(());
    }

    output.extend_from_slice(payload);
    pad_vec_to(output, alignment)
}

fn pad_vec_to(output: &mut Vec<u8>, alignment: usize) -> Result<()> {
    let padded_len = align_up_usize(output.len(), alignment)?;
    output.resize(padded_len, 0);
    Ok(())
}

fn align_up_usize(value: usize, alignment: usize) -> Result<usize> {
    let pad = (alignment - (value % alignment)) % alignment;
    value
        .checked_add(pad)
        .ok_or_else(|| "aligned size overflows usize".to_string())
}

fn u32_len(value: usize, description: &str, context: &Path) -> Result<u32> {
    u32::try_from(value).map_err(|_| {
        format!(
            "{description} is too large near {}: {value} bytes",
            context.display()
        )
    })
}

fn write_u32_le(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn boot_addr_u32(base: u64, offset: u64, name: &str) -> Result<u32> {
    let value = base
        .checked_add(offset)
        .ok_or_else(|| format!("{name} overflows u64"))?;
    u32::try_from(value).map_err(|_| format!("{name} does not fit in u32: 0x{value:x}"))
}

fn fixed_bytes<const N: usize>(value: &str, field: &str, context: &Path) -> Result<[u8; N]> {
    let bytes = value.as_bytes();
    if bytes.len() > N {
        return Err(format!(
            "{field} is too long for Android boot header near {}: {} > {N} bytes",
            context.display(),
            bytes.len()
        ));
    }

    let mut output = [0; N];
    output[..bytes.len()].copy_from_slice(bytes);
    Ok(output)
}

fn kernel_tree(path: &Path) -> Result<PathBuf> {
    let path =
        fs::canonicalize(path).map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    ensure_file(&path.join("Makefile"), "kernel Makefile")?;
    ensure_file(
        &path.join("scripts/kconfig/merge_config.sh"),
        "merge_config.sh",
    )?;
    Ok(path)
}

fn canonical_file(path: &Path, description: &str) -> Result<PathBuf> {
    let path =
        fs::canonicalize(path).map_err(|err| format!("canonicalize {}: {err}", path.display()))?;
    ensure_file(&path, description)?;
    Ok(path)
}

fn make_command(kernel_tree: &Path, out_dir: &Path) -> Command {
    let make = env::var_os("MAKE").unwrap_or_else(|| "make".into());
    let mut output = OsString::from("O=");
    output.push(out_dir.as_os_str());

    let mut command = Command::new(make);
    command
        .current_dir(kernel_tree)
        .env("ARCH", KERNEL_ARCH)
        .arg(output);
    if env::var_os("CROSS_COMPILE").is_none() {
        command.env("CROSS_COMPILE", "aarch64-linux-gnu-");
    }
    command
}

fn run_command(mut command: Command, action: &str) -> Result<()> {
    let status = command
        .status()
        .map_err(|err| format!("spawn {action}: {err}"))?;
    if !status.success() {
        return Err(format!("{action} failed with {status}"));
    }
    Ok(())
}

fn parallel_jobs() -> usize {
    thread::available_parallelism().map_or(1, usize::from)
}

fn ensure_file(path: &Path, description: &str) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("missing {description}: {}", path.display()))
    }
}

fn kconfig_string(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))?;
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(ch),
        }
    }
    Ok(escaped)
}

struct BusyBoxInstall {
    root: PathBuf,
    binary: PathBuf,
}

fn build_busybox(target_dir: &Path, target: &str) -> Result<BusyBoxInstall> {
    let root = target_dir.join("busybox");
    let archive = root.join(format!("busybox-{BUSYBOX_VERSION}.tar.bz2"));
    let source_parent = root.join("src");
    let source = source_parent.join(format!("busybox-{BUSYBOX_VERSION}"));
    let build = root.join(format!("build-{target}"));
    let install = root.join(format!("install-{target}"));

    ensure_busybox_archive(&archive)?;
    ensure_busybox_source(&archive, &source_parent, &source)?;

    fs::create_dir_all(&build).map_err(|err| format!("create {}: {err}", build.display()))?;
    ensure_busybox_toolchain(&build, target)?;
    run_busybox_make(&source, &build, target, &["allnoconfig"])?;
    configure_busybox(&build.join(".config"))?;
    run_busybox_make(&source, &build, target, &["oldconfig"])?;
    run_busybox_make(
        &source,
        &build,
        target,
        &[&format!("-j{}", parallel_jobs()), "busybox"],
    )?;

    let binary = build.join("busybox");
    ensure_file(&binary, "busybox binary")?;
    strip_busybox(&binary, target)?;

    if install.exists() {
        fs::remove_dir_all(&install)
            .map_err(|err| format!("remove {}: {err}", install.display()))?;
    }
    fs::create_dir_all(&install).map_err(|err| format!("create {}: {err}", install.display()))?;
    run_busybox_make(
        &source,
        &build,
        target,
        &[&format!("CONFIG_PREFIX={}", install.display()), "install"],
    )?;

    let installed_binary = install.join("bin/busybox");
    ensure_file(&installed_binary, "installed busybox binary")?;
    Ok(BusyBoxInstall {
        root: install,
        binary: installed_binary,
    })
}

fn ensure_busybox_archive(archive: &Path) -> Result<()> {
    if archive.is_file() {
        verify_busybox_archive(archive)?;
        return Ok(());
    }

    if let Some(parent) = archive
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }

    let tmp = archive.with_extension("tar.bz2.tmp");
    let curl = env::var_os("CURL").unwrap_or_else(|| "curl".into());
    let mut command = Command::new(curl);
    command
        .args(["--fail", "--location", "--output"])
        .arg(&tmp)
        .arg(BUSYBOX_SOURCE_URL);
    run_command(command, "download busybox")?;
    verify_busybox_archive(&tmp)?;
    fs::rename(&tmp, archive)
        .map_err(|err| format!("rename {} to {}: {err}", tmp.display(), archive.display()))
}

fn verify_busybox_archive(path: &Path) -> Result<()> {
    let mut file = File::open(path).map_err(|err| format!("open {}: {err}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("read {}: {err}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let digest = format!("{:x}", hasher.finalize());
    if digest == BUSYBOX_ARCHIVE_SHA256 {
        Ok(())
    } else {
        Err(format!(
            "busybox archive checksum mismatch for {}: expected {BUSYBOX_ARCHIVE_SHA256}, got {digest}",
            path.display()
        ))
    }
}

fn ensure_busybox_source(archive: &Path, source_parent: &Path, source: &Path) -> Result<()> {
    if source.is_dir() {
        return Ok(());
    }

    fs::create_dir_all(source_parent)
        .map_err(|err| format!("create {}: {err}", source_parent.display()))?;
    let mut command = Command::new("tar");
    command
        .arg("-xjf")
        .arg(archive)
        .arg("-C")
        .arg(source_parent);
    run_command(command, "extract busybox")?;
    if source.is_dir() {
        Ok(())
    } else {
        Err(format!(
            "busybox source was not extracted: {}",
            source.display()
        ))
    }
}

fn configure_busybox(config: &Path) -> Result<()> {
    const ENABLED: &[&str] = &[
        "STATIC",
        "FEATURE_INSTALLER",
        "INSTALL_APPLET_SYMLINKS",
        "ASH",
        "SH_IS_ASH",
        "ASH_OPTIMIZE_FOR_SIZE",
        "ASH_BASH_COMPAT",
        "ASH_JOB_CONTROL",
        "ASH_ALIAS",
        "ASH_EXPAND_PRMT",
        "ASH_ECHO",
        "ASH_PRINTF",
        "ASH_TEST",
        "ASH_GETOPTS",
        "ASH_CMDCMD",
        "CTTYHACK",
        "FEATURE_SH_MATH",
        "FEATURE_SH_STANDALONE",
        "FEATURE_SH_NOFORK",
        "FEATURE_EDITING",
        "FEATURE_EDITING_WINCH",
        "FEATURE_EDITING_FANCY_PROMPT",
        "FEATURE_PREFER_APPLETS",
        "BUSYBOX",
        "CAT",
        "CHMOD",
        "CHOWN",
        "CP",
        "CUT",
        "DATE",
        "DD",
        "DF",
        "DIRNAME",
        "DMESG",
        "DU",
        "ECHO",
        "ENV",
        "FALSE",
        "FIND",
        "FREE",
        "GREP",
        "EGREP",
        "FGREP",
        "HEAD",
        "HEXDUMP",
        "ID",
        "KILL",
        "LN",
        "LOSETUP",
        "LS",
        "MKDIR",
        "MKNOD",
        "MKTEMP",
        "MOUNT",
        "MOUNTPOINT",
        "MV",
        "PIDOF",
        "PRINTENV",
        "PRINTF",
        "PS",
        "PWD",
        "READLINK",
        "REALPATH",
        "REBOOT",
        "RESET",
        "RM",
        "RMDIR",
        "SED",
        "SETSID",
        "SLEEP",
        "SORT",
        "STAT",
        "STTY",
        "STRINGS",
        "SYNC",
        "TAIL",
        "TAR",
        "TEE",
        "TEST",
        "TEST1",
        "TEST2",
        "TOUCH",
        "TR",
        "TRUE",
        "UMOUNT",
        "UNAME",
        "UNIQ",
        "WC",
        "WHOAMI",
        "XARGS",
        "BLKDISCARD",
        "BLKID",
        "BLOCKDEV",
        "FDISK",
        "FEATURE_FDISK_WRITABLE",
        "FEATURE_GPT_LABEL",
        "FSTRIM",
        "LSBLK",
        "MKDOSFS",
        "MKFS_VFAT",
        "MKSWAP",
        "SWAPON",
        "SWAPOFF",
        "FEATURE_MOUNT_FLAGS",
        "FEATURE_MOUNT_FSTAB",
        "FEATURE_MOUNT_LABEL",
        "FEATURE_MOUNT_LOOP",
        "FEATURE_MOUNT_LOOP_CREATE",
        "FEATURE_VOLUMEID_EXT",
        "FEATURE_VOLUMEID_FAT",
        "FEATURE_VOLUMEID_F2FS",
        "FEATURE_VOLUMEID_LINUXSWAP",
        "FEATURE_VOLUMEID_SQUASHFS",
    ];
    const DISABLED: &[&str] = &["HUSH", "SH_IS_HUSH", "SH_IS_NONE", "BASH_IS_ASH"];

    let mut contents = fs::read_to_string(config)
        .map_err(|err| format!("read busybox config {}: {err}", config.display()))?;
    for name in ENABLED {
        set_kconfig_bool(&mut contents, name, true);
    }
    for name in DISABLED {
        set_kconfig_bool(&mut contents, name, false);
    }
    set_kconfig_string(&mut contents, "BUSYBOX_EXEC_PATH", "/bin/busybox");
    set_kconfig_string(&mut contents, "PREFIX", "./_install");
    set_kconfig_string(&mut contents, "CROSS_COMPILER_PREFIX", "");
    set_kconfig_int(&mut contents, "FEATURE_EDITING_MAX_LEN", 1024);
    set_kconfig_int(&mut contents, "FEATURE_EDITING_HISTORY", 64);
    fs::write(config, contents)
        .map_err(|err| format!("write busybox config {}: {err}", config.display()))
}

fn set_kconfig_bool(contents: &mut String, name: &str, enabled: bool) {
    let enabled_line = format!("CONFIG_{name}=y");
    let disabled_line = format!("# CONFIG_{name} is not set");
    let mut found = false;
    let mut lines = Vec::new();
    for line in contents.lines() {
        if line == enabled_line || line == disabled_line {
            found = true;
            lines.push(if enabled {
                enabled_line.clone()
            } else {
                disabled_line.clone()
            });
        } else {
            lines.push(line.to_string());
        }
    }
    if !found {
        lines.push(if enabled { enabled_line } else { disabled_line });
    }
    *contents = lines.join("\n");
    contents.push('\n');
}

fn set_kconfig_string(contents: &mut String, name: &str, value: &str) {
    let prefix = format!("CONFIG_{name}=");
    let line = format!("CONFIG_{name}=\"{value}\"");
    let mut found = false;
    let mut lines = Vec::new();
    for existing in contents.lines() {
        if existing.starts_with(&prefix) || existing == format!("# CONFIG_{name} is not set") {
            found = true;
            lines.push(line.clone());
        } else {
            lines.push(existing.to_string());
        }
    }
    if !found {
        lines.push(line);
    }
    *contents = lines.join("\n");
    contents.push('\n');
}

fn set_kconfig_int(contents: &mut String, name: &str, value: u32) {
    let prefix = format!("CONFIG_{name}=");
    let line = format!("CONFIG_{name}={value}");
    let mut found = false;
    let mut lines = Vec::new();
    for existing in contents.lines() {
        if existing.starts_with(&prefix) || existing == format!("# CONFIG_{name} is not set") {
            found = true;
            lines.push(line.clone());
        } else {
            lines.push(existing.to_string());
        }
    }
    if !found {
        lines.push(line);
    }
    *contents = lines.join("\n");
    contents.push('\n');
}

fn run_busybox_make(source: &Path, build: &Path, target: &str, args: &[&str]) -> Result<()> {
    let mut command = Command::new(env::var_os("MAKE").unwrap_or_else(|| "make".into()));
    command
        .current_dir(source)
        .env("CROSS_COMPILE", busybox_cross_compile(target))
        .stdin(Stdio::null())
        .arg(format!("O={}", build.display()))
        .arg({
            let mut value = OsString::from("CC=");
            value.push(busybox_cc(target));
            value
        })
        .args(args);
    run_command(command, "make busybox")
}

fn ensure_busybox_toolchain(build: &Path, target: &str) -> Result<()> {
    let source = build.join("toolchain-check.c");
    let output = build.join("toolchain-check");
    fs::write(
        &source,
        "#include <byteswap.h>\n#include <sys/types.h>\nint main(void) { return bswap_32(0x12345678) == 0 ? 1 : 0; }\n",
    )
    .map_err(|err| format!("write {}: {err}", source.display()))?;

    let compiler = busybox_cc(target);
    let mut command = Command::new(&compiler);
    command
        .arg("-Os")
        .arg("-static")
        .arg(&source)
        .arg("-o")
        .arg(&output);

    let status = command.status().map_err(|err| {
        busybox_toolchain_error(target, &compiler, format!("run compiler: {err}"))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(busybox_toolchain_error(
            target,
            &compiler,
            format!("compiler check failed with {status}"),
        ))
    }
}

fn busybox_toolchain_error(target: &str, compiler: &OsString, reason: impl AsRef<str>) -> String {
    format!(
        "BusyBox requires a static libc-capable target C compiler for {target}; \
         tried {}; set BUSYBOX_CC or BUSYBOX_CROSS_COMPILE, install a matching musl toolchain, \
         or pass --no-busybox ({})",
        compiler.to_string_lossy(),
        reason.as_ref()
    )
}

fn busybox_cc(target: &str) -> OsString {
    if let Some(value) = env::var_os("BUSYBOX_CC") {
        return value;
    }
    let mut value = busybox_cross_compile(target);
    value.push("gcc");
    value
}

fn busybox_cross_compile(target: &str) -> OsString {
    if let Some(value) = env::var_os("BUSYBOX_CROSS_COMPILE") {
        return value;
    }
    match target {
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl-".into(),
        _ => env::var_os("CROSS_COMPILE").unwrap_or_default(),
    }
}

fn strip_busybox(binary: &Path, target: &str) -> Result<()> {
    let strip = if let Some(strip) = env::var_os("BUSYBOX_STRIP") {
        strip
    } else {
        let mut value = busybox_cross_compile(target);
        value.push("strip");
        value
    };
    let mut command = Command::new(strip);
    command.arg("--strip-all").arg(binary);
    run_command(command, "strip busybox")
}

fn write_initrd(init: &Path, busybox: Option<&BusyBoxInstall>, output: &Path) -> Result<()> {
    let mut writer = NewcWriter::create(output)?;
    writer.dir("dev", 0o755)?;
    writer.char_dev("dev/console", 0o600, 5, 1)?;
    writer.char_dev("dev/kmsg", 0o600, 1, 11)?;
    writer.char_dev("dev/null", 0o666, 1, 3)?;
    writer.dir("etc", 0o755)?;
    writer.dir("proc", 0o755)?;
    writer.dir("run", 0o755)?;
    writer.dir("sys", 0o755)?;
    writer.dir("tmp", 0o1777)?;
    if let Some(busybox) = busybox {
        writer.tree(&busybox.root)?;
        println!(
            "busybox {} ({} bytes)",
            busybox.binary.display(),
            fs::metadata(&busybox.binary)
                .map_err(|err| format!("stat {}: {err}", busybox.binary.display()))?
                .len()
        );
    }
    writer.file("init", init, 0o755)?;
    writer.finish()
}

fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "xtask manifest directory has no parent".to_string())
}

fn target_dir(workspace_root: &Path) -> PathBuf {
    match env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        }
        None => workspace_root.join("target"),
    }
}

struct NewcWriter {
    file: File,
    offset: u64,
    ino: u32,
}

impl NewcWriter {
    fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).map_err(|err| format!("create {}: {err}", path.display()))?;
        Ok(Self {
            file,
            offset: 0,
            ino: 1,
        })
    }

    fn dir(&mut self, name: &str, mode: u32) -> Result<()> {
        self.entry(name, 0o040000 | mode, 2, 0, 0, &[])
    }

    fn file(&mut self, name: &str, path: &Path, mode: u32) -> Result<()> {
        let contents = fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        self.entry(name, 0o100000 | mode, 1, 0, 0, &contents)
    }

    fn file_with_mode(&mut self, name: &str, path: &Path, mode: u32) -> Result<()> {
        let contents = fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        self.entry(name, 0o100000 | mode, 1, 0, 0, &contents)
    }

    fn symlink(&mut self, name: &str, target: &Path) -> Result<()> {
        self.entry(
            name,
            0o120000 | 0o777,
            1,
            0,
            0,
            target.as_os_str().as_bytes(),
        )
    }

    fn char_dev(&mut self, name: &str, mode: u32, major: u32, minor: u32) -> Result<()> {
        self.entry(name, 0o020000 | mode, 1, major, minor, &[])
    }

    fn tree(&mut self, root: &Path) -> Result<()> {
        self.tree_dir(root, root)
    }

    fn tree_dir(&mut self, root: &Path, dir: &Path) -> Result<()> {
        let mut entries = fs::read_dir(dir)
            .map_err(|err| format!("read directory {}: {err}", dir.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| format!("read directory entry under {}: {err}", dir.display()))?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|err| {
                format!(
                    "strip {} prefix from {}: {err}",
                    root.display(),
                    path.display()
                )
            })?;
            let name = cpio_name(relative)?;
            let metadata = fs::symlink_metadata(&path)
                .map_err(|err| format!("stat {}: {err}", path.display()))?;
            let mode = metadata.permissions().mode() & 0o7777;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                self.dir(&name, mode)?;
                self.tree_dir(root, &path)?;
            } else if file_type.is_symlink() {
                let target = fs::read_link(&path)
                    .map_err(|err| format!("readlink {}: {err}", path.display()))?;
                self.symlink(&name, &target)?;
            } else if file_type.is_file() {
                self.file_with_mode(&name, &path, mode)?;
            } else {
                return Err(format!(
                    "unsupported file type in initrd tree: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.entry("TRAILER!!!", 0, 1, 0, 0, &[])?;
        self.file
            .flush()
            .map_err(|err| format!("flush initrd: {err}"))
    }

    fn entry(
        &mut self,
        name: &str,
        mode: u32,
        nlink: u32,
        rdevmajor: u32,
        rdevminor: u32,
        contents: &[u8],
    ) -> Result<()> {
        if name.starts_with('/') {
            return Err(format!("cpio entry must be relative: {name}"));
        }

        let namesize = name.len() + 1;
        let filesize =
            u32::try_from(contents.len()).map_err(|_| format!("cpio entry too large: {name}"))?;
        let header = format!(
            "070701{ino:08x}{mode:08x}{uid:08x}{gid:08x}{nlink:08x}{mtime:08x}{filesize:08x}{devmajor:08x}{devminor:08x}{rdevmajor:08x}{rdevminor:08x}{namesize:08x}{check:08x}",
            ino = self.ino,
            mode = mode,
            uid = 0,
            gid = 0,
            nlink = nlink,
            mtime = source_date_epoch(),
            filesize = filesize,
            devmajor = 0,
            devminor = 0,
            rdevmajor = rdevmajor,
            rdevminor = rdevminor,
            namesize = namesize,
            check = 0,
        );

        self.write_all(header.as_bytes())?;
        self.write_all(name.as_bytes())?;
        self.write_all(&[0])?;
        self.pad_to_4()?;
        self.write_all(contents)?;
        self.pad_to_4()?;
        self.ino = self.ino.wrapping_add(1);
        Ok(())
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.file
            .write_all(bytes)
            .map_err(|err| format!("write initrd: {err}"))?;
        self.offset += bytes.len() as u64;
        Ok(())
    }

    fn pad_to_4(&mut self) -> Result<()> {
        let pad = (4 - (self.offset % 4)) % 4;
        if pad != 0 {
            self.write_all(&vec![0; pad as usize])?;
        }
        Ok(())
    }
}

fn source_date_epoch() -> u32 {
    env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
}

fn cpio_name(path: &Path) -> Result<String> {
    let name = path
        .to_str()
        .ok_or_else(|| format!("cpio path is not valid UTF-8: {}", path.display()))?;
    if name.is_empty()
        || name.starts_with('/')
        || name.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        Err(format!("invalid cpio path: {name}"))
    } else {
        Ok(name.to_string())
    }
}

fn print_usage() {
    println!(
        "usage: cargo xtask <command>\n\ncommands:\n  cpio      build pocketboot and create an initrd cpio\n  kernel    build a pocketboot kernel image for one device\n  bootimg   package an already-built pocketboot kernel as boot.img\n  qemu      build and boot pocketboot under qemu-system-aarch64"
    );
}

fn print_cpio_usage() {
    println!(
        "usage: cargo xtask cpio [--target TRIPLE] [--output PATH] [--no-busybox]\n\ndefault target: {DEFAULT_TARGET}\ndefault output: target/{DEFAULT_INITRD}\nbusybox: official {BUSYBOX_VERSION} release, built statically unless --no-busybox is used"
    );
}

fn print_kernel_usage() {
    println!(
        "usage: cargo xtask kernel [--initrd PATH] <vendor/device> <kernel-tree>\n\nexample: cargo xtask kernel qcom/msm8916-samsung-a5u-eur ./linux\n\nwhen --initrd is omitted, target/{DEFAULT_INITRD} is rebuilt automatically\noutputs: target/kernel/<vendor>/<device>/arch/arm64/boot/Image.gz and the inferred DTB"
    );
}

fn print_bootimg_usage() {
    println!(
        "usage: cargo xtask bootimg <vendor/device> [--output PATH]\n\nexample: cargo xtask bootimg qcom/sdm670-google-sargo\n\nrequires: target/kernel/<vendor>/<device>/arch/arm64/boot/Image.gz and the inferred DTB\ndefault output: target/kernel/<vendor>/<device>/boot.img"
    );
}

fn print_qemu_usage() {
    println!(
        "usage: cargo xtask qemu [--build-only] <kernel-tree> [-- QEMU-ARG...]\n\nexample: cargo xtask qemu ./linux\n\nbuilds: target/kernel/qemu/{QEMU_TARGET}/arch/arm64/boot/Image\nruns: qemu-system-aarch64 -machine virt -nographic"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qcdt_records_point_to_page_aligned_dtb() {
        let qcdt = build_qcdt(
            &QcdtConfig {
                entries: vec![QcdtEntry {
                    msm_id: [206, 0],
                    board_id: [0xce08ff01, 1],
                }],
            },
            16,
            b"dtb",
            Path::new("test.toml"),
        )
        .unwrap();

        assert_eq!(&qcdt[..4], b"QCDT");
        assert_eq!(u32_at(&qcdt, 4), 2);
        assert_eq!(u32_at(&qcdt, 8), 1);
        assert_eq!(u32_at(&qcdt, 12), 206);
        assert_eq!(u32_at(&qcdt, 16), 0xce08ff01);
        assert_eq!(u32_at(&qcdt, 20), 1);
        assert_eq!(u32_at(&qcdt, 24), 0);
        assert_eq!(u32_at(&qcdt, 28), 48);
        assert_eq!(u32_at(&qcdt, 32), 16);
        assert_eq!(&qcdt[48..51], b"dtb");
        assert_eq!(qcdt.len(), 64);
    }

    #[test]
    fn samsung_a5u_eur_config_is_legacy_qcdt() {
        let config = load_bootimg_config(
            &workspace_root()
                .unwrap()
                .join("configs/bootimg/qcom/msm8916-samsung-a5u-eur.toml"),
        )
        .unwrap();

        let qcdt = config.qcdt.unwrap();
        assert_eq!(config.header_version, 0);
        assert_eq!(config.page_size, 2048);
        assert_eq!(config.base, 0x80000000);
        assert_eq!(config.ramdisk_size, 1);
        assert!(config.append_seandroid_enforce);
        assert_eq!(qcdt.entries.len(), 1);
        assert_eq!(qcdt.entries[0].msm_id, [206, 0]);
        assert_eq!(qcdt.entries[0].board_id, [0xce08ff01, 1]);
    }

    fn u32_at(data: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
    }
}
