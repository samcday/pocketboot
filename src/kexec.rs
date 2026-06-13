use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    os::fd::{AsRawFd, FromRawFd},
};

const LINUX_REBOOT_CMD_KEXEC: libc::c_int = 0x45584543;
#[cfg(target_arch = "aarch64")]
const PAGE_SIZE: u64 = 4096;

pub(crate) struct KexecImage {
    kernel: File,
    initrd: Option<File>,
    dtb: Option<File>,
    cmdline: String,
}

impl KexecImage {
    pub(crate) fn new(
        kernel: File,
        initrd: Option<File>,
        dtb: Option<File>,
        cmdline: &str,
    ) -> io::Result<Self> {
        if cmdline.as_bytes().contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cmdline contains NUL byte",
            ));
        }

        Ok(Self {
            kernel,
            initrd,
            dtb,
            cmdline: cmdline.to_string(),
        })
    }

    pub(crate) fn load(&self) -> io::Result<()> {
        let kernel = read_payload(&self.kernel)?;
        let initrd = self.initrd.as_ref().map(read_payload).transpose()?;
        let dtb = match &self.dtb {
            Some(dtb) => {
                tracing::info!("using DTB from staged boot image");
                read_payload(dtb)?
            }
            None => {
                tracing::info!("using current DTB from /sys/firmware/fdt");
                read_current_dtb()?
            }
        };

        load_arm64(&kernel, initrd.as_deref(), &dtb, &self.cmdline)
    }
}

pub(crate) fn create_payload_memfd(name: &str) -> io::Result<File> {
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "memfd name contains NUL byte"))?;
    let fd = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
}

pub(crate) fn reopen_payload_readonly(file: File) -> io::Result<File> {
    let path = format!("/proc/self/fd/{}", file.as_raw_fd());
    let readonly = File::open(path)?;
    drop(file);
    Ok(readonly)
}

pub(crate) fn exec_loaded_image() -> io::Result<()> {
    let rc = unsafe { libc::reboot(LINUX_REBOOT_CMD_KEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    Err(io::Error::other(
        "reboot(LINUX_REBOOT_CMD_KEXEC) returned unexpectedly",
    ))
}

fn read_payload(file: &File) -> io::Result<Vec<u8>> {
    let mut file = File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    file.seek(SeekFrom::Start(0))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(data)
}

fn read_current_dtb() -> io::Result<Vec<u8>> {
    fs::read("/sys/firmware/fdt").map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("no DTB supplied and /sys/firmware/fdt could not be read: {err}"),
        )
    })
}

#[cfg(not(target_arch = "aarch64"))]
fn load_arm64(
    _kernel: &[u8],
    _initrd: Option<&[u8]>,
    _dtb: &[u8],
    _cmdline: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "legacy kexec_load is currently implemented only for aarch64",
    ))
}

#[cfg(target_arch = "aarch64")]
fn load_arm64(kernel: &[u8], initrd: Option<&[u8]>, dtb: &[u8], cmdline: &str) -> io::Result<()> {
    arm64::load(kernel, initrd, dtb, cmdline)
}

#[cfg(target_arch = "aarch64")]
mod arm64 {
    use super::{PAGE_SIZE, fdt, page_align};
    use std::{io, ptr, slice};

    const SYS_KEXEC_LOAD: libc::c_long = 104;
    const KEXEC_ARCH_AARCH64: libc::c_ulong = 183 << 16;
    const KEXEC_SEGMENT_MAX: usize = 16;
    const SZ_2M: u64 = 2 * 1024 * 1024;
    const SZ_1G: u64 = 1024 * 1024 * 1024;
    const SZ_32G: u64 = 32 * SZ_1G;
    const SZ_16M: u64 = 16 * 1024 * 1024;
    const ARM64_IMAGE_MAGIC: u32 = 0x644d5241;
    const MAX_DTB_SIZE: usize = 2 * 1024 * 1024;

    core::arch::global_asm!(
        r#"
        .section .rodata.pocketboot_trampoline, "a"
        .balign 8

        .global __pb_tramp_start
__pb_tramp_start:
        ldr x17, .Lpb_tramp_kernel_entry
        ldr x0, .Lpb_tramp_dtb_addr
        mov x1, xzr
        mov x2, xzr
        mov x3, xzr
        br x17

        .balign 8
        .global __pb_tramp_kernel_entry
__pb_tramp_kernel_entry:
.Lpb_tramp_kernel_entry:
        .quad 0

        .global __pb_tramp_dtb_addr
__pb_tramp_dtb_addr:
.Lpb_tramp_dtb_addr:
        .quad 0

        .global __pb_tramp_end
__pb_tramp_end:
    "#
    );

