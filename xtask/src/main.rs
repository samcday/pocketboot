use std::{env, process::ExitCode};

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

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("busybox") => commands::busybox::run(args.collect()),
        Some("cpio") => commands::cpio::run(args.collect()),
        Some("kernel") => commands::kernel::run(args.collect()),
        Some("bootimg") => commands::bootimg::run(args.collect()),
        Some("qemu") => commands::qemu::run(args.collect()),
        Some("help" | "--help" | "-h") | None => {
            commands::print_usage();
            Ok(())
        }
        Some(command) => Err(format!("unknown xtask command: {command}")),
    }
}
