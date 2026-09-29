//! QEMU PID 1 exercising the real pocketboot loader; see tools/arm32-kexec.
#[allow(dead_code)]
#[path = "../src/kexec.rs"]
mod kexec;
#[allow(dead_code)]
#[path = "../src/pe.rs"]
mod pe;
#[allow(dead_code)]
#[path = "../src/zboot.rs"]
mod zboot;

use std::{
    ffi::CString,
    fs::{self, File},
    io::{self, Write},
    path::Path,
};

const DT: &str = "/sys/firmware/devicetree/base";
const SENTINEL: &[u8] = b"pocketboot destination initramfs\n";

fn require(condition: bool, message: &str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message))
    }
}

fn mount(path: &str, kind: &str) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let path = CString::new(path)?;
    let kind = CString::new(kind)?;
    let result = unsafe {
        libc::mount(
            kind.as_ptr(),
            path.as_ptr(),
            kind.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn parameter<'a>(cmdline: &'a str, prefix: &str) -> io::Result<&'a str> {
    cmdline
        .split_ascii_whitespace()
        .find_map(|word| word.strip_prefix(prefix))
        .ok_or_else(|| io::Error::other(format!("missing cmdline parameter {prefix}")))
}

fn check_memory() -> io::Result<()> {
    // QEMU virt -m 256M: two address cells and two size cells, big-endian.
    require(
        fs::read(format!("{DT}/#address-cells"))? == 2u32.to_be_bytes()
            && fs::read(format!("{DT}/#size-cells"))? == 2u32.to_be_bytes(),
        "unexpected virt DT cell sizes",
    )?;
    let expected: Vec<u8> = [0u32, 0x4000_0000, 0, 0x1000_0000]
        .into_iter()
        .flat_map(u32::to_be_bytes)
        .collect();
    let mut memories = 0;
    for entry in fs::read_dir(DT)? {
        let path = entry?.path();
        if fs::read(path.join("device_type")).ok().as_deref() == Some(b"memory\0") {
            require(fs::read(path.join("reg"))? == expected, "stale DT memory")?;
            memories += 1;
        }
    }
    require(memories == 1, "expected exactly one virt memory node")
}

fn run() -> io::Result<()> {
    require(
        std::process::id() == 1,
        "this program must run as guest PID 1",
    )?;
    mount("/proc", "proc")?;
    mount("/sys", "sysfs")?;
    mount("/dev", "devtmpfs")?;
    let cmdline = fs::read_to_string("/proc/cmdline")?;
    let stage = parameter(&cmdline, "pocketboot.smoke.stage=")?;
    let case = parameter(&cmdline, "pocketboot.smoke.case=")?;
    require(
        matches!(case, "fallback" | "supplied"),
        "unknown smoke case",
    )?;
    check_memory()?;

    match stage {
        "source" => {
            require(
                !Path::new("/sentinel").exists(),
                "source has destination sentinel",
            )?;
            let dtb = if case == "supplied" {
                Some(File::open("/supplied.dtb")?)
            } else {
                None
            };
            println!("ARM32-KEXEC-SMOKE: LOAD case={case}");
            let destination_cmdline = format!(
                "console=ttyAMA0 rdinit=/init panic=-1 \
                 pocketboot.smoke.stage=destination pocketboot.smoke.case={case}"
            );
            kexec::KexecImage::new(
                File::open("/destination.zImage")?,
                Some(File::open("/destination.cpio.gz")?),
                dtb,
                &destination_cmdline,
            )?
            .load()?;
            io::stdout().flush()?;
            kexec::exec_loaded_image()
        }
        "destination" => {
            require(
                fs::read("/sentinel")? == SENTINEL,
                "destination initrd sentinel mismatch",
            )?;
            require(
                !Path::new("/destination.zImage").exists(),
                "destination booted source initrd",
            )?;
            require(
                Path::new(&format!("{DT}/chosen/linux,booted-from-kexec")).exists(),
                "missing DT /chosen/linux,booted-from-kexec",
            )?;
            require(
                !Path::new(&format!("{DT}/chosen/pocketboot,stale-appended-dtb")).exists(),
                "kernel consumed the stale appended DTB",
            )?;
            // Verify the supplied-DTB case really used that blob, not the fallback.
            require(
                Path::new(&format!("{DT}/chosen/pocketboot,supplied-dtb")).exists()
                    == (case == "supplied"),
                "wrong DTB source",
            )?;
            println!("ARM32-KEXEC-SMOKE: PASS case={case}");
            io::stdout().flush()?;
            unsafe { libc::reboot(libc::RB_POWER_OFF) };
            Err(io::Error::last_os_error())
        }
        _ => Err(io::Error::other("unknown smoke stage")),
    }
}

fn main() {
    tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .init();
    if let Err(error) = run() {
        eprintln!("ARM32-KEXEC-SMOKE: FAIL: {error}");
    }
    // Never return from PID 1. Leave failure output intact for the host timeout.
    loop {
        unsafe { libc::pause() };
    }
}