    unsafe extern "C" {
        static __pb_tramp_start: u8;
        static __pb_tramp_kernel_entry: u8;
        static __pb_tramp_dtb_addr: u8;
        static __pb_tramp_end: u8;
    }

    #[repr(C)]
    struct KexecSegment {
        buf: *const libc::c_void,
        bufsz: libc::size_t,
        mem: libc::c_ulong,
        memsz: libc::size_t,
    }

    struct LoadedSegment {
        name: &'static str,
        data: Vec<u8>,
        phys: u64,
        memsz: u64,
    }

    #[derive(Clone, Copy, Debug)]
    struct PhysRange {
        start: u64,
        end: u64,
    }

    impl PhysRange {
        fn new(start: u64, end: u64) -> Option<Self> {
            (start < end).then_some(Self { start, end })
        }

        fn overlaps(self, other: Self) -> bool {
            self.start < other.end && other.start < self.end
        }
    }

    struct ImageHeader {
        text_offset: u64,
        image_size: u64,
    }

    pub(super) fn load(
        kernel: &[u8],
        initrd: Option<&[u8]>,
        dtb: &[u8],
        cmdline: &str,
    ) -> io::Result<()> {
        if kernel.is_empty() {
            return invalid_input("kernel payload is empty");
        }
        if dtb.is_empty() {
            return invalid_input("DTB payload is empty");
        }

        let header = ImageHeader::parse(kernel)?;
        if header.text_offset % PAGE_SIZE != 0 {
            return invalid_data(format!(
                "arm64 Image text_offset is not page-aligned: 0x{:x}",
                header.text_offset
            ));
        }

        let usable = read_usable_memory()?;
        let mut occupied = Vec::new();
        let mut segments = Vec::new();

        let kernel_memsz = page_align(header.image_size.max(kernel.len() as u64))?;
        let kernel_region_size = checked_add(header.text_offset, kernel_memsz)?;
        let kernel_base = find_region(&usable, &occupied, kernel_region_size, SZ_2M, 0, u64::MAX)
            .ok_or_else(|| io::Error::other("no suitable RAM hole for kernel"))?;
        let kernel_entry = checked_add(kernel_base, header.text_offset)?;
        occupied.push(PhysRange {
            start: kernel_base,
            end: checked_add(kernel_base, kernel_region_size)?,
        });
        segments.push(LoadedSegment {
            name: "kernel",
            data: kernel.to_vec(),
            phys: kernel_entry,
            memsz: kernel_memsz,
        });

        let initrd_range = if let Some(initrd) = initrd.filter(|initrd| !initrd.is_empty()) {
            let initrd_memsz = page_align(initrd.len() as u64)?;
            let min = checked_add(kernel_entry, kernel_memsz)?;
            let max = checked_add(align_down(kernel_entry, SZ_1G), SZ_32G)?;
            let initrd_phys = find_region(&usable, &occupied, initrd_memsz, PAGE_SIZE, min, max)
                .ok_or_else(|| io::Error::other("no suitable RAM hole for initrd"))?;
            let initrd_end = checked_add(initrd_phys, initrd.len() as u64)?;
            occupied.push(PhysRange {
                start: initrd_phys,
                end: checked_add(initrd_phys, initrd_memsz)?,
            });
            segments.push(LoadedSegment {
                name: "initrd",
                data: initrd.to_vec(),
                phys: initrd_phys,
                memsz: initrd_memsz,
            });
            Some((initrd_phys, initrd_end))
        } else {
            None
        };

        let patched_dtb = fdt::patch_chosen(dtb, cmdline, initrd_range)?;
        tracing::info!(
            cmdline = %cmdline,
            bytes = patched_dtb.len(),
            "patched kexec DTB /chosen node"
        );
        if patched_dtb.len() > MAX_DTB_SIZE {
            return invalid_data(format!(
                "patched DTB is too large: {} bytes > {} bytes",
                patched_dtb.len(),
                MAX_DTB_SIZE
            ));
        }

        let dtb_memsz = page_align(patched_dtb.len() as u64)?;
        let min = checked_add(kernel_entry, kernel_memsz)?;
        let dtb_phys = find_region(&usable, &occupied, dtb_memsz, SZ_2M, min, u64::MAX)
            .ok_or_else(|| io::Error::other("no suitable RAM hole for DTB"))?;
        occupied.push(PhysRange {
            start: dtb_phys,
            end: checked_add(dtb_phys, dtb_memsz)?,
        });
        segments.push(LoadedSegment {
            name: "dtb",
            data: patched_dtb,
            phys: dtb_phys,
            memsz: dtb_memsz,
        });

        let trampoline = build_trampoline(kernel_entry, dtb_phys)?;
        let trampoline_memsz = page_align(trampoline.len() as u64)?;
        let trampoline_phys = find_region(
            &usable,
            &occupied,
            trampoline_memsz,
            PAGE_SIZE,
            min,
            u64::MAX,
        )
        .ok_or_else(|| io::Error::other("no suitable RAM hole for trampoline"))?;
        segments.push(LoadedSegment {
            name: "trampoline",
            data: trampoline,
            phys: trampoline_phys,
            memsz: trampoline_memsz,
        });

        if segments.len() > KEXEC_SEGMENT_MAX {
            return invalid_input(format!(
                "too many kexec segments: {} > {}",
                segments.len(),
                KEXEC_SEGMENT_MAX
            ));
        }

        for segment in &segments {
            tracing::info!(
                segment = segment.name,
                phys = format_args!("0x{:x}", segment.phys),
                bufsz = segment.data.len(),
                memsz = segment.memsz,
                "prepared legacy kexec segment"
            );
        }

        let raw_segments = segments
            .iter()
            .map(|segment| KexecSegment {
                buf: segment.data.as_ptr().cast(),
                bufsz: segment.data.len(),
                mem: segment.phys as libc::c_ulong,
                memsz: segment.memsz as libc::size_t,
            })
            .collect::<Vec<_>>();

        kexec_load(trampoline_phys, &raw_segments)
    }

