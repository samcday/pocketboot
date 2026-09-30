use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
};

use crate::Result;

use super::{
    ensure_file,
    kernel::{self, KernelBuild},
    kernel_tree, run_command, target_dir, workspace_root,
};

const QEMU_DEVICE: &str = "qemu/aarch64-virt";
const QEMU_TARGET: &str = "aarch64-virt";
const QEMU_DISK_SIZE: u64 = 64 * 1024 * 1024;
/// Console and log setup shared with `qemu-boot-policy`.
pub(super) const QEMU_CONSOLE: &str =
    "console=ttyAMA0 earlycon=pl011,mmio32,0x09000000 loglevel=7 pocketboot.log=info";

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
    let build = build_qemu_kernel(&workspace_root, &args.kernel_tree)?;
    let disk = qemu_disk(&target_dir(&workspace_root))?;

    println!("initrd {}", build.initrd.display());
    println!("image {}", build.image.display());
    println!("disk {}", disk.display());
    println!("config {}", build.config.display());

    if args.build_only {
        return Ok(());
    }

    run_qemu(&workspace_root, &build.image, &disk, &args.qemu_args)
}

pub(super) fn build_qemu_kernel(workspace_root: &Path, tree: &Path) -> Result<KernelBuild> {
    let tree = kernel_tree(tree)?;
    kernel::build_device_kernel_id(workspace_root, &tree, QEMU_DEVICE, None)
}

pub(super) fn qemu_binary() -> OsString {
    env::var_os("QEMU").unwrap_or_else(|| "qemu-system-aarch64".into())
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
    let qemu = qemu_binary();
    let drive = format!("if=none,id=pocketboot,format=raw,file={}", disk.display());
    let append = format!("{QEMU_CONSOLE} panic=1");

    println!("USB/IP guest server will be forwarded to 127.0.0.1:3240");
    println!(
        "host attach: sudo modprobe vhci-hcd && sudo usbip attach -r 127.0.0.1 -d usbip-vudc.0"
    );
    println!(
        "vol-down break-in: pass -- -qmp tcp:127.0.0.1:4444,server=on,wait=off and hold qcode \"volumedown\" with input-send-event"
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
        .args(["-append", &append, "-drive"])
        .arg(drive)
        .args(["-device", "virtio-blk-device,drive=pocketboot"])
        .args(["-netdev", "user,id=net0,hostfwd=tcp:127.0.0.1:3240-:3240"])
        .args(["-device", "virtio-net-device,netdev=net0"])
        .args(["-global", "virtio-mmio.force-legacy=false"])
        .args(["-device", "virtio-gpu-device"])
        .args(["-device", "virtio-keyboard-device"])
        .args(extra_args);
    run_command(command, "qemu")
}
