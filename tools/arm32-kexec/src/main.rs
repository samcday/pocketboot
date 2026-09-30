//! QEMU PID 1 for the ARM32 two-kernel kexec smoke test; see
//! `tools/arm32-kexec/README.md`.
//!
//! Built as the standalone `arm32-kexec-smoke` binary in this directory. The
//! real pocketboot loader is included through `#[path]`; there is no substitute
//! loader and no syscall shim.
#[allow(dead_code)]
#[path = "../../../src/kexec.rs"]
mod kexec;
#[allow(dead_code)]
#[path = "../../../src/pe.rs"]
mod pe;
#[allow(dead_code)]
#[path = "../../../src/zboot.rs"]
mod zboot;

use std::{
    ffi::CString,
    fs::{self, File},
    io::{self, Write},
    os::unix::fs::FileExt,
    path::Path,
};

/// Mount point of the kernel's device tree in the guest.
const DT: &str = "/sys/firmware/devicetree/base";
/// Written by `tools/arm32-kexec/initramfs.py` into the destination initrd
/// only, so finding it proves the loader passed the destination initrd.
const SENTINEL: &[u8] = b"pocketboot destination initramfs\n";
/// Fixture DTB property whose value names the DTB the loader selected.
const DTB_SOURCE: &str = "pocketboot,dtb-source";
/// QEMU `virt`: 256 MiB of live RAM at `0x40000000`. The fixture DTBs carry a
/// deliberately stale map that the loader's live-memory graft must correct.
const MEMORY: [u32; 4] = [0, 0x4000_0000, 0, 0x1000_0000];
/// Values of `pocketboot.smoke.case=`; also the `source-<case>.cpio.gz` names.
const CASES: [&str; 3] = ["fallback", "supplied", "appended"];

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

/// The command line the source stage hands to the loader. The destination
/// stage compares `/proc/cmdline` against it, so an unpatched DTB (live tree or
/// a fixture blob) cannot pass by carrying the source command line.
fn destination_cmdline(case: &str) -> String {
    format!(
        "console=ttyAMA0 rdinit=/init panic=-1 \
         pocketboot.smoke.stage=destination pocketboot.smoke.case={case}"
    )
}

/// DTB source the loader must select for `case`: `/chosen/{DTB_SOURCE}` is the
/// live tree (no property) in the fallback case, and the fixture marker value
/// otherwise.
fn expected_dtb_source(case: &str) -> Option<&'static [u8]> {
    match case {
        "supplied" => Some(b"supplied\0"),
        "appended" => Some(b"appended\0"),
        _ => None,
    }
}

/// Device tree property value, or `None` when the property does not exist.
fn dt_property(path: &str) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn source_label(value: Option<&[u8]>) -> String {
    match value {
        None => "the live tree (no marker)".to_string(),
        Some(value) => format!("marker {:?}", String::from_utf8_lossy(value)),
    }
}

fn check_dtb_source(case: &str) -> io::Result<()> {
    let found = dt_property(&format!("{DT}/chosen/{DTB_SOURCE}"))?;
    let expected = expected_dtb_source(case);
    require(
        found.as_deref() == expected,
        &format!(
            "wrong DTB source for case={case}: loader selected {} but this case needs {} (/chosen/{DTB_SOURCE})",
            source_label(found.as_deref()),
            source_label(expected),
        ),
    )
}

fn check_memory() -> io::Result<()> {
    // QEMU virt -m 256M: two address cells and two size cells, big-endian.
    require(
        fs::read(format!("{DT}/#address-cells"))? == 2u32.to_be_bytes()
            && fs::read(format!("{DT}/#size-cells"))? == 2u32.to_be_bytes(),
        "unexpected virt DT cell sizes",
    )?;
    let expected: Vec<u8> = MEMORY.into_iter().flat_map(u32::to_be_bytes).collect();
    let mut memories = 0;
    for entry in fs::read_dir(DT)? {
        let path = entry?.path();
        if fs::read(path.join("device_type")).ok().as_deref() == Some(b"memory\0") {
            // Any other map means the loader passed a fixture DTB's stale
            // memory instead of grafting the live RAM.
            require(fs::read(path.join("reg"))? == expected, "stale DT memory")?;
            memories += 1;
        }
    }
    require(memories == 1, "expected exactly one virt memory node")
}

