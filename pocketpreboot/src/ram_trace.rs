//! Optional primary-CPU lab log in the shim's own reserved image footprint.
//!
//! Header: magic[8], stage u64, text length u64, FDT u64, CurrentEL u64,
//! SCTLR u64, ESR u64, ELR u64, FAR u64, FNV-1a text checksum u64. Text starts
//! at byte 128. Three 4096-byte copies permit recovery from partial RAM loss
//! across reset. All integers are little-endian. The host must validate the
//! header, bounds and checksum before interpreting recovered text.

unsafe extern "C" {
    static mut pocketpreboot_ram_trace: u8;
}

fn base() -> *mut u8 {
    core::ptr::addr_of_mut!(pocketpreboot_ram_trace)
}

pub fn stage(value: u64) {
    unsafe {
        for copy in 0..3 {
            base()
                .add(copy * 4096 + 8)
                .cast::<u64>()
                .write_volatile(value.to_le());
        }
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

pub fn write_byte(byte: u8) {
    unsafe {
        let length = base().add(16).cast::<u64>();
        let used = u64::from_le(length.read_volatile()) as usize;
        if used < 4096 - 128 {
            let checksum = u64::from_le(base().add(72).cast::<u64>().read_volatile());
            let checksum = (checksum ^ byte as u64).wrapping_mul(0x100000001b3);
            for copy in 0..3 {
                let page = base().add(copy * 4096);
                page.add(128 + used).write_volatile(byte);
                page.add(72).cast::<u64>().write_volatile(checksum.to_le());
                page.add(16)
                    .cast::<u64>()
                    .write_volatile(((used + 1) as u64).to_le());
            }
            core::arch::asm!("dsb sy", options(nostack, preserves_flags));
        }
    }
}
