//! ARMv7 DT boot using Linux's own relocation/register handoff.
//!
//! Unlike arm64, arch/arm/kernel/relocate_kernel.S already sets r0/r1/r2.
//! The syscall entry is the zImage itself; a segment starting with the FDT
//! magic makes machine_kexec_prepare() select that segment as r2.

use std::{borrow::Cow, io};

use super::{PAGE_SIZE, fdt, is_raw_arm_zimage, memory::*, page_align};

const EXTENSION_MAGIC: u32 = 0x45454545;
const KERNEL_SIZE_TAG: u32 = 0x5a534c4b;
const TEXT_OFFSET: u64 = 0x8000;
const BOOT_WINDOW: u64 = 128 * 1024 * 1024;
const MAX_DTB_SIZE: usize = 2 * 1024 * 1024;
// Non-LPAE ARM initially maps only two 1 MiB sections around r2.
const DTB_ALIGNMENT: u64 = 1024 * 1024;
// Supported decompressor contract, not a field in the zImage header.
// The tested Linux gzip decompressors have 24 bytes of BSS. See the scope
// and symbol-validation requirement in docs/arm32-kexec.md.
const DECOMPRESSOR_BSS_LIMIT: u64 = 64 * 1024;

#[derive(Debug)]
struct ZImage {
    len: usize,
    run_size: u64,
}

impl ZImage {
    fn parse(kernel: &[u8]) -> io::Result<Self> {
        if !is_raw_arm_zimage(kernel) {
            return invalid("ARM32 handoff requires a zImage, not a raw Image or ARM64 image");
        }
        let start = le32(kernel, 0x28)?;
        let end = le32(kernel, 0x2c)?;
        // Only the normal RAM-loaded, position-independent decompressor.
        if start != 0 || end < 60 || end as usize > kernel.len() {
            return invalid("invalid or unsupported ARM zImage start/end");
        }
        let image = &kernel[..end as usize];
        if le32(image, 0x30)? != 0x04030201 {
            return invalid("ARM32 handoff supports only little-endian zImages");
        }
        if le32(image, 0x34)? != EXTENSION_MAGIC {
            return invalid("zImage lacks size extensions; refusing to guess decompression space");
        }
        let mut cursor = le32(image, 0x38)? as usize;
        if cursor < 60 || cursor % 4 != 0 {
            return invalid("invalid zImage extension table offset");
        }
        let mut run_size = None;
        loop {
            let words = le32(image, cursor)? as usize;
            if words == 0 {
                break;
            }
            let bytes = words
                .checked_mul(4)
                .ok_or_else(|| bad("zImage tag overflow"))?;
            let next = cursor
                .checked_add(bytes)
                .ok_or_else(|| bad("zImage tag overflow"))?;
            if words < 2 || next > image.len() {
                return invalid("truncated or invalid zImage extension tag");
            }
            if le32(image, cursor + 4)? == KERNEL_SIZE_TAG {
                if words < 6 || run_size.is_some() {
                    return invalid("invalid or duplicate zImage kernel-size tag");
                }
                let size_ptr = le32(image, cursor + 8)? as usize;
                let inflated = le32(image, size_ptr)? as u64;
                let bss = le32(image, cursor + 12)? as u64;
                let text_offset = le32(image, cursor + 16)? as u64;
                let malloc_size = le32(image, cursor + 20)? as u64;
                if inflated == 0 || text_offset != TEXT_OFFSET || malloc_size != 0x10000 {
                    return invalid("unsupported zImage size/text-offset/decompressor workspace");
                }
                // Protect both destination BSS and the relocated decompressor.
                // The relocation-code headroom fits within the file itself:
                // budget another full file length instead of decoding head.S.
                // Also cover decompressor BSS (not the tag's kernel BSS),
                // a 4 KiB stack, 64 KiB heap, and alignment rounding.
                let relocated = inflated
                    + 2 * end as u64
                    + DECOMPRESSOR_BSS_LIMIT
                    + malloc_size
                    + 2 * PAGE_SIZE;
                run_size = Some((inflated + bss).max(relocated));
            }
            cursor = next;
        }
        Ok(Self {
            len: end as usize,
            run_size: run_size.ok_or_else(|| bad("zImage has no kernel-size tag"))?,
        })
    }
}

