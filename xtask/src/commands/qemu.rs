use std::{
    env,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
};

use crate::Result;

use super::{
    KERNEL_ARCH,
    cpio::{DEFAULT_TARGET, build_initrd},
    ensure_file, kernel_tree, make_command, parallel_jobs, run_command, target_dir, workspace_root,
};

const QEMU_TARGET: &str = "aarch64-virt";
const QEMU_DISK_SIZE: u64 = 64 * 1024 * 1024;

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

pub(crate) fn run(args: Vec<String>) -> Result<()> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print_usage();
        Ok(())
    } else {
        qemu(QemuArgs::parse(args)?)
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

fn print_usage() {
    println!(
        "usage: cargo xtask qemu [--build-only] <kernel-tree> [-- QEMU-ARG...]\n\nexample: cargo xtask qemu ./linux\n\nbuilds: target/kernel/qemu/{QEMU_TARGET}/arch/arm64/boot/Image\nruns: qemu-system-aarch64 -machine virt -nographic"
    );
}
