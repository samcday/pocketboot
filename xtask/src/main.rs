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
    #[command(about = "build all configured pocketboot artifacts for one device")]
    Build(commands::build::BuildArgs),
    #[command(about = "build BusyBox for initrd use")]
    Busybox(commands::busybox::BusyBoxArgs),
    #[command(about = "build pocketboot and create a device initrd cpio")]
    Initrd(commands::cpio::InitrdArgs),
    #[command(name = "ci-matrix", about = "emit the configured CI matrix as JSON")]
    CiMatrix(commands::ci_matrix::CiMatrixArgs),
    #[command(
        name = "kernel-build",
        about = "prepare, module-build, or image-build a pocketboot kernel"
    )]
    KernelBuild(commands::kernel::KernelBuildArgs),
    #[command(about = "build a pocketpreboot shim for one device")]
    Preboot(commands::preboot::PrebootArgs),
    #[command(about = "package an already-built pocketboot kernel as boot.img")]
    Bootimg(commands::bootimg::BootImgArgs),
    #[command(about = "build and boot pocketboot under qemu-system-aarch64")]
    Qemu(commands::qemu::QemuArgs),
}

fn run() -> Result<()> {
    match Cli::parse().command {
        XtaskCommand::Build(args) => commands::build::run(args),
        XtaskCommand::Busybox(args) => commands::busybox::run(args),
        XtaskCommand::Initrd(args) => commands::cpio::run(args),
        XtaskCommand::CiMatrix(args) => commands::ci_matrix::run(args),
        XtaskCommand::KernelBuild(args) => commands::kernel::run(args),
        XtaskCommand::Preboot(args) => commands::preboot::run(args),
        XtaskCommand::Bootimg(args) => commands::bootimg::run(args),
        XtaskCommand::Qemu(args) => commands::qemu::run(args),
    }
}