pub(super) fn appended_dtb(kernel: &[u8]) -> io::Result<Option<&[u8]>> {
    let image = ZImage::parse(kernel)?;
    let tail = &kernel[image.len..];
    if !tail.starts_with(&0xd00dfeedu32.to_be_bytes()) {
        return Ok(None);
    }
    let size = fdt::blob_size(tail)?;
    if tail[size..].iter().any(|byte| *byte != 0) {
        return invalid("multiple appended DTBs or trailing data; supply an explicit DTB");
    }
    Ok(Some(&tail[..size]))
}

struct Segment<'a> {
    name: &'static str,
    data: Cow<'a, [u8]>,
    range: PhysRange,
}

struct Plan<'a> {
    entry: u64,
    segments: Vec<Segment<'a>>,
}

impl<'a> Plan<'a> {
    fn build(
        kernel: &'a [u8],
        initrd: Option<&'a [u8]>,
        dtb: &[u8],
        cmdline: &str,
        iomem: &str,
    ) -> io::Result<Self> {
        let image = ZImage::parse(kernel)?;
        let base = iomem
            .lines()
            .filter_map(parse_iomem_line)
            .filter(|(_, name)| *name == "System RAM")
            .map(|(range, _)| range.start)
            .min()
            .ok_or_else(|| bad("no System RAM in /proc/iomem"))?;
        if base % (2 * 1024 * 1024) != 0
            || base > u32::MAX as u64
            || fdt::arm_memory_base(dtb)? != base
        {
            return invalid("ARM32 needs an agreed, 2 MiB-aligned DTB/System RAM base");
        }
        // AUTO_ZRELADDR starts with pc & 0xf8000000. An unaligned bank
        // requires the v5.12+ DT-aware decompressor (or matching fixed
        // ZRELADDR); the size tag alone cannot prove that build contract.
        // Never move into another 128 MiB window to dodge a reservation.
        let limit = checked_add(align_down(base, BOOT_WINDOW), BOOT_WINDOW)?.min(u32::MAX as u64);
        let entry = checked_add(base, TEXT_OFFSET)?;
        let run_end = page_align(checked_add(entry, image.run_size)?)?;
        let mut usable = parse_iomem(iomem);
        for (start, end) in fdt::reserved_ranges(dtb)? {
            subtract_range(&mut usable, PhysRange { start, end });
        }
        if run_end > limit
            || !usable
                .iter()
                .any(|ram| ram.start <= base && ram.end >= run_end)
        {
            return invalid(
                "ARM zImage decompression area is reserved or outside the first 128 MiB",
            );
        }
        // min=run_end keeps later payloads out of the entire workspace.
        let mut occupied = Vec::new();
        // Strip any appended DTB. Otherwise ARM_APPENDED_DTB can override the
        // patched DTB passed in r2. A four-byte tail alone is insufficient:
        // head.S can probe _edata again after relocating itself. Make kexec
        // zero-fill the entire supported workspace beyond the image, so both
        // probes see zeros even where the previous kernel left a DTB.
        let mut segments = vec![Segment {
            name: "zImage",
            range: PhysRange {
                start: entry,
                end: run_end,
            },
            data: Cow::Borrowed(&kernel[..image.len]),
        }];
        // Keep this first implementation entirely in the boot window.
        // That is deliberately stricter than Linux's general boot protocol:
        // /proc/iomem alone cannot tell us the destination kernel's lowmem end.
        let mut place = |size: usize, alignment: u64| -> io::Result<PhysRange> {
            let size = page_align(size as u64)?;
            let start = find_region(&usable, &occupied, size, alignment, run_end, limit)
                .ok_or_else(|| bad("no ARM boot-window RAM for initrd/DTB"))?;
            let range = PhysRange {
                start,
                end: checked_add(start, size)?,
            };
            occupied.push(range);
            Ok(range)
        };
        let initrd_range = match initrd.filter(|data| !data.is_empty()) {
            Some(data) => {
                let range = place(data.len(), PAGE_SIZE)?;
                let bytes_end = checked_add(range.start, data.len() as u64)?;
                segments.push(Segment {
                    name: "initrd",
                    data: Cow::Borrowed(data),
                    range,
                });
                Some((range.start, bytes_end))
            }
            None => None,
        };
        let data = fdt::patch_chosen(dtb, cmdline, initrd_range)?;
        if data.len() > MAX_DTB_SIZE {
            return invalid("ARM destination DTB exceeds 2 MiB");
        }
        let range = place(data.len(), DTB_ALIGNMENT)?;
        segments.push(Segment {
            name: "dtb",
            data: Cow::Owned(data),
            range,
        });
        Ok(Self { entry, segments })
    }
}

