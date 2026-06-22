use std::{
    env,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
};

use crate::Result;

use super::{ensure_file, kernel, kernel_tree, run_command, target_dir, workspace_root};

const QEMU_DEVICE: &str = "qemu/aarch64-virt";
const QEMU_TARGET: &str = "aarch64-virt";
const QEMU_DISK_SIZE: u64 = 64 * 1024 * 1024;

#[derive(clap::Args, Debug)]
pub(crate) struct QemuArgs {
    #[arg(value_name = "KERNEL_TREE")]
    kernel_tree: PathBuf,
    #[arg(long)]
    build_only: bool,
    #[arg(last = true, value_name = "QEMU_ARG")]
    qemu_args: Vec<String>,
}

pub(crate) fn run(args: QemuArgs) -> Result<()> {
    qemu(args)
}

fn qemu(args: QemuArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let kernel_tree = kernel_tree(&args.kernel_tree)?;
    let target_dir = target_dir(&workspace_root);
    let build = kernel::build_device_kernel_id(&workspace_root, &kernel_tree, QEMU_DEVICE, None)?;
    let disk = qemu_disk(&target_dir)?;

    println!("initrd {}", build.initrd.display());
    println!("image {}", build.image.display());
    println!("disk {}", disk.display());
    println!("config {}", build.config.display());

    if args.build_only {
        return Ok(());
    }

    run_qemu(&workspace_root, &build.image, &disk, &args.qemu_args)
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

fn run_qemu(workspace_root: &Path, image: &Path, disk: &Path, extra_args: &[String]) -> Result<()> {
    let qemu = env::var_os("QEMU").unwrap_or_else(|| "qemu-system-aarch64".into());
    let drive = format!("if=none,id=pocketboot,format=raw,file={}", disk.display());
    let append =
        "console=ttyAMA0 earlycon=pl011,mmio32,0x09000000 loglevel=7 panic=1 pocketboot.log=info";

    println!("USB/IP guest server will be forwarded to 127.0.0.1:3240");
    println!(
        "host attach: sudo modprobe vhci-hcd && sudo usbip attach -r 127.0.0.1 -d usbip-vudc.0"
    );

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
        .args(["-append", append, "-drive"])
        .arg(drive)
        .args(["-device", "virtio-blk-device,drive=pocketboot"])
        .args(["-netdev", "user,id=net0,hostfwd=tcp:127.0.0.1:3240-:3240"])
        .args(["-device", "virtio-net-device,netdev=net0"])
        .args(extra_args);
    run_command(command, "qemu")
}
