use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod commands;

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

#[derive(Debug, Parser)]
#[command(bin_name = "cargo xtask")]
#[command(about = "pocketboot build helper")]
#[command(arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: XtaskCommand,
}

#[derive(Debug, Subcommand)]
enum XtaskCommand {
    #[command(about = "build BusyBox for initrd use")]
    Busybox(commands::busybox::BusyBoxArgs),
    #[command(about = "build pocketboot and create an initrd cpio")]
    Cpio(commands::cpio::CpioArgs),
    #[command(about = "build a pocketboot kernel image for one device")]
    Kernel(commands::kernel::KernelArgs),
    #[command(
        name = "kernel-src",
        about = "fetch or update a configured kernel source tree"
    )]
    KernelSrc(commands::kernel_src::KernelSrcArgs),
    #[command(about = "package an already-built pocketboot kernel as boot.img")]
    Bootimg(commands::bootimg::BootImgArgs),
    #[command(about = "build and boot pocketboot under qemu-system-aarch64")]
    Qemu(commands::qemu::QemuArgs),
}

fn run() -> Result<()> {
    match Cli::parse().command {
        XtaskCommand::Busybox(args) => commands::busybox::run(args),
        XtaskCommand::Cpio(args) => commands::cpio::run(args),
        XtaskCommand::Kernel(args) => commands::kernel::run(args),
        XtaskCommand::KernelSrc(args) => commands::kernel_src::run(args),
        XtaskCommand::Bootimg(args) => commands::bootimg::run(args),
        XtaskCommand::Qemu(args) => commands::qemu::run(args),
    }
}
