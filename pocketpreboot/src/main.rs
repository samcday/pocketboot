#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(target_os = "none")]
use core::panic::PanicInfo;

#[cfg(target_os = "none")]
core::arch::global_asm!(include_str!("start.S"));

#[cfg(all(target_os = "none", feature = "soc-exynos7870"))]
mod exynos7870;

#[cfg(all(target_os = "none", feature = "soc-exynos7870"))]
use exynos7870 as soc;

#[cfg(all(target_os = "none", not(feature = "soc-exynos7870")))]
compile_error!("pocketpreboot needs a supported soc-* Cargo feature");

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
    #[cfg(feature = "device-exynos7870-j7xelte")]
    {
        let _ = soc::j7xelte::route_muic_to_uart();
    }
    soc::uart::writeln("hi mom\r\n");
    let payload = payload_entry();
    if read32(payload + ARM64_IMAGE_MAGIC_OFFSET) != ARM64_IMAGE_MAGIC {
        soc::uart::writeln("\r\npocketpreboot: bad payload\r\n");
        halt();
    }

    jump_to_payload(payload, fdt)
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    soc::uart::writeln("\r\npocketpreboot: panic\r\n");
    halt()
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