    impl ImageHeader {
        fn parse(kernel: &[u8]) -> io::Result<Self> {
            if kernel.len() < 64 {
                return invalid_data("arm64 Image is smaller than its 64-byte header");
            }

            let magic = u32::from_le_bytes(kernel[56..60].try_into().unwrap());
            if magic != ARM64_IMAGE_MAGIC {
                return invalid_data("kernel is not a raw arm64 Image");
            }

            let raw_text_offset = u64::from_le_bytes(kernel[8..16].try_into().unwrap());
            let raw_image_size = u64::from_le_bytes(kernel[16..24].try_into().unwrap());
            let (text_offset, image_size) = if raw_image_size == 0 {
                (0x80000, (kernel.len() as u64).max(SZ_16M))
            } else {
                (raw_text_offset, raw_image_size)
            };

            Ok(Self {
                text_offset,
                image_size,
            })
        }
    }

    fn kexec_load(entry: u64, segments: &[KexecSegment]) -> io::Result<()> {
        let rc = unsafe {
            libc::syscall(
                SYS_KEXEC_LOAD,
                entry as libc::c_ulong,
                segments.len() as libc::c_ulong,
                segments.as_ptr(),
                KEXEC_ARCH_AARCH64,
            )
        };

        if rc == 0 {
            Ok(())
        } else {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOSYS) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "kexec_load syscall is not available; enable CONFIG_KEXEC in the pocketboot kernel",
                ));
            }
            Err(err)
        }
    }

    fn read_usable_memory() -> io::Result<Vec<PhysRange>> {
        let iomem = std::fs::read_to_string("/proc/iomem")?;
        let ranges = parse_iomem(&iomem);
        if ranges.is_empty() {
            return invalid_data("no usable System RAM ranges found in /proc/iomem");
        }
        Ok(ranges)
    }

    fn parse_iomem(iomem: &str) -> Vec<PhysRange> {
        let mut ranges = Vec::new();

        for line in iomem.lines() {
            let Some((range, name)) = parse_iomem_line(line) else {
                continue;
            };

            match name {
                "System RAM" => add_range(&mut ranges, range),
                "Kernel code" | "Kernel data" | "Kernel bss" => {}
                _ => subtract_range(&mut ranges, range),
            }
        }

        ranges.sort_by_key(|range| range.start);
        ranges
    }

    fn parse_iomem_line(line: &str) -> Option<(PhysRange, &str)> {
        let (raw_range, raw_name) = line.trim_start().split_once(':')?;
        let (start, end) = raw_range.trim().split_once('-')?;
        let start = u64::from_str_radix(start, 16).ok()?;
        let end = u64::from_str_radix(end, 16).ok()?.checked_add(1)?;
        Some((PhysRange::new(start, end)?, raw_name.trim()))
    }

    fn add_range(ranges: &mut Vec<PhysRange>, range: PhysRange) {
        ranges.push(range);
        ranges.sort_by_key(|range| range.start);

        let mut merged: Vec<PhysRange> = Vec::new();
        for range in ranges.drain(..) {
            if let Some(last) = merged.last_mut() {
                if range.start <= last.end {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            merged.push(range);
        }
        *ranges = merged;
    }

    fn subtract_range(ranges: &mut Vec<PhysRange>, remove: PhysRange) {
        let mut updated = Vec::new();
        for range in ranges.drain(..) {
            if !range.overlaps(remove) {
                updated.push(range);
                continue;
            }
            if range.start < remove.start {
                if let Some(left) = PhysRange::new(range.start, remove.start.min(range.end)) {
                    updated.push(left);
                }
            }
            if remove.end < range.end {
                if let Some(right) = PhysRange::new(remove.end.max(range.start), range.end) {
                    updated.push(right);
                }
            }
        }
        *ranges = updated;
    }

    fn find_region(
        usable: &[PhysRange],
        occupied: &[PhysRange],
        size: u64,
        align: u64,
        min: u64,
        max: u64,
    ) -> Option<u64> {
        if size == 0 || !align.is_power_of_two() {
            return None;
        }

        for range in usable {
            let start = range.start.max(min);
            let end = range.end.min(max);
            if checked_add(start, size).ok()? > end {
                continue;
            }

            let mut candidate = align_up(start, align)?;
            while checked_add(candidate, size).ok()? <= end {
                let candidate_range = PhysRange {
                    start: candidate,
                    end: checked_add(candidate, size).ok()?,
                };
                if let Some(conflict) = occupied
                    .iter()
                    .copied()
                    .filter(|occupied| candidate_range.overlaps(*occupied))
                    .min_by_key(|occupied| occupied.end)
                {
                    candidate = align_up(conflict.end, align)?;
                } else {
                    return Some(candidate);
                }
            }
        }

        None
    }

    fn build_trampoline(kernel_entry: u64, dtb_addr: u64) -> io::Result<Vec<u8>> {
        let start = ptr::addr_of!(__pb_tramp_start) as usize;
        let end = ptr::addr_of!(__pb_tramp_end) as usize;
        let kernel_slot = ptr::addr_of!(__pb_tramp_kernel_entry) as usize;
        let dtb_slot = ptr::addr_of!(__pb_tramp_dtb_addr) as usize;

        if !(start <= kernel_slot
            && kernel_slot + 8 <= end
            && start <= dtb_slot
            && dtb_slot + 8 <= end)
        {
            return invalid_data("invalid arm64 trampoline symbol layout");
        }

        let mut trampoline =
            unsafe { slice::from_raw_parts(start as *const u8, end - start) }.to_vec();
        let kernel_offset = kernel_slot - start;
        let dtb_offset = dtb_slot - start;
        trampoline[kernel_offset..kernel_offset + 8].copy_from_slice(&kernel_entry.to_le_bytes());
        trampoline[dtb_offset..dtb_offset + 8].copy_from_slice(&dtb_addr.to_le_bytes());
        Ok(trampoline)
    }

    fn align_up(value: u64, align: u64) -> Option<u64> {
        value
            .checked_add(align - 1)
            .map(|value| value & !(align - 1))
    }

    fn align_down(value: u64, align: u64) -> u64 {
        value & !(align - 1)
    }

    fn checked_add(left: u64, right: u64) -> io::Result<u64> {
        left.checked_add(right)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "physical address overflow"))
    }

    fn invalid_input<T>(message: impl Into<String>) -> io::Result<T> {
        Err(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
    }

    fn invalid_data<T>(message: impl Into<String>) -> io::Result<T> {
        Err(io::Error::new(io::ErrorKind::InvalidData, message.into()))
    }
}

