use std::{
    env,
    ffi::OsString,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    thread,
};

const DEFAULT_TARGET: &str = "aarch64-unknown-linux-musl";
const INIT_BINARY: &str = "pocketboot";
const DEFAULT_INITRD: &str = "pocketboot-initrd.cpio";
const KERNEL_ARCH: &str = "arm64";

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
}

impl CpioArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut target = DEFAULT_TARGET.to_string();
        let mut output = None;
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

        Ok(Self { target, output })
    }
}

fn cpio(args: CpioArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let output = build_initrd(&workspace_root, &args.target, args.output)?;
    println!("wrote {}", output.display());
    Ok(())
}

fn build_initrd(workspace_root: &Path, target: &str, output: Option<PathBuf>) -> Result<PathBuf> {
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

    write_initrd(&init, &output)?;
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
struct KernelArgs {
    device: KernelDevice,
    kernel_tree: PathBuf,
}

impl KernelArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        if args.len() != 2 {
            return Err("usage: cargo xtask kernel <vendor/device> <kernel-tree>".to_string());
        }

        Ok(Self {
            device: KernelDevice::parse(&args[0])?,
            kernel_tree: PathBuf::from(&args[1]),
        })
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

    let initrd = build_initrd(&workspace_root, DEFAULT_TARGET, None)?;
    println!("wrote {}", initrd.display());

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

fn make_command(kernel_tree: &Path, out_dir: &Path) -> Command {
    let make = env::var_os("MAKE").unwrap_or_else(|| "make".into());
    let mut output = OsString::from("O=");
    output.push(out_dir.as_os_str());

    let mut command = Command::new(make);
    command
        .current_dir(kernel_tree)
        .env("ARCH", KERNEL_ARCH)
        .arg(output);
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

fn write_initrd(init: &Path, output: &Path) -> Result<()> {
    let mut writer = NewcWriter::create(output)?;
    writer.dir("dev", 0o755)?;
    writer.char_dev("dev/console", 0o600, 5, 1)?;
    writer.dir("proc", 0o755)?;
    writer.dir("run", 0o755)?;
    writer.dir("sys", 0o755)?;
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

    fn char_dev(&mut self, name: &str, mode: u32, major: u32, minor: u32) -> Result<()> {
        self.entry(name, 0o020000 | mode, 1, major, minor, &[])
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

fn print_usage() {
    println!(
        "usage: cargo xtask <command>\n\ncommands:\n  cpio      build pocketboot and create an initrd cpio\n  kernel    build a pocketboot kernel image for one device"
    );
}

fn print_cpio_usage() {
    println!(
        "usage: cargo xtask cpio [--target TRIPLE] [--output PATH]\n\ndefault target: {DEFAULT_TARGET}\ndefault output: target/{DEFAULT_INITRD}"
    );
}

fn print_kernel_usage() {
    println!(
        "usage: cargo xtask kernel <vendor/device> <kernel-tree>\n\nexample: cargo xtask kernel qcom/msm8916-samsung-a5u-eur ./linux\n\noutputs: target/kernel/<vendor>/<device>/arch/arm64/boot/Image.gz and the inferred DTB"
    );
}