#[cfg(target_arch = "arm")]
pub(super) fn load(
    kernel: &[u8],
    initrd: Option<&[u8]>,
    dtb: &[u8],
    cmdline: &str,
) -> io::Result<()> {
    let plan = Plan::build(
        kernel,
        initrd,
        dtb,
        cmdline,
        &std::fs::read_to_string("/proc/iomem")?,
    )?;
    #[repr(C)]
    struct KexecSegment {
        buf: *const libc::c_void,
        bufsz: libc::size_t,
        mem: libc::c_ulong,
        memsz: libc::size_t,
    }
    let segments = plan
        .segments
        .iter()
        .map(|segment| {
            tracing::info!(
                segment = segment.name,
                phys = format_args!("0x{:x}", segment.range.start),
                bufsz = segment.data.len(),
                memsz = segment.range.end - segment.range.start,
                "prepared ARM kexec segment"
            );
            Ok(KexecSegment {
                buf: segment.data.as_ptr().cast(),
                bufsz: segment.data.len(),
                mem: segment
                    .range
                    .start
                    .try_into()
                    .map_err(|_| bad("ARM address overflow"))?,
                memsz: (segment.range.end - segment.range.start)
                    .try_into()
                    .map_err(|_| bad("ARM segment size overflow"))?,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let entry = u32::try_from(plan.entry).map_err(|_| bad("ARM entry overflow"))?;
    // Linux ARM identifies the standalone FDT segment and sets r2 itself.
    // No purgatory/trampoline segment is needed.
    let result = unsafe {
        libc::syscall(
            libc::SYS_kexec_load,
            entry as libc::c_ulong,
            segments.len() as libc::c_ulong,
            segments.as_ptr(),
            (40 << 16) as libc::c_ulong, // KEXEC_ARCH_ARM, normal (not crash) load
        )
    };
    if result == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOSYS) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ARM kexec_load is unavailable; enable CONFIG_KEXEC in the loader kernel",
        )),
        Some(libc::EINVAL) => Err(io::Error::new(
            err.kind(),
            format!(
                "ARM kexec_load: {err}; check RAM placement and platform CPU-hotplug support (Qualcomm loader kernels need SMP=n)"
            ),
        )),
        _ => Err(err),
    }
}