#[cfg(target_arch = "aarch64")]
fn page_align(value: u64) -> io::Result<u64> {
    value
        .checked_add(PAGE_SIZE - 1)
        .map(|value| value & !(PAGE_SIZE - 1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "size overflow"))
}

#[cfg(any(target_arch = "aarch64", test))]
mod fdt {
    use std::io;

    const FDT_MAGIC: u32 = 0xd00dfeed;
    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE: u32 = 2;
    const FDT_PROP: u32 = 3;
    const FDT_NOP: u32 = 4;
    const FDT_END: u32 = 9;

    const PROP_BOOTARGS: &[u8] = b"bootargs";
    const PROP_INITRD_START: &[u8] = b"linux,initrd-start";
    const PROP_INITRD_END: &[u8] = b"linux,initrd-end";
    const PROP_BOOTED_FROM_KEXEC: &[u8] = b"linux,booted-from-kexec";

    struct Header {
        totalsize: usize,
        off_dt_struct: usize,
        off_dt_strings: usize,
        off_mem_rsvmap: usize,
        version: u32,
        last_comp_version: u32,
        boot_cpuid_phys: u32,
        size_dt_strings: usize,
        size_dt_struct: usize,
    }

    struct ChosenOffsets {
        bootargs: u32,
        initrd_start: u32,
        initrd_end: u32,
        booted_from_kexec: u32,
    }

