#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(target_os = "none")]
use core::panic::PanicInfo;

#[cfg(all(target_os = "none", not(feature = "debug-ram-trace")))]
core::arch::global_asm!(include_str!("start.S"));
#[cfg(all(target_os = "none", not(feature = "debug-ram-trace")))]
core::arch::global_asm!(include_str!("exceptions.S"));
#[cfg(all(target_os = "none", feature = "debug-ram-trace"))]
core::arch::global_asm!(
    ".set PB_RAM_TRACE, 1",
    include_str!("start.S"),
    include_str!("exceptions.S"),
    ".section .pb_ram_trace, \"aw\", %nobits",
    ".balign 4096",
    ".global pocketpreboot_ram_trace",
    "pocketpreboot_ram_trace: .skip 12288",
);

#[cfg(all(target_os = "none", feature = "debug-ram-trace"))]
mod ram_trace;

#[cfg(any(all(target_os = "none", feature = "soc-msm8916"), test))]
mod fdt;

#[cfg(all(target_os = "none", feature = "soc-exynos7870"))]
mod exynos7870;

#[cfg(all(target_os = "none", feature = "soc-exynos7870"))]
use exynos7870 as soc;

#[cfg(any(
    all(target_os = "none", feature = "soc-msm8916"),
    all(test, feature = "soc-msm8916")
))]
mod msm8916;

#[cfg(all(target_os = "none", feature = "soc-msm8916"))]
use msm8916 as soc;

#[cfg(all(
    target_os = "none",
    not(any(feature = "soc-exynos7870", feature = "soc-msm8916"))
))]
compile_error!("pocketpreboot needs a supported soc-* Cargo feature");

#[cfg(all(
    target_os = "none",
    feature = "soc-exynos7870",
    feature = "soc-msm8916"
))]
compile_error!("pocketpreboot supports only one soc-* Cargo feature at a time");

#[cfg(target_os = "none")]
const ARM64_IMAGE_SIZE_OFFSET: usize = 16;
#[cfg(target_os = "none")]
const ARM64_IMAGE_MAGIC_OFFSET: usize = 56;
#[cfg(target_os = "none")]
const ARM64_IMAGE_MAGIC: u32 = u32::from_le_bytes(*b"ARM\x64");
#[cfg(target_os = "none")]
const PAYLOAD_ALIGN: usize = 0x200000;

#[cfg(target_os = "none")]
unsafe extern "C" {
    fn _start() -> !;
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
pub extern "C" fn pocketpreboot_main(fdt: usize) -> ! {
    #[cfg(feature = "debug-ram-trace")]
    ram_trace::stage(3);
    soc::early_init(fdt);
    soc::uart::writeln("pocketpreboot");

    let payload = payload_entry();
    if read32(payload + ARM64_IMAGE_MAGIC_OFFSET) != ARM64_IMAGE_MAGIC {
        soc::uart::writeln("\r\npocketpreboot: bad payload\r\n");
        for (label, value) in [
            ("preboot base", _start as *const () as usize as u64),
            ("payload address", payload as u64),
            (
                "payload magic",
                read32(payload + ARM64_IMAGE_MAGIC_OFFSET) as u64,
            ),
        ] {
            log_hex(label, value);
        }
        halt();
    }
    let payload_size = read64(payload + ARM64_IMAGE_SIZE_OFFSET) as usize;
    let Some(fdt) = soc::prepare_fdt(fdt, payload, payload_size) else {
        soc::uart::writeln("pocketpreboot: preboot setup failed");
        halt();
    };

    #[cfg(feature = "debug-ram-trace")]
    ram_trace::stage(0x1000);
    jump_to_payload(payload, fdt)
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    soc::uart::writeln("\r\npocketpreboot: panic\r\n");
    halt()
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
pub extern "C" fn pocketpreboot_exception(esr: u64, elr: u64, far: u64, el: u64) -> ! {
    soc::uart::writeln("pocketpreboot: exception");
    for (label, value) in [("CurrentEL", el), ("ESR", esr), ("ELR", elr), ("FAR", far)] {
        log_hex(label, value);
    }
    halt()
}

#[cfg(target_os = "none")]
fn log_hex(label: &str, value: u64) {
    let mut hex = [b'0'; 18];
    hex[1] = b'x';
    for index in 0..16 {
        hex[index + 2] = b"0123456789abcdef"[((value >> ((15 - index) * 4)) & 15) as usize];
    }
    soc::uart::writeln(label);
    soc::uart::writeln(unsafe { core::str::from_utf8_unchecked(&hex) });
}

#[cfg(target_os = "none")]
fn halt() -> ! {
    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(target_os = "none")]
fn payload_entry() -> usize {
    let base = _start as *const () as usize;
    let image_size = read64(base + ARM64_IMAGE_SIZE_OFFSET) as usize;
    align_up(base + image_size, PAYLOAD_ALIGN)
}

#[cfg(target_os = "none")]
fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

#[cfg(target_os = "none")]
fn read32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}

#[cfg(target_os = "none")]
fn read64(address: usize) -> u64 {
    unsafe { (address as *const u64).read_volatile() }
}

#[cfg(target_os = "none")]
fn jump_to_payload(entry: usize, fdt: usize) -> ! {
    unsafe {
        core::arch::asm!(
            "dsb sy",
            "isb",
            "mov x1, xzr",
            "mov x2, xzr",
            "mov x3, xzr",
            "br x16",
            in("x0") fdt,
            in("x16") entry,
            options(noreturn)
        );
    }
}

#[cfg(not(target_os = "none"))]
fn main() {}