/// The destination zImage is built with `CONFIG_EFI`, so it carries the EFI-stub
/// wrapper: an `MZ`/PE32 image that production has to recognise as an ARM32
/// zImage instead of rejecting as a foreign PE/COFF payload. Refuse to run a
/// plain zImage where that coverage would silently disappear.
fn require_efi_stub(kernel: &File) -> io::Result<()> {
    let mut header = [0u8; 2];
    kernel.read_exact_at(&mut header, 0)?;
    require(
        &header == b"MZ",
        "destination zImage is not an EFI-stub (MZ/PE32) payload; build the tiny kernel with CONFIG_EFI=y",
    )
}

/// Name the failing production step in the serial FAIL record, so a payload
/// preparation error is told apart from a later loader rejection.
fn prepare_failure(error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("prepare_kernel_payload: {error}"))
}

/// Source stage: run payload preparation and the real loader, then hand off.
/// Returns only when the loader or the kexec syscall failed.
fn source_stage(case: &str) -> io::Result<()> {
    require(
        !Path::new("/sentinel").exists(),
        "source initrd carries the destination sentinel",
    )?;
    // The host bakes the explicit DTB fixture into the source initrd only for
    // the case that supplies one.
    require(
        Path::new("/supplied.dtb").exists() == (case == "supplied"),
        "source initrd DTB fixtures do not match the case",
    )?;
    let dtb = match case == "supplied" {
        true => Some(File::open("/supplied.dtb")?),
        false => None,
    };
    println!("ARM32-KEXEC-SMOKE: LOAD case={case}");
    io::stdout().flush()?;
    // Both production callers prepare the kernel payload before the loader;
    // skipping this step left the EFI-stub and gzip branches untested.
    let kernel = kexec::prepare_kernel_payload(File::open("/destination.zImage")?)
        .map_err(prepare_failure)?;
    require_efi_stub(&kernel)?;
    kexec::KexecImage::new(
        kernel,
        Some(File::open("/destination.cpio.gz")?),
        dtb,
        &destination_cmdline(case),
    )?
    .load()?;
    io::stdout().flush()?;
    kexec::exec_loaded_image()
}

/// Destination stage: the kernel the loader started must see the destination
/// initrd, the loader's patched `/chosen`, live RAM and the DTB source this
/// case selects. Returns only when a check failed.
fn destination_stage(case: &str, cmdline: &str) -> io::Result<()> {
    require(
        fs::read("/sentinel")? == SENTINEL,
        "destination initrd sentinel mismatch",
    )?;
    require(
        !Path::new("/destination.zImage").exists(),
        "destination booted the source initrd",
    )?;
    check_memory()?;
    check_dtb_source(case)?;
    // The checks below depend on loader-produced state: the live-memory graft,
    // the patched `/chosen` and the selected DTB. The live tree and both fixture
    // blobs each lack at least one of them.
    require(
        dt_property(&format!("{DT}/chosen/linux,booted-from-kexec"))?.is_some(),
        "destination /chosen was not patched by the loader",
    )?;
    let expected = destination_cmdline(case);
    require(
        cmdline
            .split_ascii_whitespace()
            .eq(expected.split_ascii_whitespace()),
        "destination command line is not the loader's patched bootargs",
    )?;
    println!("ARM32-KEXEC-SMOKE: PASS case={case}");
    io::stdout().flush()
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
    require(CASES.contains(&case), &format!("unknown smoke case {case}"))?;

    match stage {
        "source" => source_stage(case),
        "destination" => destination_stage(case, &cmdline),
        _ => Err(io::Error::other(format!("unknown smoke stage {stage}"))),
    }
}

/// True only for the QEMU PID 1 that `run.sh` boots: PID 1 whose kernel
/// command line carries the harness marker. A stray host invocation (or a
/// failure before `run()` mounted `/proc`) returns false and must never reboot
/// or power off the machine.
fn qemu_guest() -> bool {
    std::process::id() == 1
        && fs::read_to_string("/proc/cmdline")
            .is_ok_and(|cmdline| cmdline.contains("pocketboot.smoke.stage="))
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
    if !qemu_guest() {
        // A normal host invocation has no QEMU to end and no PASS record to
        // make meaningful.
        std::process::exit(1);
    }
    // A proven guest powers off instead of pausing, so the host sees the
    // FAIL/PASS record in seconds rather than waiting out SMOKE_TIMEOUT.
    unsafe { libc::reboot(libc::RB_POWER_OFF) };
    eprintln!(
        "ARM32-KEXEC-SMOKE: FAIL: power off failed: {}",
        io::Error::last_os_error()
    );
    // PID 1 must not return; the host's per-case timeout bounds the wait.
    loop {
        unsafe { libc::pause() };
    }
}