    pub(super) fn patch_chosen(
        dtb: &[u8],
        cmdline: &str,
        initrd: Option<(u64, u64)>,
    ) -> io::Result<Vec<u8>> {
        let header = Header::parse(dtb)?;
        let reserve_map = reserve_map(dtb, &header)?;
        let struct_block = checked_slice(
            dtb,
            header.off_dt_struct,
            header.size_dt_struct,
            "DTB structure block",
        )?;
        let mut strings = checked_slice(
            dtb,
            header.off_dt_strings,
            header.size_dt_strings,
            "DTB strings block",
        )?
        .to_vec();

        let offsets = ChosenOffsets {
            bootargs: ensure_string(&mut strings, PROP_BOOTARGS)?,
            initrd_start: ensure_string(&mut strings, PROP_INITRD_START)?,
            initrd_end: ensure_string(&mut strings, PROP_INITRD_END)?,
            booted_from_kexec: ensure_string(&mut strings, PROP_BOOTED_FROM_KEXEC)?,
        };

        let new_struct = patch_structure_block(struct_block, &strings, &offsets, cmdline, initrd)?;
        build_dtb(&header, &reserve_map, &new_struct, &strings)
    }

    impl Header {
        fn parse(dtb: &[u8]) -> io::Result<Self> {
            if dtb.len() < 40 {
                return invalid_data("DTB is smaller than its header");
            }
            let magic = read_be32(dtb, 0)?;
            if magic != FDT_MAGIC {
                return invalid_data("DTB has invalid magic");
            }

            let header = Self {
                totalsize: read_be32(dtb, 4)? as usize,
                off_dt_struct: read_be32(dtb, 8)? as usize,
                off_dt_strings: read_be32(dtb, 12)? as usize,
                off_mem_rsvmap: read_be32(dtb, 16)? as usize,
                version: read_be32(dtb, 20)?,
                last_comp_version: read_be32(dtb, 24)?,
                boot_cpuid_phys: read_be32(dtb, 28)?,
                size_dt_strings: read_be32(dtb, 32)? as usize,
                size_dt_struct: read_be32(dtb, 36)? as usize,
            };

            if header.totalsize > dtb.len() {
                return invalid_data("DTB totalsize exceeds payload size");
            }
            checked_slice(
                dtb,
                header.off_dt_struct,
                header.size_dt_struct,
                "DTB structure block",
            )?;
            checked_slice(
                dtb,
                header.off_dt_strings,
                header.size_dt_strings,
                "DTB strings block",
            )?;
            if header.off_mem_rsvmap >= header.totalsize {
                return invalid_data("DTB reserve map offset is out of range");
            }

            Ok(header)
        }
    }

    fn reserve_map(dtb: &[u8], header: &Header) -> io::Result<Vec<u8>> {
        let mut cursor = header.off_mem_rsvmap;
        let mut reserve_map = Vec::new();
        loop {
            if cursor
                .checked_add(16)
                .is_none_or(|end| end > header.totalsize)
            {
                return invalid_data("DTB reserve map is unterminated");
            }
            let entry = &dtb[cursor..cursor + 16];
            reserve_map.extend_from_slice(entry);
            let address = u64::from_be_bytes(entry[0..8].try_into().unwrap());
            let size = u64::from_be_bytes(entry[8..16].try_into().unwrap());
            cursor += 16;
            if address == 0 && size == 0 {
                return Ok(reserve_map);
            }
        }
    }