fn le32(data: &[u8], offset: usize) -> io::Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| bad("zImage offset overflow"))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| bad("truncated zImage metadata"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn bad(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid<T>(message: &str) -> io::Result<T> {
    Err(bad(message))
}

#[cfg(test)]
mod tests {
    use super::super::ARM_ZIMAGE_MAGIC as ZIMAGE_MAGIC;
    use super::*;

    fn image() -> Vec<u8> {
        let mut data = vec![0; 4096];
        for (offset, value) in [
            (0x24, ZIMAGE_MAGIC),
            (0x28, 0),
            (0x2c, 4096),
            (0x30, 0x04030201),
            (0x34, EXTENSION_MAGIC),
            (0x38, 64),
            (64, 6),
            (68, KERNEL_SIZE_TAG),
            (72, 128),
            (76, 0x20000),
            (80, TEXT_OFFSET as u32),
            (84, 0x10000),
            (88, 0),
            (128, 0x400000),
        ] {
            data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        data
    }

    fn dtb(reserved: &[(u64, u64)]) -> Vec<u8> {
        // 1-cell ARM32 RAM description matching the fixture's /proc/iomem.
        let mut structure = Vec::new();
        for word in [1u32, 0, 3, 4, 0, 1, 3, 4, 15, 1, 1] {
            structure.extend_from_slice(&word.to_be_bytes());
        }
        structure.extend_from_slice(b"memory\0\0");
        for word in [3u32, 7, 27] {
            structure.extend_from_slice(&word.to_be_bytes());
        }
        structure.extend_from_slice(b"memory\0\0");
        for word in [3u32, 8, 39, 0x80200000, 0x3fe00000, 2, 2, 9] {
            structure.extend_from_slice(&word.to_be_bytes());
        }
        let strings = b"#address-cells\0#size-cells\0device_type\0reg\0";
        let mut reserve = Vec::new();
        for &(start, size) in reserved {
            reserve.extend_from_slice(&start.to_be_bytes());
            reserve.extend_from_slice(&size.to_be_bytes());
        }
        reserve.extend_from_slice(&[0; 16]);
        let offset = 40 + reserve.len();
        let mut data = Vec::new();
        for word in [
            0xd00dfeed,
            (offset + structure.len() + strings.len()) as u32,
            offset as u32,
            (offset + structure.len()) as u32,
            40,
            17,
            16,
            0,
            strings.len() as u32,
            structure.len() as u32,
        ] {
            data.extend_from_slice(&word.to_be_bytes());
        }
        data.extend(reserve);
        data.extend(structure);
        data.extend(strings);
        data
    }

    const RAM: &str = "80200000-bfffffff : System RAM\n  80208000-80ffffff : Kernel code\n";

    #[test]
    fn validates_extensions_and_protects_bss_or_decompressor_workspace() {
        let mut kernel = image();
        let parsed = ZImage::parse(&kernel).unwrap();
        assert_eq!(parsed.len, 4096);
        assert_eq!(parsed.run_size, 0x424000);
        kernel[76..80].copy_from_slice(&0x100000u32.to_le_bytes());
        assert_eq!(ZImage::parse(&kernel).unwrap().run_size, 0x500000);
    }

    #[test]
    fn envelope_covers_recorded_samsung_relocation_and_heap_extents() {
        // Actual pinned gzip builds. The old inflated+file+0x11000 formula,
        // even page-rounded, fell short of these ends by 0x4a8 and 0x638.
        for (len, inflated, bss, heap_end) in [
            (0x424a70usize, 0x7aa250u32, 0x30da8u32, 0x80de84a8u64),
            (0x46d000, 0x851e5c, 0x2f11c, 0x80ed8638),
        ] {
            let mut kernel = image();
            kernel.resize(len, 0);
            kernel[0x2c..0x30].copy_from_slice(&(len as u32).to_le_bytes());
            kernel[76..80].copy_from_slice(&bss.to_le_bytes());
            kernel[128..132].copy_from_slice(&inflated.to_le_bytes());
            let size = ZImage::parse(&kernel).unwrap().run_size;
            assert!(0x80208000 + size >= heap_end);
        }
    }

    #[test]
    fn rejects_raw_truncated_and_unbounded_images() {
        for offset in [0x24, 0x2c, 0x30, 0x34, 0x38, 64, 68, 72, 80, 84, 88] {
            let mut kernel = image();
            kernel[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(ZImage::parse(&kernel).is_err(), "offset {offset:#x}");
        }
        for length in [0, 36, 40, 59, 128, 4095] {
            assert!(ZImage::parse(&image()[..length]).is_err());
        }
    }

    #[test]
    fn strips_appended_dtb_and_places_separate_segments_after_workspace() {
        let mut kernel = image();
        kernel.extend_from_slice(&dtb(&[]));
        let plan = Plan::build(&kernel, Some(&[1; 8193]), &dtb(&[]), "test", RAM).unwrap();
        assert_eq!(plan.entry, 0x80208000);
        assert_eq!(plan.segments.len(), 3);
        assert_eq!(plan.segments[0].name, "zImage");
        assert_eq!(plan.segments[0].data.len(), 4096);
        assert_eq!(plan.segments[0].data.as_ptr(), kernel.as_ptr());
        assert_eq!(plan.segments[0].range.end - plan.entry, 0x424000);
        assert_eq!(plan.segments[1].range.start, 0x8062c000);
        assert_eq!(plan.segments[2].data[..4], [0xd0, 0x0d, 0xfe, 0xed]);
        for (i, segment) in plan.segments.iter().enumerate() {
            assert_eq!(segment.range.start % PAGE_SIZE, 0);
            assert_eq!(segment.range.end % PAGE_SIZE, 0);
            assert!(segment.range.end <= 0x88000000);
            for other in &plan.segments[i + 1..] {
                assert!(!segment.range.overlaps(other.range));
            }
        }
    }

    #[test]
    fn zero_fill_covers_the_relocated_appended_dtb_probe() {
        let kernel = image(); // Its _edata is 32-byte aligned.
        let plan = Plan::build(&kernel, None, &dtb(&[]), "", RAM).unwrap();
        let segment = &plan.segments[0];
        let memsz = (segment.range.end - segment.range.start) as usize;
        let relocated_edata = 0x400000 + kernel.len();
        assert!(relocated_edata + 4 <= memsz);
        // Model kexec's copy plus zero-fill over stale destination contents.
        let mut memory = vec![0xd0; memsz];
        memory[relocated_edata..relocated_edata + 4].copy_from_slice(&0xd00dfeedu32.to_be_bytes());
        memory[..segment.data.len()].copy_from_slice(&segment.data);
        memory[segment.data.len()..].fill(0);
        assert_eq!(&memory[kernel.len()..kernel.len() + 4], &[0; 4]);
        assert_eq!(&memory[relocated_edata..relocated_edata + 4], &[0; 4]);
    }

    #[test]
    fn selects_a_single_bounded_appended_dtb() {
        let blob = dtb(&[]);
        let mut kernel = image();
        assert!(appended_dtb(&kernel).unwrap().is_none());
        kernel.extend_from_slice(&blob);
        kernel.extend_from_slice(&[0; 8]);
        assert_eq!(appended_dtb(&kernel).unwrap(), Some(blob.as_slice()));
        kernel.extend_from_slice(&blob);
        assert!(appended_dtb(&kernel).is_err());
        let mut truncated = image();
        truncated.extend_from_slice(&blob[..blob.len() - 1]);
        assert!(appended_dtb(&truncated).is_err());
    }

    #[test]
    fn reservation_in_output_area_is_not_avoided_by_moving_the_kernel() {
        assert!(Plan::build(&image(), None, &dtb(&[(0x80400000, 0x1000)]), "", RAM).is_err());
        let iomem = format!("{RAM}  80400000-80400fff : reserved\n");
        assert!(Plan::build(&image(), None, &dtb(&[]), "", &iomem).is_err());
    }

    #[test]
    fn rejects_disagreement_with_the_decompressors_dt_ram_base() {
        for iomem in [
            "80201000-bfffffff : System RAM",
            "80400000-bfffffff : System RAM",
            "80000000-bfffffff : System RAM",
        ] {
            assert!(Plan::build(&image(), None, &dtb(&[]), "", iomem).is_err());
        }
    }

    #[test]
    fn initrd_placement_skips_reservations_and_obeys_the_boot_window() {
        let kernel = image();
        let plan = Plan::build(
            &kernel,
            Some(&[1; 4096]),
            &dtb(&[(0x8062c000, 0x1000)]),
            "",
            RAM,
        )
        .unwrap();
        assert_eq!(plan.segments[1].range.start, 0x8062d000);
        let tiny = "80200000-8062bfff : System RAM";
        assert!(Plan::build(&image(), Some(&[1; 4096]), &dtb(&[]), "", tiny).is_err());
        let mut huge = image();
        huge[76..80].copy_from_slice(&0x8000000u32.to_le_bytes());
        assert!(Plan::build(&huge, None, &dtb(&[]), "", RAM).is_err());
        assert!(
            Plan::build(
                &image(),
                None,
                &dtb(&[]),
                "",
                "100000000-1ffffffff : System RAM"
            )
            .is_err()
        );
    }

    #[test]
    fn metadata_cannot_escape_above_the_boot_window_even_when_ram_exists() {
        let kernel = image();
        let tree = dtb(&[(0x8062c000, 0x88000000 - 0x8062c000)]);
        for initrd in [None, Some(&[1; 4096][..])] {
            let error = Plan::build(&kernel, initrd, &tree, "", RAM).err().unwrap();
            assert!(error.to_string().contains("no ARM boot-window RAM"));
        }
    }

    #[test]
    fn large_dtb_fits_in_the_two_early_mapped_sections() {
        let kernel = image();
        let mut tree = dtb(&[]);
        // Unused strings are legal and retained by /chosen patching.
        let strings = u32::from_be_bytes(tree[32..36].try_into().unwrap());
        tree.resize(tree.len() + 0x180000, 0);
        let total = tree.len() as u32;
        tree[4..8].copy_from_slice(&total.to_be_bytes());
        tree[32..36].copy_from_slice(&(strings + 0x180000).to_be_bytes());
        // Without section alignment this puts r2 just below a section boundary.
        let initrd = vec![1; 0xd3000];
        let plan = Plan::build(&kernel, Some(&initrd), &tree, "", RAM).unwrap();
        let dtb = plan.segments.last().unwrap();
        assert_eq!(dtb.name, "dtb");
        assert!(dtb.data.len() > DTB_ALIGNMENT as usize);
        assert_eq!(dtb.range.start % DTB_ALIGNMENT, 0);
        assert!(dtb.range.end <= dtb.range.start + 2 * DTB_ALIGNMENT);
    }

    #[test]
    fn absent_or_empty_initrd_uses_only_kernel_and_dtb_segments() {
        let kernel = image();
        for initrd in [None, Some(&[][..])] {
            let plan = Plan::build(&kernel, initrd, &dtb(&[]), "", RAM).unwrap();
            assert_eq!(plan.segments.len(), 2);
        }
    }
}
