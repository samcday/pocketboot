#![no_std]
#![no_main]
core::arch::global_asm!(include_str!("../../src/start.S"));
core::arch::global_asm!(include_str!("../../src/exceptions.S"));
static MESSAGES: [&str; 2] = ["PASS relocated Rust pointer table and FDT argument\n", "FAIL unexpected index\n"];
#[unsafe(no_mangle)]
pub extern "C" fn pocketpreboot_main(fdt: usize) -> ! {
    let value = unsafe { (fdt as *const u32).read_volatile() };
    if value != u32::from_le_bytes([0xd0, 0x0d, 0xfe, 0xed]) { exit(1) }
    let index = (value & 1) as usize;
    for byte in MESSAGES[index].bytes() {
        unsafe { (0x09000000 as *mut u32).write_volatile(byte as u32) };
    }
    exit(0)
}
#[unsafe(no_mangle)]
pub extern "C" fn pocketpreboot_exception(_: u64, _: u64, _: u64, _: u64) -> ! { exit(2) }
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { exit(3) }
fn exit(status: u64) -> ! {
    let args = [0x20026u64, status];
    unsafe { core::arch::asm!("hlt #0xf000", in("x0") 0x20u64, in("x1") args.as_ptr(), options(noreturn)); }
}