    fn patch_structure_block(
        struct_block: &[u8],
        strings: &[u8],
        offsets: &ChosenOffsets,
        cmdline: &str,
        initrd: Option<(u64, u64)>,
    ) -> io::Result<Vec<u8>> {
        let mut cursor = 0usize;
        let mut output = Vec::with_capacity(struct_block.len() + cmdline.len() + 128);
        let mut stack: Vec<Vec<u8>> = Vec::new();
        let mut saw_root = false;
        let mut saw_chosen = false;

        loop {
            let token_start = cursor;
            let token = read_be32(struct_block, cursor)?;
            cursor += 4;

            match token {
                FDT_BEGIN_NODE => {
                    let name_start = cursor;
                    let name_end = find_nul(struct_block, name_start)?;
                    let name = &struct_block[name_start..name_end];
                    cursor = align_usize(name_end + 1, 4)?;

                    write_be32(&mut output, FDT_BEGIN_NODE);
                    output.extend_from_slice(name);
                    output.push(0);
                    pad_to(&mut output, 4);

                    if stack.is_empty() {
                        if !name.is_empty() {
                            return invalid_data("DTB root node name is not empty");
                        }
                        saw_root = true;
                    }

                    stack.push(name.to_vec());
                    if is_chosen(&stack) {
                        saw_chosen = true;
                        append_chosen_props(&mut output, offsets, cmdline, initrd);
                    }
                }
                FDT_END_NODE => {
                    if is_root(&stack) && !saw_chosen {
                        write_begin_node(&mut output, b"chosen");
                        append_chosen_props(&mut output, offsets, cmdline, initrd);
                        write_be32(&mut output, FDT_END_NODE);
                        saw_chosen = true;
                    }

                    write_be32(&mut output, FDT_END_NODE);
                    if stack.pop().is_none() {
                        return invalid_data("DTB structure has too many END_NODE tokens");
                    }
                }
                FDT_PROP => {
                    let len = read_be32(struct_block, cursor)? as usize;
                    let nameoff = read_be32(struct_block, cursor + 4)?;
                    let value_start = cursor + 8;
                    let value_end = value_start
                        .checked_add(len)
                        .ok_or_else(|| invalid_data_error("DTB property length overflow"))?;
                    let next = align_usize(value_end, 4)?;
                    if next > struct_block.len() {
                        return invalid_data("DTB property extends past structure block");
                    }

                    let prop_name = string_at(strings, nameoff).unwrap_or_default();
                    if !(is_chosen(&stack) && should_replace_chosen_prop(prop_name)) {
                        output.extend_from_slice(&struct_block[token_start..next]);
                    }
                    cursor = next;
                }
                FDT_NOP => output.extend_from_slice(&struct_block[token_start..cursor]),
                FDT_END => {
                    write_be32(&mut output, FDT_END);
                    if !saw_root {
                        return invalid_data("DTB structure has no root node");
                    }
                    if !stack.is_empty() {
                        return invalid_data("DTB structure ended before all nodes were closed");
                    }
                    return Ok(output);
                }
                _ => return invalid_data(format!("DTB has unknown structure token {token}")),
            }
        }
    }

    fn append_chosen_props(
        output: &mut Vec<u8>,
        offsets: &ChosenOffsets,
        cmdline: &str,
        initrd: Option<(u64, u64)>,
    ) {
        if !cmdline.is_empty() {
            let mut value = Vec::with_capacity(cmdline.len() + 1);
            value.extend_from_slice(cmdline.as_bytes());
            value.push(0);
            write_prop(output, offsets.bootargs, &value);
        }

        if let Some((start, end)) = initrd {
            write_prop(output, offsets.initrd_start, &start.to_be_bytes());
            write_prop(output, offsets.initrd_end, &end.to_be_bytes());
        }

        write_prop(output, offsets.booted_from_kexec, &[]);
    }

    fn build_dtb(
        old_header: &Header,
        reserve_map: &[u8],
        struct_block: &[u8],
        strings: &[u8],
    ) -> io::Result<Vec<u8>> {
        let mut output = vec![0; 40];
        output.extend_from_slice(reserve_map);
        pad_to(&mut output, 4);
        let off_dt_struct = output.len();
        output.extend_from_slice(struct_block);
        let size_dt_struct = struct_block.len();
        let off_dt_strings = output.len();
        output.extend_from_slice(strings);
        let totalsize = output.len();

        write_be32_at(&mut output, 0, FDT_MAGIC)?;
        write_be32_at(&mut output, 4, totalsize_u32(totalsize, "DTB totalsize")?)?;
        write_be32_at(
            &mut output,
            8,
            totalsize_u32(off_dt_struct, "DTB structure offset")?,
        )?;
        write_be32_at(
            &mut output,
            12,
            totalsize_u32(off_dt_strings, "DTB strings offset")?,
        )?;
        write_be32_at(&mut output, 16, 40)?;
        write_be32_at(&mut output, 20, old_header.version)?;
        write_be32_at(&mut output, 24, old_header.last_comp_version)?;
        write_be32_at(&mut output, 28, old_header.boot_cpuid_phys)?;
        write_be32_at(
            &mut output,
            32,
            totalsize_u32(strings.len(), "DTB strings size")?,
        )?;
        write_be32_at(
            &mut output,
            36,
            totalsize_u32(size_dt_struct, "DTB structure size")?,
        )?;

        Ok(output)
    }

    fn should_replace_chosen_prop(name: &[u8]) -> bool {
        matches!(
            name,
            PROP_BOOTARGS | PROP_INITRD_START | PROP_INITRD_END | PROP_BOOTED_FROM_KEXEC
        )
    }

    fn is_root(stack: &[Vec<u8>]) -> bool {
        stack.len() == 1 && stack[0].is_empty()
    }

    fn is_chosen(stack: &[Vec<u8>]) -> bool {
        stack.len() == 2 && stack[0].is_empty() && stack[1] == b"chosen"
    }

    fn write_begin_node(output: &mut Vec<u8>, name: &[u8]) {
        write_be32(output, FDT_BEGIN_NODE);
        output.extend_from_slice(name);
        output.push(0);
        pad_to(output, 4);
    }

    fn write_prop(output: &mut Vec<u8>, nameoff: u32, value: &[u8]) {
        write_be32(output, FDT_PROP);
        write_be32(output, value.len() as u32);
        write_be32(output, nameoff);
        output.extend_from_slice(value);
        pad_to(output, 4);
    }

    fn ensure_string(strings: &mut Vec<u8>, name: &[u8]) -> io::Result<u32> {
        let mut cursor = 0usize;
        while cursor < strings.len() {
            let Some(relative_end) = strings[cursor..].iter().position(|byte| *byte == 0) else {
                break;
            };
            let end = cursor + relative_end;
            if &strings[cursor..end] == name {
                return totalsize_u32(cursor, "DTB string offset");
            }
            cursor = end + 1;
        }

        let offset = strings.len();
        strings.extend_from_slice(name);
        strings.push(0);
        totalsize_u32(offset, "DTB string offset")
    }

    fn string_at(strings: &[u8], offset: u32) -> Option<&[u8]> {
        let start = offset as usize;
        if start >= strings.len() {
            return None;
        }
        let end = strings[start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|end| start + end)?;
        Some(&strings[start..end])
    }

    fn checked_slice<'a>(
        data: &'a [u8],
        offset: usize,
        size: usize,
        name: &str,
    ) -> io::Result<&'a [u8]> {
        let end = offset
            .checked_add(size)
            .ok_or_else(|| invalid_data_error(format!("{name} range overflow")))?;
        if end > data.len() {
            return invalid_data(format!("{name} extends past DTB payload"));
        }
        Ok(&data[offset..end])
    }

    fn find_nul(data: &[u8], start: usize) -> io::Result<usize> {
        data[start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|pos| start + pos)
            .ok_or_else(|| invalid_data_error("unterminated DTB node name"))
    }

    fn align_usize(value: usize, align: usize) -> io::Result<usize> {
        value
            .checked_add(align - 1)
            .map(|value| value & !(align - 1))
            .ok_or_else(|| invalid_data_error("DTB alignment overflow"))
    }

    fn pad_to(output: &mut Vec<u8>, align: usize) {
        let len = output.len();
        let padded = (len + align - 1) & !(align - 1);
        output.resize(padded, 0);
    }

    fn read_be32(data: &[u8], offset: usize) -> io::Result<u32> {
        let end = offset
            .checked_add(4)
            .ok_or_else(|| invalid_data_error("u32 offset overflow"))?;
        if end > data.len() {
            return invalid_data("unexpected end of data while reading u32");
        }
        Ok(u32::from_be_bytes(data[offset..end].try_into().unwrap()))
    }

    fn write_be32(output: &mut Vec<u8>, value: u32) {
        output.extend_from_slice(&value.to_be_bytes());
    }

    fn write_be32_at(output: &mut [u8], offset: usize, value: u32) -> io::Result<()> {
        let end = offset
            .checked_add(4)
            .ok_or_else(|| invalid_data_error("u32 write offset overflow"))?;
        if end > output.len() {
            return invalid_data("unexpected end of data while writing u32");
        }
        output[offset..end].copy_from_slice(&value.to_be_bytes());
        Ok(())
    }

    fn totalsize_u32(value: usize, name: &str) -> io::Result<u32> {
        u32::try_from(value).map_err(|_| invalid_data_error(format!("{name} does not fit u32")))
    }

    fn invalid_data<T>(message: impl Into<String>) -> io::Result<T> {
        Err(invalid_data_error(message))
    }

    fn invalid_data_error(message: impl Into<String>) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message.into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[derive(Debug, PartialEq, Eq)]
        enum ChosenEvent {
            Prop(String, Vec<u8>),
            Child(String),
        }

        #[test]
        fn patched_chosen_properties_are_before_child_nodes() {
            let dtb = test_dtb_with_chosen_child();
            let patched = patch_chosen(&dtb, "console=tty0 root=/dev/ram0", None).unwrap();
            let events = chosen_events(&patched).unwrap();

            let bootargs = events
                .iter()
                .position(|event| matches!(event, ChosenEvent::Prop(name, _) if name == "bootargs"))
                .expect("patched bootargs property missing");
            let child = events
                .iter()
                .position(
                    |event| matches!(event, ChosenEvent::Child(name) if name == "framebuffer@0"),
                )
                .expect("chosen child node missing");

            assert!(bootargs < child, "bootargs must precede /chosen children");
            assert_eq!(
                events[bootargs],
                ChosenEvent::Prop(
                    "bootargs".to_string(),
                    b"console=tty0 root=/dev/ram0\0".to_vec()
                )
            );
            assert_eq!(
                events
                    .iter()
                    .filter(
                        |event| matches!(event, ChosenEvent::Prop(name, _) if name == "bootargs")
                    )
                    .count(),
                1,
                "old bootargs property should be replaced, not duplicated"
            );
        }

        fn test_dtb_with_chosen_child() -> Vec<u8> {
            let mut strings = Vec::new();
            let bootargs = ensure_string(&mut strings, PROP_BOOTARGS).unwrap();
            let compatible = ensure_string(&mut strings, b"compatible").unwrap();
            let mut structure = Vec::new();

            write_begin_node(&mut structure, b"");
            write_begin_node(&mut structure, b"chosen");
            write_prop(&mut structure, bootargs, b"old-console=ttyS0\0");
            write_begin_node(&mut structure, b"framebuffer@0");
            write_prop(&mut structure, compatible, b"simple-framebuffer\0");
            write_be32(&mut structure, FDT_END_NODE);
            write_be32(&mut structure, FDT_END_NODE);
            write_be32(&mut structure, FDT_END_NODE);
            write_be32(&mut structure, FDT_END);

            build_dtb(
                &Header {
                    totalsize: 0,
                    off_dt_struct: 0,
                    off_dt_strings: 0,
                    off_mem_rsvmap: 0,
                    version: 17,
                    last_comp_version: 16,
                    boot_cpuid_phys: 0,
                    size_dt_strings: 0,
                    size_dt_struct: 0,
                },
                &[0; 16],
                &structure,
                &strings,
            )
            .unwrap()
        }

        fn chosen_events(dtb: &[u8]) -> io::Result<Vec<ChosenEvent>> {
            let header = Header::parse(dtb)?;
            let struct_block = checked_slice(
                dtb,
                header.off_dt_struct,
                header.size_dt_struct,
                "DTB structure block",
            )?;
            let strings = checked_slice(
                dtb,
                header.off_dt_strings,
                header.size_dt_strings,
                "DTB strings block",
            )?;
            let mut cursor = 0usize;
            let mut stack: Vec<Vec<u8>> = Vec::new();
            let mut events = Vec::new();

            loop {
                let token = read_be32(struct_block, cursor)?;
                cursor += 4;

                match token {
                    FDT_BEGIN_NODE => {
                        let name_start = cursor;
                        let name_end = find_nul(struct_block, name_start)?;
                        let name = &struct_block[name_start..name_end];
                        cursor = align_usize(name_end + 1, 4)?;
                        if is_chosen(&stack) {
                            events.push(ChosenEvent::Child(
                                String::from_utf8_lossy(name).into_owned(),
                            ));
                        }
                        stack.push(name.to_vec());
                    }
                    FDT_END_NODE => {
                        stack.pop();
                    }
                    FDT_PROP => {
                        let len = read_be32(struct_block, cursor)? as usize;
                        let nameoff = read_be32(struct_block, cursor + 4)?;
                        let value_start = cursor + 8;
                        let value_end = value_start + len;
                        cursor = align_usize(value_end, 4)?;
                        if is_chosen(&stack) {
                            let name = string_at(strings, nameoff)
                                .map(String::from_utf8_lossy)
                                .map(|name| name.into_owned())
                                .unwrap_or_else(|| format!("<invalid:{nameoff}>"));
                            events.push(ChosenEvent::Prop(
                                name,
                                struct_block[value_start..value_end].to_vec(),
                            ));
                        }
                    }
                    FDT_NOP => {}
                    FDT_END => return Ok(events),
                    _ => return invalid_data(format!("unknown test DTB token: {token}")),
                }
            }
        }
    }
}
