use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    time::Instant,
};

use flate2::read::{GzDecoder, MultiGzDecoder};
use ruzstd::decoding::StreamingDecoder;

use crate::{cmdline, pe, quiesce, zboot};

const LINUX_REBOOT_CMD_KEXEC: libc::c_int = 0x45584543;
const NO_KEXEC_QUIESCE_CMDLINE_PARAM: &str = "pocketboot.no_kexec_quiesce";
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ARM64_IMAGE_MAGIC_OFFSET: usize = 56;
const ARM64_IMAGE_MAGIC_BYTES: &[u8; 4] = b"ARM\x64";
const ARM64_IMAGE_MIN_SIZE: usize = 64;
const ARM64_PAGE_SIZE: u64 = 4096;
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
                with_live_memory(read_payload(dtb)?)?
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

pub(crate) fn prepare_kernel_payload(mut kernel: File) -> io::Result<File> {
    let mut payload = Vec::new();
    kernel.read_to_end(&mut payload)?;
    kernel.seek(SeekFrom::Start(0))?;

    let Some(prepared) = prepare_kernel_payload_bytes(&payload)? else {
        return Ok(kernel);
    };
    memfd_payload("kernel-prepared", &prepared)
}

pub(crate) fn exec_loaded_image() -> io::Result<()> {
    quiesce_before_kexec();

    let rc = unsafe { libc::reboot(LINUX_REBOOT_CMD_KEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    Err(io::Error::other(
        "reboot(LINUX_REBOOT_CMD_KEXEC) returned unexpectedly",
    ))
}

fn quiesce_before_kexec() {
    let cmdline = cmdline::KernelCommandLine::read("/proc/cmdline").unwrap_or_default();
    if cmdline.is_set(NO_KEXEC_QUIESCE_CMDLINE_PARAM) {
        tracing::info!(
            param = NO_KEXEC_QUIESCE_CMDLINE_PARAM,
            "skipping pre-kexec display quiesce"
        );
        return;
    }

    match quiesce::quiesce_display(Path::new("/sys"), Path::new("/proc")) {
        Ok(report) => tracing::info!(report = %report, "pre-kexec display quiesce complete"),
        Err(err) => tracing::warn!(error = %err, "pre-kexec display quiesce failed; continuing"),
    }
}

fn read_payload(file: &File) -> io::Result<Vec<u8>> {
    let mut file = File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    file.seek(SeekFrom::Start(0))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(data)
}

fn memfd_payload(name: &str, data: &[u8]) -> io::Result<File> {
    let mut payload = create_payload_memfd(name)?;
    io::Write::write_all(&mut payload, data)?;
    payload.seek(SeekFrom::Start(0))?;
    reopen_payload_readonly(payload)
}

fn prepare_kernel_payload_bytes(payload: &[u8]) -> io::Result<Option<Vec<u8>>> {
    tracing::debug!(bytes = payload.len(), "preparing kernel payload");
    if is_raw_arm64_image(payload) {
        tracing::debug!(
            bytes = payload.len(),
            "detected raw arm64 Image kernel payload"
        );
        return Ok(None);
    }
    if payload.starts_with(&GZIP_MAGIC) {
        tracing::debug!(
            bytes = payload.len(),
            "detected top-level gzip kernel payload"
        );
        return decompress_toplevel_gzip(payload).map(Some);
    }

    let Some(pe) = pe::Image::parse(payload)? else {
        tracing::debug!(
            bytes = payload.len(),
            "kernel payload does not need PE/gzip preparation"
        );
        return Ok(None);
    };
    tracing::debug!(
        machine = %format_args!("0x{:04x}", pe.machine()),
        sections = pe.sections().len(),
        bytes = pe.data().len(),
        "detected PE/COFF kernel payload"
    );

    if pe.machine() != pe::IMAGE_FILE_MACHINE_ARM64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("PE/COFF image is not ARM64: machine=0x{:04x}", pe.machine()),
        ));
    }

    extract_pe_arm64_kernel(&pe).map(Some)
}

fn decompress_toplevel_gzip(payload: &[u8]) -> io::Result<Vec<u8>> {
    tracing::info!(bytes = payload.len(), "decompressing gzip kernel image");
    let started = Instant::now();
    let mut decoder = MultiGzDecoder::new(payload);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).map_err(|err| {
        io::Error::new(err.kind(), format!("decompress gzip kernel image: {err}"))
    })?;
    tracing::info!(
        compressed_bytes = payload.len(),
        bytes = decompressed.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "decompressed gzip kernel image"
    );
    Ok(decompressed)
}

fn extract_pe_arm64_kernel(pe: &pe::Image<'_>) -> io::Result<Vec<u8>> {
    tracing::debug!(
        sections = pe.sections().len(),
        bytes = pe.data().len(),
        "extracting arm64 Image from PE/COFF kernel"
    );
    if let Some(zboot) = zboot::Image::parse(pe.data())? {
        let image = decompress_zboot_payload(&zboot)?;
        if !is_raw_arm64_image(&image) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "zboot payload did not decompress to a raw arm64 Image",
            ));
        }
        tracing::info!(
            compression = zboot.compression_name(),
            offset = zboot.payload_offset(),
            compressed_bytes = zboot.payload().len(),
            bytes = image.len(),
            "extracted Linux EFI zboot arm64 Image"
        );
        return Ok(image);
    }
    tracing::debug!("PE/COFF kernel is not a Linux EFI zboot image");

    for section in pe.sections() {
        tracing::debug!(
            section = %section.name(),
            raw_offset = section.raw_offset(),
            bytes = section.data().len(),
            "scanning PE/COFF section for raw arm64 Image"
        );
        if let Some((offset, image)) = find_raw_arm64_image(section.data()) {
            tracing::info!(
                section = %section.name(),
                offset = section.raw_offset() + offset,
                bytes = image.len(),
                "extracted raw arm64 Image from PE/COFF kernel"
            );
            return Ok(image.to_vec());
        }
    }

    for section in pe.sections() {
        tracing::debug!(
            section = %section.name(),
            raw_offset = section.raw_offset(),
            bytes = section.data().len(),
            "scanning PE/COFF section for gzipped arm64 Image"
        );
        if let Some((offset, image)) = find_gzipped_arm64_image(section.data())? {
            tracing::info!(
                section = %section.name(),
                offset = section.raw_offset() + offset,
                bytes = image.len(),
                "extracted gzipped arm64 Image from PE/COFF kernel"
            );
            return Ok(image);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "PE/COFF ARM64 kernel does not contain a supported zboot, raw, or gzipped arm64 Image payload",
    ))
}

fn decompress_zboot_payload(image: &zboot::Image<'_>) -> io::Result<Vec<u8>> {
    tracing::debug!(
        compression = image.compression_name(),
        payload_offset = image.payload_offset(),
        compressed_bytes = image.payload().len(),
        "decompressing Linux EFI zboot payload"
    );
    let started = Instant::now();
    match image.compression() {
        zboot::Compression::Gzip => {
            let decompressed = decompress_embedded_gzip(image.payload()).map_err(|err| {
                io::Error::new(err.kind(), format!("decompress gzip zboot payload: {err}"))
            })?;
            tracing::debug!(
                compression = image.compression_name(),
                compressed_bytes = image.payload().len(),
                bytes = decompressed.len(),
                elapsed_ms = started.elapsed().as_millis(),
                "decompressed Linux EFI zboot payload"
            );
            Ok(decompressed)
        }
        zboot::Compression::Zstd => {
            let decompressed = decompress_zstd(image.payload())?;
            tracing::debug!(
                compression = image.compression_name(),
                compressed_bytes = image.payload().len(),
                bytes = decompressed.len(),
                elapsed_ms = started.elapsed().as_millis(),
                "decompressed Linux EFI zboot payload"
            );
            Ok(decompressed)
        }
        zboot::Compression::Unsupported => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported zboot compression type {}",
                image.compression_name()
            ),
        )),
    }
}

fn decompress_zstd(payload: &[u8]) -> io::Result<Vec<u8>> {
    tracing::debug!(compressed_bytes = payload.len(), "creating zstd decoder");
    let mut source = payload;
    let mut decoder = StreamingDecoder::new(&mut source).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("create zstd decoder: {err}"),
        )
    })?;
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|err| io::Error::new(err.kind(), format!("decompress zstd payload: {err}")))?;
    Ok(decompressed)
}

fn find_raw_arm64_image(payload: &[u8]) -> Option<(usize, &[u8])> {
    let started = Instant::now();
    let image = payload
        .windows(ARM64_IMAGE_MAGIC_BYTES.len())
        .position(|window| window == ARM64_IMAGE_MAGIC_BYTES)
        .and_then(|magic_offset| {
            let start = magic_offset.checked_sub(ARM64_IMAGE_MAGIC_OFFSET)?;
            let image = &payload[start..];
            is_raw_arm64_image(image).then_some((start, image))
        });
    match image {
        Some((offset, image)) => tracing::debug!(
            offset,
            scanned_bytes = payload.len(),
            bytes = image.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "found raw arm64 Image"
        ),
        None => tracing::debug!(
            scanned_bytes = payload.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "finished raw arm64 Image scan"
        ),
    }
    image
}

fn find_gzipped_arm64_image(payload: &[u8]) -> io::Result<Option<(usize, Vec<u8>)>> {
    let mut search_offset = 0;
    let mut candidates = 0usize;
    let started = Instant::now();
    while let Some(relative_offset) = payload[search_offset..]
        .windows(GZIP_MAGIC.len())
        .position(|window| window == GZIP_MAGIC)
    {
        let offset = search_offset + relative_offset;
        candidates += 1;
        tracing::debug!(
            offset,
            remaining_bytes = payload.len() - offset,
            candidates,
            "trying embedded gzip candidate"
        );
        let candidate_started = Instant::now();
        match decompress_embedded_gzip(&payload[offset..]) {
            Ok(decompressed) if is_raw_arm64_image(&decompressed) => {
                tracing::debug!(
                    offset,
                    candidates,
                    compressed_bytes = payload.len() - offset,
                    bytes = decompressed.len(),
                    elapsed_ms = candidate_started.elapsed().as_millis(),
                    total_elapsed_ms = started.elapsed().as_millis(),
                    "embedded gzip candidate is an arm64 Image"
                );
                return Ok(Some((offset, decompressed)));
            }
            Ok(decompressed) => {
                tracing::debug!(
                    offset,
                    candidates,
                    bytes = decompressed.len(),
                    elapsed_ms = candidate_started.elapsed().as_millis(),
                    "embedded gzip candidate did not contain an arm64 Image"
                );
                search_offset = offset + 1;
            }
            Err(err) => {
                tracing::debug!(
                    offset,
                    candidates,
                    elapsed_ms = candidate_started.elapsed().as_millis(),
                    error = %err,
                    "embedded gzip candidate failed to decompress"
                );
                search_offset = offset + 1;
            }
        }
    }
    tracing::debug!(
        bytes = payload.len(),
        candidates,
        elapsed_ms = started.elapsed().as_millis(),
        "finished embedded gzip scan"
    );
    Ok(None)
}

fn decompress_embedded_gzip(payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(payload);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("decompress embedded gzip kernel image: {err}"),
        )
    })?;
    Ok(decompressed)
}

fn is_raw_arm64_image(payload: &[u8]) -> bool {
    if payload.len() < ARM64_IMAGE_MIN_SIZE
        || payload
            .get(ARM64_IMAGE_MAGIC_OFFSET..ARM64_IMAGE_MAGIC_OFFSET + ARM64_IMAGE_MAGIC_BYTES.len())
            != Some(ARM64_IMAGE_MAGIC_BYTES)
    {
        return false;
    }

    let text_offset = u64::from_le_bytes(payload[8..16].try_into().unwrap());
    text_offset % ARM64_PAGE_SIZE == 0
}

fn read_current_dtb() -> io::Result<Vec<u8>> {
    let dtb = fs::read("/sys/firmware/fdt").map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("no DTB supplied and /sys/firmware/fdt could not be read: {err}"),
        )
    })?;
    // The live-tree fallback needs the same ownership checks and early memory
    // reservation as an independently supplied tree.
    #[cfg(target_arch = "aarch64")]
    {
        fdt::graft_spin_table(&dtb, &dtb)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        Ok(dtb)
    }
}

#[cfg(target_arch = "aarch64")]
fn with_live_memory(dtb: Vec<u8>) -> io::Result<Vec<u8>> {
    let live_dtb = fs::read("/sys/firmware/fdt").map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "DTB supplied but /sys/firmware/fdt could not be read for /memory graft: {err}"
            ),
        )
    })?;
    let grafted = fdt::graft_memory(&dtb, &live_dtb)?;
    let grafted = fdt::graft_spin_table(&grafted, &live_dtb)?;
    tracing::info!(
        bytes = grafted.len(),
        "grafted live memory and CPU handoff contract into supplied DTB"
    );
    Ok(grafted)
}

#[cfg(not(target_arch = "aarch64"))]
fn with_live_memory(dtb: Vec<u8>) -> io::Result<Vec<u8>> {
    Ok(dtb)
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

        let mut usable = read_usable_memory()?;
        for (start, end) in fdt::reserved_ranges(dtb)? {
            let reserved = PhysRange { start, end };
            tracing::debug!(
                start = format_args!("0x{:x}", reserved.start),
                end = format_args!("0x{:x}", reserved.end),
                "excluding destination DTB reserved memory from kexec placement"
            );
            subtract_range(&mut usable, reserved);
        }
        if usable.is_empty() {
            return invalid_data("destination DTB reservations exclude all usable System RAM");
        }
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_iomem_excludes_resident_kernel_image() {
            let iomem = "\
80000000-bfffffff : System RAM
80200000-808bffff : Kernel code
808c0000-80b3ffff : reserved
80b40000-80c5ffff : Kernel data
80c60000-80dbffff : Kernel bss
";
            let ranges = parse_iomem(iomem)
                .into_iter()
                .map(|range| (range.start, range.end))
                .collect::<Vec<_>>();

            assert_eq!(
                ranges,
                vec![(0x80000000, 0x80200000), (0x80dc0000, 0xc0000000)],
                "resident kernel image ranges must not be offered as usable RAM"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use ruzstd::encoding::{CompressionLevel, compress_to_vec};
    use std::io::Write;

    #[test]
    fn raw_kernel_payload_passes_through() {
        let raw = raw_arm64_image();
        let payload = payload_file("test-raw-kernel", &raw);

        let prepared = prepare_kernel_payload(payload).unwrap();

        assert_eq!(read_file(&prepared), raw);
    }

    #[test]
    fn raw_kernel_with_efi_mz_prefix_passes_through() {
        let mut raw = raw_arm64_image();
        raw[..2].copy_from_slice(b"MZ");
        let payload = payload_file("test-raw-kernel-mz", &raw);

        let prepared = prepare_kernel_payload(payload).unwrap();

        assert_eq!(read_file(&prepared), raw);
    }

    #[test]
    fn gzip_kernel_payload_decompresses() {
        let raw = b"decompressed arm64 Image payload";
        let payload = payload_file("test-gzip-kernel", &gzip(raw));

        let prepared = prepare_kernel_payload(payload).unwrap();

        assert_eq!(read_file(&prepared), raw);
    }

    #[test]
    fn pe_gzip_kernel_payload_decompresses() {
        let raw = raw_arm64_image();
        let section = [
            b"prefix".as_slice(),
            gzip(&raw).as_slice(),
            b"padding".as_slice(),
        ]
        .concat();
        let payload = payload_file("test-pe-gzip-kernel", &pe_arm64(&section));

        let prepared = prepare_kernel_payload(payload).unwrap();

        assert_eq!(read_file(&prepared), raw);
    }

    #[test]
    fn zboot_gzip_kernel_payload_decompresses() {
        let raw = raw_arm64_image();
        let payload = payload_file(
            "test-zboot-gzip-kernel",
            &zboot_pe_arm64("gzip", &gzip(&raw)),
        );

        let prepared = prepare_kernel_payload(payload).unwrap();

        assert_eq!(read_file(&prepared), raw);
    }

    #[test]
    fn zboot_zstd_kernel_payload_decompresses() {
        let raw = raw_arm64_image();
        let payload = payload_file(
            "test-zboot-zstd-kernel",
            &zboot_pe_arm64("zstd", &zstd(&raw)),
        );

        let prepared = prepare_kernel_payload(payload).unwrap();

        assert_eq!(read_file(&prepared), raw);
    }

    #[test]
    fn pe_without_arm64_payload_fails() {
        let payload = payload_file("test-pe-no-kernel", &pe_arm64(b"not a kernel"));

        let err = prepare_kernel_payload(payload).unwrap_err();

        assert!(
            err.to_string().contains("does not contain a supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn corrupt_gzip_kernel_payload_fails() {
        let payload = payload_file("test-corrupt-gzip-kernel", b"\x1f\x8bnot really gzip");

        let err = prepare_kernel_payload(payload).unwrap_err();

        assert!(
            err.to_string().contains("decompress gzip kernel image"),
            "unexpected error: {err}"
        );
    }

    fn payload_file(name: &str, data: &[u8]) -> File {
        let mut file = create_payload_memfd(name).unwrap();
        file.write_all(data).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        reopen_payload_readonly(file).unwrap()
    }

    fn read_file(file: &File) -> Vec<u8> {
        let mut file = file.try_clone().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut data = Vec::new();
        file.read_to_end(&mut data).unwrap();
        data
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn zstd(data: &[u8]) -> Vec<u8> {
        compress_to_vec(data, CompressionLevel::Fastest)
    }

    fn raw_arm64_image() -> Vec<u8> {
        let mut image = vec![0; 128];
        let image_size = image.len() as u64;
        image[8..16].copy_from_slice(&0x80000u64.to_le_bytes());
        image[16..24].copy_from_slice(&image_size.to_le_bytes());
        image[56..60].copy_from_slice(ARM64_IMAGE_MAGIC_BYTES);
        image
    }

    fn pe_arm64(section_data: &[u8]) -> Vec<u8> {
        const PE_OFFSET: usize = 0x80;
        const COFF_HEADER_SIZE: usize = 20;
        const OPTIONAL_HEADER_SIZE: usize = 0xf0;
        const SECTION_HEADER_SIZE: usize = 40;
        const PE32_PLUS_MAGIC: u16 = 0x20b;
        const PE_SIGNATURE: &[u8; 4] = b"PE\0\0";

        let section_header_offset =
            PE_OFFSET + PE_SIGNATURE.len() + COFF_HEADER_SIZE + OPTIONAL_HEADER_SIZE;
        let raw_offset = section_header_offset + SECTION_HEADER_SIZE;
        let mut image = vec![0; raw_offset + section_data.len()];

        image[..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&(PE_OFFSET as u32).to_le_bytes());
        image[PE_OFFSET..PE_OFFSET + PE_SIGNATURE.len()].copy_from_slice(PE_SIGNATURE);

        let coff_offset = PE_OFFSET + PE_SIGNATURE.len();
        image[coff_offset..coff_offset + 2]
            .copy_from_slice(&pe::IMAGE_FILE_MACHINE_ARM64.to_le_bytes());
        image[coff_offset + 2..coff_offset + 4].copy_from_slice(&1u16.to_le_bytes());
        image[coff_offset + 16..coff_offset + 18]
            .copy_from_slice(&(OPTIONAL_HEADER_SIZE as u16).to_le_bytes());

        let optional_header_offset = coff_offset + COFF_HEADER_SIZE;
        image[optional_header_offset..optional_header_offset + 2]
            .copy_from_slice(&PE32_PLUS_MAGIC.to_le_bytes());

        image[section_header_offset..section_header_offset + 8].copy_from_slice(b".gzdata\0");
        image[section_header_offset + 16..section_header_offset + 20]
            .copy_from_slice(&(section_data.len() as u32).to_le_bytes());
        image[section_header_offset + 20..section_header_offset + 24]
            .copy_from_slice(&(raw_offset as u32).to_le_bytes());
        image[raw_offset..raw_offset + section_data.len()].copy_from_slice(section_data);

        image
    }

    fn zboot_pe_arm64(compression: &str, payload: &[u8]) -> Vec<u8> {
        const LINUX_PE_MAGIC: &[u8; 4] = &0x8182_23cdu32.to_le_bytes();
        const LINUX_PE_MAGIC_OFFSET: usize = 0x38;
        const PAYLOAD_OFFSET_OFFSET: usize = 8;
        const PAYLOAD_SIZE_OFFSET: usize = 12;
        const COMPRESSION_OFFSET: usize = 24;
        const COMPRESSION_LEN: usize = 8;

        let mut image = pe_arm64(payload);
        let payload_offset = image.len() - payload.len();

        image[..4].copy_from_slice(b"MZ\0\0");
        image[4..8].copy_from_slice(b"zimg");
        image[PAYLOAD_OFFSET_OFFSET..PAYLOAD_OFFSET_OFFSET + 4]
            .copy_from_slice(&(payload_offset as u32).to_le_bytes());
        image[PAYLOAD_SIZE_OFFSET..PAYLOAD_SIZE_OFFSET + 4]
            .copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let compression = compression.as_bytes();
        assert!(compression.len() < COMPRESSION_LEN);
        image[COMPRESSION_OFFSET..COMPRESSION_OFFSET + compression.len()]
            .copy_from_slice(compression);
        image[LINUX_PE_MAGIC_OFFSET..LINUX_PE_MAGIC_OFFSET + LINUX_PE_MAGIC.len()]
            .copy_from_slice(LINUX_PE_MAGIC);

        image
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
    const PROP_ADDRESS_CELLS: &[u8] = b"#address-cells";
    const PROP_SIZE_CELLS: &[u8] = b"#size-cells";
    const PROP_REG: &[u8] = b"reg";
    const PROP_STATUS: &[u8] = b"status";

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

    struct MemoryFragment {
        data: Vec<u8>,
    }

    struct PropertyParts {
        len: u32,
        nameoff: u32,
        value_start: usize,
        value_end: usize,
        next: usize,
    }

    struct ReservedChild {
        enabled: bool,
        reg: Option<Vec<u8>>,
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

    pub(super) fn graft_memory(dtb: &[u8], live_dtb: &[u8]) -> io::Result<Vec<u8>> {
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

        let live_header = Header::parse(live_dtb)?;
        let live_struct_block = checked_slice(
            live_dtb,
            live_header.off_dt_struct,
            live_header.size_dt_struct,
            "live DTB structure block",
        )?;
        let live_strings = checked_slice(
            live_dtb,
            live_header.off_dt_strings,
            live_header.size_dt_strings,
            "live DTB strings block",
        )?;

        let memory = extract_memory_fragments(live_struct_block, live_strings, &mut strings)?;
        if memory.is_empty() {
            return invalid_data("live DTB has no root memory nodes");
        }

        let new_struct = graft_memory_structure(struct_block, &memory)?;
        build_dtb(&header, &reserve_map, &new_struct, &strings)
    }

    const SPIN_TABLE_COMPAT: &[u8] = b"pocketboot,spin-table-v1";
    const SPIN_TABLE_BYTES: u64 = 0x1000;
    const SPIN_TABLE_SLOTS: u64 = 0x400;
    const SPIN_TABLE_STRIDE: u64 = 0x80;

    struct SpinTableContract {
        base: u64,
    }

    /// Preserve the running CPUs' boot protocol when a boot entry supplies its
    /// own DTB. An unowned spin-table cannot safely be guessed into this ABI.
    pub(super) fn graft_spin_table(dtb: &[u8], live_dtb: &[u8]) -> io::Result<Vec<u8>> {
        let mut live = contract_tree(live_dtb)?;
        let Some(contract) = spin_table_contract(&live)? else {
            return Ok(dtb.to_vec());
        };
        if Header::parse(live_dtb)?.boot_cpuid_phys != 0 || Header::parse(dtb)?.boot_cpuid_phys != 0
        {
            return invalid_data("spin-table v1 requires CPU0 as the boot CPU");
        }
        // Validate the live ownership too: a second overlapping reservation
        // would make the persistent code's lifetime ambiguous.
        graft_parking_reservation(&mut live, &contract)?;
        validate_parking_reserve_map(live_dtb, &contract)?;
        let mut target = contract_tree(dtb)?;
        let live_cpus = cpu_nodes(&live)?;
        let target_cpus = cpu_nodes(&target)?;
        if live_cpus.keys().ne(target_cpus.keys()) {
            return invalid_data("destination CPU MPIDRs do not match the live spin-table CPUs");
        }

        let providers = power_domain_providers(&target)?;
        let cpus = target
            .children
            .iter_mut()
            .find(|node| node.name == b"cpus")
            .unwrap();
        for (mpidr, index) in target_cpus {
            let cpu = &mut cpus.children[index];
            cpu.set(b"enable-method", b"spin-table\0");
            cpu.set(
                b"cpu-release-addr",
                &(contract.base + SPIN_TABLE_SLOTS + mpidr * SPIN_TABLE_STRIDE).to_be_bytes(),
            );
            cpu.remove(b"cpu-idle-states");
            remove_psci_cpu_domains(cpu, &providers)?;
        }
        disable_psci(&mut target);
        graft_parking_reservation(&mut target, &contract)?;

        let header = Header::parse(dtb)?;
        let mut reservations = reserve_map(dtb, &header)?;
        let already_reserved = validate_parking_reserve_map(dtb, &contract)?;
        if !already_reserved {
            reservations.truncate(reservations.len() - 16);
            reservations.extend_from_slice(&contract.base.to_be_bytes());
            reservations.extend_from_slice(&SPIN_TABLE_BYTES.to_be_bytes());
            reservations.extend_from_slice(&[0; 16]);
        }
        build_contract_tree(dtb, &target, &reservations)
    }

    fn validate_parking_reserve_map(dtb: &[u8], contract: &SpinTableContract) -> io::Result<bool> {
        let header = Header::parse(dtb)?;
        let end = contract.base + SPIN_TABLE_BYTES;
        let mut already_reserved = false;
        for (start, existing_end) in reserve_map_ranges(dtb, &header)? {
            if start < end && contract.base < existing_end {
                if (start, existing_end) != (contract.base, end) {
                    return invalid_data("DTB reserve map overlaps the spin-table page");
                }
                already_reserved = true;
            }
        }
        Ok(already_reserved)
    }

    fn cpu_nodes(root: &ContractNode) -> io::Result<std::collections::BTreeMap<u64, usize>> {
        let cpus = root
            .child(b"cpus")
            .ok_or_else(|| invalid_data_error("DTB has no /cpus node"))?;
        let address_cells = cpus.cells(PROP_ADDRESS_CELLS, 2)?;
        if !(1..=2).contains(&address_cells) || cpus.cells(PROP_SIZE_CELLS, 0)? != 0 {
            return invalid_data("unsupported /cpus cell format");
        }
        let mut result = std::collections::BTreeMap::new();
        for (index, cpu) in cpus.children.iter().enumerate() {
            if cpu.get(b"device_type") != Some(b"cpu\0") || !cpu.enabled() {
                continue;
            }
            let reg = cpu
                .get(PROP_REG)
                .ok_or_else(|| invalid_data_error("CPU has no reg property"))?;
            if reg.len() != address_cells as usize * 4 {
                return invalid_data("CPU reg property does not encode one MPIDR");
            }
            let mpidr = decode_cells(reg);
            if mpidr & !0xff_00ff_ffff != 0 || result.insert(mpidr, index).is_some() {
                return invalid_data("CPU reg contains an invalid or duplicate MPIDR");
            }
        }
        Ok(result)
    }

    fn spin_table_contract(root: &ContractNode) -> io::Result<Option<SpinTableContract>> {
        let has_spin_table_cpu = root.child(b"cpus").is_some_and(|cpus| {
            cpus.children.iter().any(|cpu| {
                cpu.enabled()
                    && cpu.get(b"device_type") == Some(b"cpu\0")
                    && cpu.get(b"enable-method") == Some(b"spin-table\0")
            })
        });
        // The reserved page may be present in the board DTB before preboot
        // activates it. Ordinary PSCI boots must not inherit a dormant ABI.
        if !has_spin_table_cpu {
            return Ok(None);
        }
        let mut reservations = root
            .child(b"reserved-memory")
            .into_iter()
            .flat_map(|node| &node.children)
            .filter(|node| {
                node.get(b"compatible").is_some_and(|value| {
                    value
                        .split(|byte| *byte == 0)
                        .any(|item| item.starts_with(b"pocketboot,spin-table-"))
                })
            });
        let owned = reservations.next();
        if reservations.next().is_some() {
            return invalid_data("live DTB has multiple spin-table parking reservations");
        }
        let Some(owned) = owned else {
            return invalid_data("live spin-table CPUs have no owned parking contract");
        };
        if !owned.compatible(SPIN_TABLE_COMPAT)
            || !owned.enabled()
            || owned.get(b"no-map") != Some(&[])
            || owned.get(b"reusable").is_some()
        {
            return invalid_data("invalid or unsupported live spin-table parking reservation");
        }
        let reserved = root.child(b"reserved-memory").unwrap();
        let (address_cells, size_cells) = reserved_cells(root, reserved)?;
        let reg = owned
            .get(PROP_REG)
            .ok_or_else(|| invalid_data_error("live spin-table reservation has no reg"))?;
        let ranges = decode_reg_ranges(reg, address_cells, size_cells)?;
        if ranges.len() != 1 || reg.len() != (address_cells + size_cells) as usize * 4 {
            return invalid_data("spin-table v1 requires exactly one reservation range");
        }
        let (base, end) = ranges[0];
        if base == 0 || base % SPIN_TABLE_BYTES != 0 || end - base != SPIN_TABLE_BYTES {
            return invalid_data("spin-table v1 requires one aligned 4 KiB page");
        }
        let cpus = cpu_nodes(root)?;
        if !cpus.keys().copied().eq(0..4) {
            return invalid_data("spin-table v1 requires enabled CPU MPIDRs 0 through 3");
        }
        let nodes = &root.child(b"cpus").unwrap().children;
        for (mpidr, index) in cpus {
            let cpu = &nodes[index];
            let expected = (base + SPIN_TABLE_SLOTS + mpidr * SPIN_TABLE_STRIDE).to_be_bytes();
            if cpu.get(b"enable-method") != Some(b"spin-table\0")
                || cpu.get(b"cpu-release-addr") != Some(expected.as_slice())
            {
                return invalid_data(
                    "live CPU release address or enable method does not match spin-table v1",
                );
            }
        }
        Ok(Some(SpinTableContract { base }))
    }

    fn reserved_cells(root: &ContractNode, reserved: &ContractNode) -> io::Result<(u32, u32)> {
        if !reserved.enabled() || reserved.get(b"ranges") != Some(&[]) {
            return invalid_data(
                "spin-table requires an enabled /reserved-memory with empty ranges",
            );
        }
        let address_cells = reserved.cells(PROP_ADDRESS_CELLS, 0)?;
        let size_cells = reserved.cells(PROP_SIZE_CELLS, 0)?;
        // Linux rejects the whole reserved-memory subtree if these properties
        // are absent or differ from the root's cells.
        if !(1..=2).contains(&address_cells)
            || !(1..=2).contains(&size_cells)
            || address_cells != root.cells(PROP_ADDRESS_CELLS, 2)?
            || size_cells != root.cells(PROP_SIZE_CELLS, 1)?
        {
            return invalid_data(
                "unsupported or inconsistent spin-table reserved-memory cell format",
            );
        }
        Ok((address_cells, size_cells))
    }

    fn graft_parking_reservation(
        root: &mut ContractNode,
        contract: &SpinTableContract,
    ) -> io::Result<()> {
        let name = format!("spin-table@{:x}", contract.base).into_bytes();
        if root.child(b"reserved-memory").is_none() {
            let mut reserved = ContractNode::new(b"reserved-memory");
            reserved.set(
                PROP_ADDRESS_CELLS,
                &root.cells(PROP_ADDRESS_CELLS, 2)?.to_be_bytes(),
            );
            reserved.set(
                PROP_SIZE_CELLS,
                &root.cells(PROP_SIZE_CELLS, 1)?.to_be_bytes(),
            );
            reserved.set(b"ranges", &[]);
            root.children.push(reserved);
        }
        let (address_cells, size_cells) =
            reserved_cells(root, root.child(b"reserved-memory").unwrap())?;
        let reserved = root
            .children
            .iter_mut()
            .find(|node| node.name == b"reserved-memory")
            .unwrap();
        let end = contract.base + SPIN_TABLE_BYTES;
        let mut already_reserved = false;
        for node in &reserved.children {
            let is_contract = node.get(b"compatible").is_some_and(|value| {
                value
                    .split(|byte| *byte == 0)
                    .any(|item| item.starts_with(b"pocketboot,spin-table-"))
            });
            let ranges = node
                .get(PROP_REG)
                .map(|reg| decode_reg_ranges(reg, address_cells, size_cells))
                .transpose()?
                .unwrap_or_default();
            let overlaps = ranges
                .iter()
                .any(|&(start, other_end)| start < end && contract.base < other_end);
            if overlaps || is_contract || node.name == name {
                if !node.enabled()
                    || !node.compatible(SPIN_TABLE_COMPAT)
                    || node.get(b"no-map") != Some(&[])
                    || node.get(b"reusable").is_some()
                    || ranges != [(contract.base, end)]
                    || already_reserved
                {
                    return invalid_data(
                        "destination reserved-memory conflicts with the live spin-table page",
                    );
                }
                already_reserved = true;
            }
        }
        if !already_reserved {
            let mut node = ContractNode::new(&name);
            node.set(b"compatible", &[SPIN_TABLE_COMPAT, &[0]].concat());
            node.set(b"no-map", &[]);
            let mut reg = encode_cells(contract.base, address_cells)?;
            reg.extend(encode_cells(SPIN_TABLE_BYTES, size_cells)?);
            node.set(PROP_REG, &reg);
            reserved.children.push(node);
        }
        Ok(())
    }

    fn encode_cells(value: u64, count: u32) -> io::Result<Vec<u8>> {
        match count {
            1 if value <= u32::MAX as u64 => Ok((value as u32).to_be_bytes().to_vec()),
            2 => Ok(value.to_be_bytes().to_vec()),
            _ => invalid_data("spin-table physical address does not fit destination DTB cells"),
        }
    }

    struct PowerDomainProvider {
        phandle: u32,
        cells: u32,
        psci: bool,
    }

    fn is_psci(node: &ContractNode) -> bool {
        [b"arm,psci".as_slice(), b"arm,psci-0.2", b"arm,psci-1.0"]
            .iter()
            .any(|value| node.compatible(value))
    }

    fn power_domain_providers(root: &ContractNode) -> io::Result<Vec<PowerDomainProvider>> {
        fn visit(
            node: &ContractNode,
            psci_parent: bool,
            providers: &mut Vec<PowerDomainProvider>,
        ) -> io::Result<()> {
            let psci = psci_parent || is_psci(node);
            if let Some(cells) = node.get(b"#power-domain-cells") {
                let phandle = node.get(b"phandle").or_else(|| node.get(b"linux,phandle"));
                if let Some(phandle) = phandle {
                    let phandle = property_u32(phandle, b"phandle")?;
                    if phandle == 0
                        || phandle == u32::MAX
                        || providers.iter().any(|provider| provider.phandle == phandle)
                    {
                        return invalid_data("invalid or duplicate power-domain provider phandle");
                    }
                    providers.push(PowerDomainProvider {
                        phandle,
                        cells: property_u32(cells, b"#power-domain-cells")?,
                        psci,
                    });
                }
            }
            for child in &node.children {
                visit(child, psci, providers)?;
            }
            Ok(())
        }
        let mut providers = Vec::new();
        visit(root, false, &mut providers)?;
        Ok(providers)
    }

    fn remove_psci_cpu_domains(
        cpu: &mut ContractNode,
        providers: &[PowerDomainProvider],
    ) -> io::Result<()> {
        let Some(domains) = cpu.get(b"power-domains") else {
            return Ok(());
        };
        if domains.len() % 4 != 0 {
            return invalid_data("CPU power-domains contains a partial cell");
        }
        let mut retained = Vec::new();
        let mut keep_names = Vec::new();
        let mut offset = 0;
        while offset < domains.len() {
            let phandle = read_be32(domains, offset)?;
            let provider = providers
                .iter()
                .find(|provider| provider.phandle == phandle)
                .ok_or_else(|| {
                    invalid_data_error("CPU power-domains refers to an unknown provider")
                })?;
            let bytes = (u64::from(provider.cells) + 1) * 4;
            if bytes > (domains.len() - offset) as u64 {
                return invalid_data("CPU power-domains contains a partial provider tuple");
            }
            let end = offset + bytes as usize;
            keep_names.push(!provider.psci);
            if !provider.psci {
                retained.extend_from_slice(&domains[offset..end]);
            }
            offset = end;
        }
        let retained_names = if let Some(names) = cpu.get(b"power-domain-names") {
            let Some(names) = names.strip_suffix(&[0]) else {
                return invalid_data("unterminated CPU power-domain-names");
            };
            let names: Vec<_> = names.split(|byte| *byte == 0).collect();
            if names.len() != keep_names.len() {
                return invalid_data("CPU power-domain-names does not match its provider tuples");
            }
            let mut retained_names = Vec::new();
            for (name, keep) in names.into_iter().zip(keep_names) {
                if keep {
                    retained_names.extend_from_slice(name);
                    retained_names.push(0);
                }
            }
            Some(retained_names)
        } else {
            None
        };
        if retained.is_empty() {
            cpu.remove(b"power-domains");
            cpu.remove(b"power-domain-names");
        } else {
            cpu.set(b"power-domains", &retained);
            if let Some(names) = retained_names {
                cpu.set(b"power-domain-names", &names);
            }
        }
        Ok(())
    }

    fn disable_psci(node: &mut ContractNode) {
        if is_psci(node) {
            node.set(PROP_STATUS, b"disabled\0");
        }
        // PSCI idle states cannot be entered by CPUs that firmware did not boot.
        node.children.retain(|child| {
            child.get(b"entry-method") != Some(b"psci\0")
                && child.get(b"arm,psci-suspend-param").is_none()
        });
        for child in &mut node.children {
            disable_psci(child);
        }
    }

    // An owned tree is used only for the spin-table contract: names and phandles
    // in an independently supplied DTB need not match those in the live tree.
    #[derive(Clone, Debug)]
    struct ContractNode {
        name: Vec<u8>,
        props: Vec<(Vec<u8>, Vec<u8>)>,
        children: Vec<ContractNode>,
    }

    impl ContractNode {
        fn new(name: &[u8]) -> Self {
            Self {
                name: name.to_vec(),
                props: Vec::new(),
                children: Vec::new(),
            }
        }

        fn get(&self, name: &[u8]) -> Option<&[u8]> {
            self.props
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_slice())
        }

        fn set(&mut self, name: &[u8], value: &[u8]) {
            if let Some((_, old)) = self.props.iter_mut().find(|(key, _)| key == name) {
                *old = value.to_vec();
            } else {
                self.props.push((name.to_vec(), value.to_vec()));
            }
        }

        fn remove(&mut self, name: &[u8]) {
            self.props.retain(|(key, _)| key != name);
        }

        fn child(&self, name: &[u8]) -> Option<&Self> {
            self.children.iter().find(|node| node.name == name)
        }

        fn enabled(&self) -> bool {
            self.get(PROP_STATUS).is_none_or(property_status_enabled)
        }

        fn compatible(&self, value: &[u8]) -> bool {
            self.get(b"compatible")
                .is_some_and(|list| list.split(|byte| *byte == 0).any(|item| item == value))
        }

        fn cells(&self, name: &[u8], default: u32) -> io::Result<u32> {
            self.get(name)
                .map(|value| property_u32(value, name))
                .unwrap_or(Ok(default))
        }

        fn write(&self, structure: &mut Vec<u8>, strings: &mut Vec<u8>) -> io::Result<()> {
            write_begin_node(structure, &self.name);
            for (name, value) in &self.props {
                let offset = ensure_string(strings, name)?;
                write_prop(structure, offset, value);
            }
            for child in &self.children {
                child.write(structure, strings)?;
            }
            write_be32(structure, FDT_END_NODE);
            Ok(())
        }
    }

    fn contract_tree(dtb: &[u8]) -> io::Result<ContractNode> {
        let header = Header::parse(dtb)?;
        let structure = checked_slice(
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
        let mut stack: Vec<ContractNode> = Vec::new();
        let mut root = None;
        let mut cursor = 0;
        loop {
            let token = read_be32(structure, cursor)?;
            cursor += 4;
            match token {
                FDT_BEGIN_NODE => {
                    let end = find_nul(structure, cursor)?;
                    let name = &structure[cursor..end];
                    if stack.is_empty() && (root.is_some() || !name.is_empty()) {
                        return invalid_data("DTB must have exactly one unnamed root");
                    }
                    if stack.len() >= 64 {
                        return invalid_data("DTB exceeds maximum node nesting depth");
                    }
                    stack.push(ContractNode::new(name));
                    cursor = align_usize(end + 1, 4)?;
                }
                FDT_END_NODE => {
                    let node = stack
                        .pop()
                        .ok_or_else(|| invalid_data_error("DTB has unmatched END_NODE"))?;
                    if let Some(parent) = stack.last_mut() {
                        if parent.child(&node.name).is_some() {
                            return invalid_data("DTB has duplicate sibling node names");
                        }
                        parent.children.push(node);
                    } else {
                        root = Some(node);
                    }
                }
                FDT_PROP => {
                    let parts = property_parts(structure, cursor)?;
                    let name = string_at(strings, parts.nameoff).ok_or_else(|| {
                        invalid_data_error("DTB property has invalid string offset")
                    })?;
                    let node = stack
                        .last_mut()
                        .ok_or_else(|| invalid_data_error("DTB property outside a node"))?;
                    if node.get(name).is_some() || !node.children.is_empty() {
                        return invalid_data(
                            "DTB has duplicate property or property after child nodes",
                        );
                    }
                    node.set(name, &structure[parts.value_start..parts.value_end]);
                    cursor = parts.next;
                }
                FDT_NOP => {}
                FDT_END if stack.is_empty() => {
                    return root.ok_or_else(|| invalid_data_error("DTB has no root node"));
                }
                FDT_END => return invalid_data("DTB has unterminated nodes"),
                _ => return invalid_data(format!("DTB has unknown structure token {token}")),
            }
        }
    }

    fn build_contract_tree(
        dtb: &[u8],
        root: &ContractNode,
        reserve_map: &[u8],
    ) -> io::Result<Vec<u8>> {
        let header = Header::parse(dtb)?;
        let mut structure = Vec::new();
        let mut strings = Vec::new();
        root.write(&mut structure, &mut strings)?;
        write_be32(&mut structure, FDT_END);
        build_dtb(&header, reserve_map, &structure, &strings)
    }

    pub(super) fn reserved_ranges(dtb: &[u8]) -> io::Result<Vec<(u64, u64)>> {
        let header = Header::parse(dtb)?;
        let mut ranges = reserve_map_ranges(dtb, &header)?;
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
        ranges.extend(reserved_memory_node_ranges(struct_block, strings)?);
        Ok(ranges)
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

            if !(40..=dtb.len()).contains(&header.totalsize) {
                return invalid_data(
                    "DTB totalsize is smaller than its header or exceeds payload size",
                );
            }
            // Padding or trailing payload bytes are not part of the DTB.
            // Validate every block against totalsize before following offsets.
            let dtb = &dtb[..header.totalsize];
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

    fn reserve_map_ranges(dtb: &[u8], header: &Header) -> io::Result<Vec<(u64, u64)>> {
        let mut cursor = header.off_mem_rsvmap;
        let mut ranges = Vec::new();
        loop {
            if cursor
                .checked_add(16)
                .is_none_or(|end| end > header.totalsize)
            {
                return invalid_data("DTB reserve map is unterminated");
            }
            let address = u64::from_be_bytes(dtb[cursor..cursor + 8].try_into().unwrap());
            let size = u64::from_be_bytes(dtb[cursor + 8..cursor + 16].try_into().unwrap());
            cursor += 16;
            if address == 0 && size == 0 {
                return Ok(ranges);
            }
            if size == 0 {
                continue;
            }
            let end = address
                .checked_add(size)
                .ok_or_else(|| invalid_data_error("DTB reserve map range overflows u64"))?;
            ranges.push((address, end));
        }
    }

    fn reserved_memory_node_ranges(
        struct_block: &[u8],
        strings: &[u8],
    ) -> io::Result<Vec<(u64, u64)>> {
        let mut cursor = 0usize;
        let mut stack: Vec<Vec<u8>> = Vec::new();
        let mut root_address_cells = 2u32;
        let mut root_size_cells = 1u32;
        let mut reserved_address_cells = None;
        let mut reserved_size_cells = None;
        let mut child = None;
        let mut ranges = Vec::new();

        loop {
            let token = read_be32(struct_block, cursor)?;
            cursor += 4;

            match token {
                FDT_BEGIN_NODE => {
                    let name_start = cursor;
                    let name_end = find_nul(struct_block, name_start)?;
                    let name = &struct_block[name_start..name_end];
                    cursor = align_usize(name_end + 1, 4)?;
                    stack.push(name.to_vec());
                    if is_reserved_memory_child(&stack) {
                        child = Some(ReservedChild {
                            enabled: true,
                            reg: None,
                        });
                    }
                }
                FDT_END_NODE => {
                    if is_reserved_memory_child(&stack) {
                        let child = child.take().ok_or_else(|| {
                            invalid_data_error("missing reserved-memory child state")
                        })?;
                        if child.enabled {
                            if let Some(reg) = child.reg {
                                let address_cells =
                                    reserved_address_cells.unwrap_or(root_address_cells);
                                let size_cells = reserved_size_cells.unwrap_or(root_size_cells);
                                ranges.extend(decode_reg_ranges(&reg, address_cells, size_cells)?);
                            }
                        }
                    }
                    if stack.pop().is_none() {
                        return invalid_data("DTB structure has too many END_NODE tokens");
                    }
                }
                FDT_PROP => {
                    let parts = property_parts(struct_block, cursor)?;
                    cursor = parts.next;
                    let name = string_at(strings, parts.nameoff).ok_or_else(|| {
                        invalid_data_error(format!(
                            "DTB property name offset {} is out of range",
                            parts.nameoff
                        ))
                    })?;
                    let value = &struct_block[parts.value_start..parts.value_end];

                    if is_root(&stack) {
                        if name == PROP_ADDRESS_CELLS {
                            root_address_cells = property_u32(value, name)?;
                        } else if name == PROP_SIZE_CELLS {
                            root_size_cells = property_u32(value, name)?;
                        }
                    } else if is_reserved_memory(&stack) {
                        if name == PROP_ADDRESS_CELLS {
                            reserved_address_cells = Some(property_u32(value, name)?);
                        } else if name == PROP_SIZE_CELLS {
                            reserved_size_cells = Some(property_u32(value, name)?);
                        }
                    } else if is_reserved_memory_child(&stack) {
                        let child = child.as_mut().ok_or_else(|| {
                            invalid_data_error("missing reserved-memory child state")
                        })?;
                        if name == PROP_REG {
                            child.reg = Some(value.to_vec());
                        } else if name == PROP_STATUS {
                            child.enabled = property_status_enabled(value);
                        }
                    }
                }
                FDT_NOP => {}
                FDT_END => {
                    if !stack.is_empty() {
                        return invalid_data("DTB structure ended before all nodes were closed");
                    }
                    return Ok(ranges);
                }
                _ => return invalid_data(format!("DTB has unknown structure token {token}")),
            }
        }
    }

    fn decode_reg_ranges(
        reg: &[u8],
        address_cells: u32,
        size_cells: u32,
    ) -> io::Result<Vec<(u64, u64)>> {
        let tuple_cells = address_cells
            .checked_add(size_cells)
            .ok_or_else(|| invalid_data_error("reserved-memory reg cell count overflow"))?;
        if !(1..=2).contains(&address_cells) || !(1..=2).contains(&size_cells) {
            return invalid_data(format!(
                "unsupported reserved-memory reg format: #address-cells={address_cells}, #size-cells={size_cells}"
            ));
        }
        let tuple_bytes = (tuple_cells as usize)
            .checked_mul(4)
            .ok_or_else(|| invalid_data_error("reserved-memory reg tuple size overflow"))?;
        if reg.len() % tuple_bytes != 0 {
            return invalid_data("reserved-memory reg property has a partial tuple");
        }

        let mut ranges = Vec::new();
        for tuple in reg.chunks_exact(tuple_bytes) {
            let address_bytes = address_cells as usize * 4;
            let address = decode_cells(&tuple[..address_bytes]);
            let size = decode_cells(&tuple[address_bytes..]);
            if size == 0 {
                continue;
            }
            let end = address
                .checked_add(size)
                .ok_or_else(|| invalid_data_error("reserved-memory reg range overflows u64"))?;
            ranges.push((address, end));
        }
        Ok(ranges)
    }

    fn decode_cells(cells: &[u8]) -> u64 {
        cells.chunks_exact(4).fold(0u64, |value, cell| {
            value << 32 | u32::from_be_bytes(cell.try_into().unwrap()) as u64
        })
    }

    fn property_u32(value: &[u8], name: &[u8]) -> io::Result<u32> {
        if value.len() != 4 {
            return invalid_data(format!(
                "DTB property {} is not one cell",
                String::from_utf8_lossy(name)
            ));
        }
        Ok(u32::from_be_bytes(value.try_into().unwrap()))
    }

    fn property_status_enabled(value: &[u8]) -> bool {
        let value = value.strip_suffix(&[0]).unwrap_or(value);
        value == b"ok" || value == b"okay"
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

    fn extract_memory_fragments(
        struct_block: &[u8],
        strings: &[u8],
        output_strings: &mut Vec<u8>,
    ) -> io::Result<Vec<MemoryFragment>> {
        let mut cursor = 0usize;
        let mut depth = 0usize;
        let mut saw_root = false;
        let mut memory = Vec::new();

        loop {
            let token_start = cursor;
            let token = read_be32(struct_block, cursor)?;
            cursor += 4;

            match token {
                FDT_BEGIN_NODE => {
                    let name_start = cursor;
                    let name_end = find_nul(struct_block, name_start)?;
                    let name = &struct_block[name_start..name_end];
                    let next = align_usize(name_end + 1, 4)?;

                    if depth == 0 {
                        if saw_root {
                            return invalid_data("DTB structure has multiple root nodes");
                        }
                        if !name.is_empty() {
                            return invalid_data("DTB root node name is not empty");
                        }
                        saw_root = true;
                        depth = 1;
                        cursor = next;
                    } else if depth == 1 && is_memory_node_name(name) {
                        let (data, next) =
                            copy_node_subtree(struct_block, strings, token_start, output_strings)?;
                        memory.push(MemoryFragment { data });
                        cursor = next;
                    } else {
                        depth += 1;
                        cursor = next;
                    }
                }
                FDT_END_NODE => {
                    if depth == 0 {
                        return invalid_data("DTB structure has too many END_NODE tokens");
                    }
                    depth -= 1;
                }
                FDT_PROP => cursor = property_parts(struct_block, cursor)?.next,
                FDT_NOP => {}
                FDT_END => {
                    if !saw_root {
                        return invalid_data("DTB structure has no root node");
                    }
                    if depth != 0 {
                        return invalid_data("DTB structure ended before all nodes were closed");
                    }
                    return Ok(memory);
                }
                _ => return invalid_data(format!("DTB has unknown structure token {token}")),
            }
        }
    }

    fn graft_memory_structure(
        struct_block: &[u8],
        memory: &[MemoryFragment],
    ) -> io::Result<Vec<u8>> {
        let memory_bytes: usize = memory.iter().map(|fragment| fragment.data.len()).sum();
        let mut cursor = 0usize;
        let mut depth = 0usize;
        let mut saw_root = false;
        let mut inserted_memory = false;
        let mut output = Vec::with_capacity(struct_block.len() + memory_bytes);

        loop {
            let token_start = cursor;
            let token = read_be32(struct_block, cursor)?;
            cursor += 4;

            match token {
                FDT_BEGIN_NODE => {
                    let name_start = cursor;
                    let name_end = find_nul(struct_block, name_start)?;
                    let name = &struct_block[name_start..name_end];
                    let next = align_usize(name_end + 1, 4)?;

                    if depth == 0 {
                        if saw_root {
                            return invalid_data("DTB structure has multiple root nodes");
                        }
                        if !name.is_empty() {
                            return invalid_data("DTB root node name is not empty");
                        }
                        saw_root = true;
                        output.extend_from_slice(&struct_block[token_start..next]);
                        depth = 1;
                        cursor = next;
                    } else if depth == 1 && is_memory_node_name(name) {
                        cursor = skip_node_subtree(struct_block, token_start)?;
                    } else {
                        if depth == 1 && !inserted_memory {
                            append_memory_fragments(&mut output, memory);
                            inserted_memory = true;
                        }
                        output.extend_from_slice(&struct_block[token_start..next]);
                        depth += 1;
                        cursor = next;
                    }
                }
                FDT_END_NODE => {
                    if depth == 0 {
                        return invalid_data("DTB structure has too many END_NODE tokens");
                    }
                    if depth == 1 && !inserted_memory {
                        append_memory_fragments(&mut output, memory);
                        inserted_memory = true;
                    }
                    output.extend_from_slice(&struct_block[token_start..cursor]);
                    depth -= 1;
                }
                FDT_PROP => {
                    let next = property_parts(struct_block, cursor)?.next;
                    output.extend_from_slice(&struct_block[token_start..next]);
                    cursor = next;
                }
                FDT_NOP => output.extend_from_slice(&struct_block[token_start..cursor]),
                FDT_END => {
                    output.extend_from_slice(&struct_block[token_start..cursor]);
                    if !saw_root {
                        return invalid_data("DTB structure has no root node");
                    }
                    if depth != 0 {
                        return invalid_data("DTB structure ended before all nodes were closed");
                    }
                    return Ok(output);
                }
                _ => return invalid_data(format!("DTB has unknown structure token {token}")),
            }
        }
    }

    fn copy_node_subtree(
        struct_block: &[u8],
        strings: &[u8],
        start: usize,
        output_strings: &mut Vec<u8>,
    ) -> io::Result<(Vec<u8>, usize)> {
        let mut cursor = start;
        let mut depth = 0usize;
        let mut output = Vec::new();

        loop {
            let token_start = cursor;
            let token = read_be32(struct_block, cursor)?;
            cursor += 4;

            match token {
                FDT_BEGIN_NODE => {
                    let name_start = cursor;
                    let name_end = find_nul(struct_block, name_start)?;
                    let next = align_usize(name_end + 1, 4)?;
                    output.extend_from_slice(&struct_block[token_start..next]);
                    cursor = next;
                    depth += 1;
                }
                FDT_END_NODE => {
                    if depth == 0 {
                        return invalid_data("DTB structure has too many END_NODE tokens");
                    }
                    output.extend_from_slice(&struct_block[token_start..cursor]);
                    depth -= 1;
                    if depth == 0 {
                        return Ok((output, cursor));
                    }
                }
                FDT_PROP => {
                    let parts = property_parts(struct_block, cursor)?;
                    let name = string_at(strings, parts.nameoff).ok_or_else(|| {
                        invalid_data_error(format!(
                            "DTB property name offset {} is out of range",
                            parts.nameoff
                        ))
                    })?;
                    let nameoff = ensure_string(output_strings, name)?;

                    write_be32(&mut output, FDT_PROP);
                    write_be32(&mut output, parts.len);
                    write_be32(&mut output, nameoff);
                    output.extend_from_slice(&struct_block[parts.value_start..parts.value_end]);
                    pad_to(&mut output, 4);
                    cursor = parts.next;
                }
                FDT_NOP => output.extend_from_slice(&struct_block[token_start..cursor]),
                FDT_END => return invalid_data("DTB node subtree is unterminated"),
                _ => return invalid_data(format!("DTB has unknown structure token {token}")),
            }
        }
    }

    fn skip_node_subtree(struct_block: &[u8], start: usize) -> io::Result<usize> {
        let mut cursor = start;
        let mut depth = 0usize;

        loop {
            let token = read_be32(struct_block, cursor)?;
            cursor += 4;

            match token {
                FDT_BEGIN_NODE => {
                    let name_end = find_nul(struct_block, cursor)?;
                    cursor = align_usize(name_end + 1, 4)?;
                    depth += 1;
                }
                FDT_END_NODE => {
                    if depth == 0 {
                        return invalid_data("DTB structure has too many END_NODE tokens");
                    }
                    depth -= 1;
                    if depth == 0 {
                        return Ok(cursor);
                    }
                }
                FDT_PROP => cursor = property_parts(struct_block, cursor)?.next,
                FDT_NOP => {}
                FDT_END => return invalid_data("DTB node subtree is unterminated"),
                _ => return invalid_data(format!("DTB has unknown structure token {token}")),
            }
        }
    }

    fn append_memory_fragments(output: &mut Vec<u8>, memory: &[MemoryFragment]) {
        for fragment in memory {
            output.extend_from_slice(&fragment.data);
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

    fn is_memory_node_name(name: &[u8]) -> bool {
        name == b"memory" || name.starts_with(b"memory@")
    }

    fn is_root(stack: &[Vec<u8>]) -> bool {
        stack.len() == 1 && stack[0].is_empty()
    }

    fn is_chosen(stack: &[Vec<u8>]) -> bool {
        stack.len() == 2 && stack[0].is_empty() && stack[1] == b"chosen"
    }

    fn is_reserved_memory(stack: &[Vec<u8>]) -> bool {
        stack.len() == 2 && stack[0].is_empty() && stack[1] == b"reserved-memory"
    }

    fn is_reserved_memory_child(stack: &[Vec<u8>]) -> bool {
        stack.len() == 3 && stack[0].is_empty() && stack[1] == b"reserved-memory"
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

    fn property_parts(data: &[u8], cursor: usize) -> io::Result<PropertyParts> {
        let len = read_be32(data, cursor)?;
        let nameoff = read_be32(data, cursor + 4)?;
        let value_start = cursor
            .checked_add(8)
            .ok_or_else(|| invalid_data_error("DTB property value offset overflow"))?;
        let value_end = value_start
            .checked_add(len as usize)
            .ok_or_else(|| invalid_data_error("DTB property length overflow"))?;
        let next = align_usize(value_end, 4)?;
        if next > data.len() {
            return invalid_data("DTB property extends past structure block");
        }

        Ok(PropertyParts {
            len,
            nameoff,
            value_start,
            value_end,
            next,
        })
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

        #[derive(Debug, PartialEq, Eq)]
        struct TestNode {
            name: String,
            props: Vec<(String, Vec<u8>)>,
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

        #[test]
        fn graft_memory_replaces_supplied_memory_nodes() {
            let old_reg = b"old memory";
            let live_reg = b"live memory";
            let target = test_dtb(|structure, strings| {
                write_memory_node(structure, strings, b"memory@deadbeef", old_reg);
                write_empty_node(structure, b"cpus");
            });
            let live = test_dtb(|structure, strings| {
                write_memory_node(structure, strings, b"memory@80000000", live_reg);
            });

            let grafted = graft_memory(&target, &live).unwrap();
            let memory = memory_nodes(&grafted).unwrap();

            assert_eq!(memory.len(), 1);
            assert_eq!(memory[0].name, "memory@80000000");
            assert_eq!(prop(&memory[0], "reg"), Some(live_reg.as_slice()));
            assert_ne!(prop(&memory[0], "reg"), Some(old_reg.as_slice()));
        }

        #[test]
        fn graft_memory_inserts_when_supplied_dtb_has_no_memory() {
            let live_reg = b"live memory";
            let target = test_dtb(|structure, _strings| {
                write_empty_node(structure, b"cpus");
            });
            let live = test_dtb(|structure, strings| {
                write_memory_node(structure, strings, b"memory", live_reg);
            });

            let grafted = graft_memory(&target, &live).unwrap();
            let memory = memory_nodes(&grafted).unwrap();

            assert_eq!(memory.len(), 1);
            assert_eq!(memory[0].name, "memory");
            assert_eq!(prop(&memory[0], "reg"), Some(live_reg.as_slice()));
        }

        #[test]
        fn graft_memory_preserves_chosen_patching() {
            let live_reg = b"live memory";
            let target = test_dtb_with_chosen_child();
            let live = test_dtb(|structure, strings| {
                write_memory_node(structure, strings, b"memory@80000000", live_reg);
            });

            let grafted = graft_memory(&target, &live).unwrap();
            let patched = patch_chosen(&grafted, "console=tty0", None).unwrap();
            let memory = memory_nodes(&patched).unwrap();
            let chosen = chosen_events(&patched).unwrap();

            assert_eq!(memory.len(), 1);
            assert_eq!(prop(&memory[0], "reg"), Some(live_reg.as_slice()));
            assert!(chosen.iter().any(|event| matches!(
                event,
                ChosenEvent::Prop(name, value)
                    if name == "bootargs" && value == b"console=tty0\0"
            )));
        }

        #[test]
        fn graft_memory_fails_when_live_dtb_has_no_memory() {
            let target = test_dtb(|structure, strings| {
                write_memory_node(structure, strings, b"memory@deadbeef", b"old memory");
            });
            let live = test_dtb(|structure, _strings| {
                write_empty_node(structure, b"cpus");
            });

            let err = graft_memory(&target, &live).unwrap_err();

            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(
                err.to_string()
                    .contains("live DTB has no root memory nodes"),
                "unexpected error: {err}"
            );
        }

        #[test]
        fn reserved_ranges_include_reserve_map_and_enabled_reserved_memory_nodes() {
            let mut reserve_map = Vec::new();
            reserve_map.extend_from_slice(&0x8100_0000u64.to_be_bytes());
            reserve_map.extend_from_slice(&0x0020_0000u64.to_be_bytes());
            reserve_map.extend_from_slice(&[0; 16]);

            let dtb = test_dtb_with_reserve_map(&reserve_map, |structure, strings| {
                let address_cells = ensure_string(strings, PROP_ADDRESS_CELLS).unwrap();
                let size_cells = ensure_string(strings, PROP_SIZE_CELLS).unwrap();
                let ranges = ensure_string(strings, b"ranges").unwrap();
                let reg = ensure_string(strings, PROP_REG).unwrap();
                let status = ensure_string(strings, PROP_STATUS).unwrap();

                write_prop(structure, address_cells, &2u32.to_be_bytes());
                write_prop(structure, size_cells, &2u32.to_be_bytes());
                write_begin_node(structure, b"reserved-memory");
                write_prop(structure, address_cells, &2u32.to_be_bytes());
                write_prop(structure, size_cells, &2u32.to_be_bytes());
                write_prop(structure, ranges, &[]);

                write_begin_node(structure, b"mpss@86800000");
                write_prop(structure, reg, &reg64(&[(0x8680_0000, 0x0540_0000)]));
                write_be32(structure, FDT_END_NODE);

                write_begin_node(structure, b"firmware@8dc00000");
                write_prop(
                    structure,
                    reg,
                    &reg64(&[(0x8dc0_0000, 0x0010_0000), (0x8dd0_0000, 0x00b0_0000)]),
                );
                write_be32(structure, FDT_END_NODE);

                write_begin_node(structure, b"disabled@90000000");
                write_prop(structure, reg, &reg64(&[(0x9000_0000, 0x0010_0000)]));
                write_prop(structure, status, b"disabled\0");
                write_be32(structure, FDT_END_NODE);

                write_be32(structure, FDT_END_NODE);
            });

            assert_eq!(
                reserved_ranges(&dtb).unwrap(),
                vec![
                    (0x8100_0000, 0x8120_0000),
                    (0x8680_0000, 0x8bc0_0000),
                    (0x8dc0_0000, 0x8dd0_0000),
                    (0x8dd0_0000, 0x8e80_0000),
                ]
            );
        }

        const PARKING_BASE: u64 = 0x854f_f000;

        fn contract_fixture(
            spin_table: bool,
            reverse_cpus: bool,
            address_cells: u32,
        ) -> ContractNode {
            let mut root = ContractNode::new(b"");
            root.set(PROP_ADDRESS_CELLS, &address_cells.to_be_bytes());
            root.set(PROP_SIZE_CELLS, &address_cells.to_be_bytes());
            let mut cpus = ContractNode::new(b"cpus");
            cpus.set(PROP_ADDRESS_CELLS, &address_cells.to_be_bytes());
            cpus.set(PROP_SIZE_CELLS, &0u32.to_be_bytes());
            let ids: Vec<u64> = if reverse_cpus {
                (0..4).rev().collect()
            } else {
                (0..4).collect()
            };
            for id in ids {
                let mut cpu = ContractNode::new(format!("cpu@{id}").as_bytes());
                cpu.set(b"device_type", b"cpu\0");
                cpu.set(PROP_REG, &encode_cells(id, address_cells).unwrap());
                cpu.set(b"phandle", &(0x200 + id as u32).to_be_bytes());
                cpu.set(
                    b"enable-method",
                    if spin_table {
                        b"spin-table\0"
                    } else {
                        b"psci\0"
                    },
                );
                if spin_table {
                    cpu.set(
                        b"cpu-release-addr",
                        &(PARKING_BASE + 0x400 + id * 0x80).to_be_bytes(),
                    );
                }
                cpus.children.push(cpu);
            }
            root.children.push(cpus);
            if spin_table {
                let mut reserved = ContractNode::new(b"reserved-memory");
                reserved.set(PROP_ADDRESS_CELLS, &address_cells.to_be_bytes());
                reserved.set(PROP_SIZE_CELLS, &address_cells.to_be_bytes());
                reserved.set(b"ranges", &[]);
                let mut parking = ContractNode::new(b"live-parking@854ff000");
                parking.set(b"compatible", b"pocketboot,spin-table-v1\0");
                parking.set(b"no-map", &[]);
                // This deliberately collides with a target provider in one test.
                parking.set(b"phandle", &7u32.to_be_bytes());
                let mut reg = encode_cells(PARKING_BASE, address_cells).unwrap();
                reg.extend(encode_cells(0x1000, address_cells).unwrap());
                parking.set(PROP_REG, &reg);
                reserved.children.push(parking);
                root.children.push(reserved);
            }
            root
        }

        fn fixture_dtb(root: &ContractNode) -> Vec<u8> {
            fixture_dtb_with_reservations(root, &[0; 16])
        }

        fn fixture_dtb_with_reservations(root: &ContractNode, reserve_map: &[u8]) -> Vec<u8> {
            let template = test_dtb(|_, _| {});
            build_contract_tree(&template, root, reserve_map).unwrap()
        }

        fn fixture_cpu(root: &mut ContractNode, index: usize) -> &mut ContractNode {
            &mut root
                .children
                .iter_mut()
                .find(|node| node.name == b"cpus")
                .unwrap()
                .children[index]
        }

        fn fixture_parking(root: &mut ContractNode) -> &mut ContractNode {
            &mut root
                .children
                .iter_mut()
                .find(|node| node.name == b"reserved-memory")
                .unwrap()
                .children[0]
        }

        #[test]
        fn spin_table_graft_leaves_psci_boots_unchanged() {
            let target = fixture_dtb(&contract_fixture(false, true, 1));
            let live = fixture_dtb(&contract_fixture(false, false, 2));
            assert_eq!(graft_spin_table(&target, &live).unwrap(), target);
        }

        #[test]
        fn spin_table_graft_ignores_dormant_reservation_on_psci_boot() {
            let target = fixture_dtb(&contract_fixture(false, true, 1));
            let mut live = contract_fixture(true, false, 2);
            for index in 0..4 {
                let cpu = fixture_cpu(&mut live, index);
                cpu.set(b"enable-method", b"psci\0");
                cpu.remove(b"cpu-release-addr");
            }
            assert_eq!(
                graft_spin_table(&target, &fixture_dtb(&live)).unwrap(),
                target
            );
        }

        #[test]
        fn spin_table_graft_matches_mpidrs_and_reserves_parking_page() {
            let live = fixture_dtb(&contract_fixture(true, false, 2));
            let target = fixture_dtb(&contract_fixture(false, true, 1));
            let grafted = graft_spin_table(&target, &live).unwrap();
            let root = contract_tree(&grafted).unwrap();
            let cpus = root.child(b"cpus").unwrap();
            assert_eq!(
                cpus.children[0].get(PROP_REG),
                Some(3u32.to_be_bytes().as_slice())
            );
            for (id, index) in cpu_nodes(&root).unwrap() {
                let cpu = &cpus.children[index];
                assert_eq!(cpu.get(b"enable-method"), Some(b"spin-table\0".as_slice()));
                assert_eq!(
                    cpu.get(b"cpu-release-addr"),
                    Some((PARKING_BASE + 0x400 + id * 0x80).to_be_bytes().as_slice())
                );
                assert_eq!(
                    cpu.get(b"phandle"),
                    Some((0x200 + id as u32).to_be_bytes().as_slice())
                );
            }
            let reserved = root.child(b"reserved-memory").unwrap();
            let parking = &reserved.children[0];
            assert_eq!(
                parking.get(PROP_REG),
                Some(
                    [0x854f_f000u32.to_be_bytes(), 0x1000u32.to_be_bytes()]
                        .concat()
                        .as_slice()
                )
            );
            assert_eq!(parking.get(b"no-map"), Some([].as_slice()));
            assert_eq!(parking.get(b"phandle"), None);
            assert_eq!(
                reserved_ranges(&grafted).unwrap(),
                [(PARKING_BASE, PARKING_BASE + 0x1000); 2]
            );
            // /chosen rewriting must retain both forms of the reservation.
            let patched = patch_chosen(&grafted, "console=ttyMSM0", None).unwrap();
            assert_eq!(
                reserved_ranges(&patched).unwrap(),
                reserved_ranges(&grafted).unwrap()
            );
            assert_eq!(graft_spin_table(&grafted, &live).unwrap(), grafted);
        }

        #[test]
        fn spin_table_graft_rejects_invalid_live_contracts() {
            let target = fixture_dtb(&contract_fixture(false, false, 2));
            let valid = contract_fixture(true, false, 2);
            let mut cases = Vec::new();
            let mut missing = valid.clone();
            missing
                .children
                .retain(|node| node.name != b"reserved-memory");
            cases.push(missing);
            let mut no_map = valid.clone();
            fixture_parking(&mut no_map).remove(b"no-map");
            cases.push(no_map);
            let mut reusable = valid.clone();
            fixture_parking(&mut reusable).set(b"reusable", &[]);
            cases.push(reusable);
            let mut version = valid.clone();
            fixture_parking(&mut version).set(b"compatible", b"pocketboot,spin-table-v2\0");
            cases.push(version);
            let mut size = valid.clone();
            fixture_parking(&mut size).set(PROP_REG, &reg64(&[(PARKING_BASE, 0x800)]));
            cases.push(size);
            let mut unaligned = valid.clone();
            fixture_parking(&mut unaligned).set(PROP_REG, &reg64(&[(PARKING_BASE + 8, 0x1000)]));
            cases.push(unaligned);
            let mut release = valid.clone();
            fixture_cpu(&mut release, 2)
                .set(b"cpu-release-addr", &(PARKING_BASE + 0x480).to_be_bytes());
            cases.push(release);
            let mut psci = valid.clone();
            fixture_cpu(&mut psci, 2).set(b"enable-method", b"psci\0");
            cases.push(psci);
            let mut duplicate = valid.clone();
            fixture_cpu(&mut duplicate, 2).set(PROP_REG, &1u64.to_be_bytes());
            cases.push(duplicate);
            let mut overlapping = valid.clone();
            let reserved = overlapping
                .children
                .iter_mut()
                .find(|node| node.name == b"reserved-memory")
                .unwrap();
            let mut conflict = ContractNode::new(b"other-owner@854ff000");
            conflict.set(PROP_REG, &reg64(&[(PARKING_BASE + 0x800, 0x1000)]));
            reserved.children.push(conflict);
            cases.push(overlapping);
            let mut inconsistent_cells = valid.clone();
            inconsistent_cells.set(PROP_ADDRESS_CELLS, &1u32.to_be_bytes());
            cases.push(inconsistent_cells);
            for (index, live) in cases.iter().enumerate() {
                assert!(
                    graft_spin_table(&target, &fixture_dtb(live)).is_err(),
                    "invalid source case {index} accepted"
                );
            }
        }

        #[test]
        fn spin_table_graft_rejects_changed_destination_cpu_sets() {
            let live = fixture_dtb(&contract_fixture(true, false, 2));
            for reg in [4u64, 0x100, 0x1_0000_0000, 0x100_0000_0000] {
                let mut target = contract_fixture(false, false, 2);
                fixture_cpu(&mut target, 3).set(PROP_REG, &reg.to_be_bytes());
                assert!(graft_spin_table(&fixture_dtb(&target), &live).is_err());
            }
            let mut target = contract_fixture(false, false, 2);
            fixture_cpu(&mut target, 3).set(PROP_STATUS, b"disabled\0");
            assert!(graft_spin_table(&fixture_dtb(&target), &live).is_err());
            let mut target = contract_fixture(false, false, 2);
            fixture_cpu(&mut target, 3).set(PROP_REG, &3u32.to_be_bytes());
            assert!(graft_spin_table(&fixture_dtb(&target), &live).is_err());
        }

        #[test]
        fn spin_table_graft_rejects_destination_reservation_conflicts() {
            let live = fixture_dtb(&contract_fixture(true, false, 2));
            for (start, size, compatible) in [
                (PARKING_BASE, 0x1000, b"some,other-owner\0".as_slice()),
                (
                    PARKING_BASE - 0x1000,
                    0x1800,
                    b"some,other-owner\0".as_slice(),
                ),
                (
                    PARKING_BASE + 0x800,
                    0x1000,
                    b"some,other-owner\0".as_slice(),
                ),
                (
                    PARKING_BASE + 0x1000,
                    0x1000,
                    b"pocketboot,spin-table-v1\0".as_slice(),
                ),
            ] {
                let mut target = contract_fixture(true, false, 2);
                let parking = fixture_parking(&mut target);
                parking.set(b"compatible", compatible);
                parking.set(PROP_REG, &reg64(&[(start, size)]));
                assert!(graft_spin_table(&fixture_dtb(&target), &live).is_err());
            }
            let mut reserve_map = reg64(&[(PARKING_BASE - 0x1000, 0x3000)]);
            reserve_map.extend_from_slice(&[0; 16]);
            let target =
                fixture_dtb_with_reservations(&contract_fixture(false, false, 2), &reserve_map);
            assert!(graft_spin_table(&target, &live).is_err());
        }

        #[test]
        fn spin_table_graft_removes_only_psci_cpu_power_domain_tuples() {
            let live = fixture_dtb(&contract_fixture(true, false, 2));
            let mut target = contract_fixture(false, true, 1);
            let mut psci = ContractNode::new(b"firmware-cpu-service");
            psci.set(b"compatible", b"arm,psci-1.0\0");
            let mut domain = ContractNode::new(b"cpu-domain");
            domain.set(b"#power-domain-cells", &0u32.to_be_bytes());
            domain.set(b"phandle", &7u32.to_be_bytes());
            psci.children.push(domain);
            target.children.push(psci);
            let mut regulator = ContractNode::new(b"regulator-domain");
            regulator.set(b"#power-domain-cells", &1u32.to_be_bytes());
            regulator.set(b"phandle", &8u32.to_be_bytes());
            target.children.push(regulator);
            let cpu = fixture_cpu(&mut target, 0);
            cpu.set(
                b"power-domains",
                &[7u32.to_be_bytes(), 8u32.to_be_bytes(), 42u32.to_be_bytes()].concat(),
            );
            cpu.set(b"power-domain-names", b"psci\0voltage\0");
            cpu.set(b"cpu-idle-states", &9u32.to_be_bytes());
            let mut idle_states = ContractNode::new(b"idle-states");
            idle_states.set(b"entry-method", b"psci\0");
            target
                .children
                .iter_mut()
                .find(|node| node.name == b"cpus")
                .unwrap()
                .children
                .push(idle_states);
            let mut unrelated = ContractNode::new(b"unrelated-state");
            unrelated.set(b"compatible", b"vendor,idle-state\0");
            target.children.push(unrelated.clone());
            let grafted = graft_spin_table(&fixture_dtb(&target), &live).unwrap();
            let root = contract_tree(&grafted).unwrap();
            let cpu = &root.child(b"cpus").unwrap().children[0];
            assert_eq!(
                cpu.get(b"power-domains"),
                Some(
                    [8u32.to_be_bytes(), 42u32.to_be_bytes()]
                        .concat()
                        .as_slice()
                )
            );
            assert_eq!(
                cpu.get(b"power-domain-names"),
                Some(b"voltage\0".as_slice())
            );
            assert_eq!(cpu.get(b"cpu-idle-states"), None);
            assert_eq!(
                root.child(b"firmware-cpu-service")
                    .unwrap()
                    .get(PROP_STATUS),
                Some(b"disabled\0".as_slice())
            );
            assert!(root.child(b"cpus").unwrap().child(b"idle-states").is_none());
            assert!(root.child(b"unrelated-state").is_some());
            assert_eq!(
                root.child(b"reserved-memory").unwrap().children[0].get(b"phandle"),
                None
            );
        }

        #[test]
        fn spin_table_graft_rejects_unresolved_cpu_power_domains() {
            let live = fixture_dtb(&contract_fixture(true, false, 2));
            let mut target = contract_fixture(false, false, 2);
            fixture_cpu(&mut target, 1).set(b"power-domains", &99u32.to_be_bytes());
            assert!(graft_spin_table(&fixture_dtb(&target), &live).is_err());
        }

        #[test]
        fn spin_table_graft_rejects_blocks_outside_declared_dtb() {
            let live = fixture_dtb(&contract_fixture(true, false, 2));
            let target = fixture_dtb(&contract_fixture(false, false, 2));
            for (original, is_live) in [(&live, true), (&target, false)] {
                let mut malformed = original.clone();
                // The backing buffer still contains valid strings, but the
                // header says they are outside the DTB handed to Linux.
                let total = read_be32(&malformed, 12).unwrap();
                malformed[4..8].copy_from_slice(&total.to_be_bytes());
                let result = if is_live {
                    graft_spin_table(&target, &malformed)
                } else {
                    graft_spin_table(&malformed, &live)
                };
                assert!(
                    result.is_err(),
                    "accepted out-of-bounds strings, live={is_live}"
                );
            }
        }

        fn test_dtb_with_chosen_child() -> Vec<u8> {
            test_dtb(|structure, strings| {
                let bootargs = ensure_string(strings, PROP_BOOTARGS).unwrap();
                let compatible = ensure_string(strings, b"compatible").unwrap();

                write_begin_node(structure, b"chosen");
                write_prop(structure, bootargs, b"old-console=ttyS0\0");
                write_begin_node(structure, b"framebuffer@0");
                write_prop(structure, compatible, b"simple-framebuffer\0");
                write_be32(structure, FDT_END_NODE);
                write_be32(structure, FDT_END_NODE);
            })
        }

        fn test_dtb(build_root: impl FnOnce(&mut Vec<u8>, &mut Vec<u8>)) -> Vec<u8> {
            test_dtb_with_reserve_map(&[0; 16], build_root)
        }

        fn test_dtb_with_reserve_map(
            reserve_map: &[u8],
            build_root: impl FnOnce(&mut Vec<u8>, &mut Vec<u8>),
        ) -> Vec<u8> {
            let mut strings = Vec::new();
            let mut structure = Vec::new();

            write_begin_node(&mut structure, b"");
            build_root(&mut structure, &mut strings);
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
                reserve_map,
                &structure,
                &strings,
            )
            .unwrap()
        }

        fn reg64(ranges: &[(u64, u64)]) -> Vec<u8> {
            let mut reg = Vec::new();
            for (address, size) in ranges {
                reg.extend_from_slice(&address.to_be_bytes());
                reg.extend_from_slice(&size.to_be_bytes());
            }
            reg
        }

        fn write_memory_node(
            structure: &mut Vec<u8>,
            strings: &mut Vec<u8>,
            name: &[u8],
            reg: &[u8],
        ) {
            let device_type = ensure_string(strings, b"device_type").unwrap();
            let reg_name = ensure_string(strings, b"reg").unwrap();

            write_begin_node(structure, name);
            write_prop(structure, device_type, b"memory\0");
            write_prop(structure, reg_name, reg);
            write_be32(structure, FDT_END_NODE);
        }

        fn write_empty_node(structure: &mut Vec<u8>, name: &[u8]) {
            write_begin_node(structure, name);
            write_be32(structure, FDT_END_NODE);
        }

        fn prop<'a>(node: &'a TestNode, name: &str) -> Option<&'a [u8]> {
            node.props
                .iter()
                .find(|(prop_name, _)| prop_name == name)
                .map(|(_, value)| value.as_slice())
        }

        fn memory_nodes(dtb: &[u8]) -> io::Result<Vec<TestNode>> {
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
            let mut current_memory = None;
            let mut memory = Vec::new();

            loop {
                let token = read_be32(struct_block, cursor)?;
                cursor += 4;

                match token {
                    FDT_BEGIN_NODE => {
                        let name_start = cursor;
                        let name_end = find_nul(struct_block, name_start)?;
                        let name = &struct_block[name_start..name_end];
                        cursor = align_usize(name_end + 1, 4)?;

                        if stack.len() == 1 && is_memory_node_name(name) {
                            current_memory = Some(TestNode {
                                name: String::from_utf8_lossy(name).into_owned(),
                                props: Vec::new(),
                            });
                        }
                        stack.push(name.to_vec());
                    }
                    FDT_END_NODE => {
                        if stack.len() == 2 && is_memory_node_name(&stack[1]) {
                            memory.push(current_memory.take().unwrap());
                        }
                        stack.pop();
                    }
                    FDT_PROP => {
                        let parts = property_parts(struct_block, cursor)?;
                        cursor = parts.next;
                        if stack.len() == 2 && is_memory_node_name(&stack[1]) {
                            let name = string_at(strings, parts.nameoff)
                                .map(String::from_utf8_lossy)
                                .map(|name| name.into_owned())
                                .unwrap_or_else(|| format!("<invalid:{}>", parts.nameoff));
                            current_memory.as_mut().unwrap().props.push((
                                name,
                                struct_block[parts.value_start..parts.value_end].to_vec(),
                            ));
                        }
                    }
                    FDT_NOP => {}
                    FDT_END => return Ok(memory),
                    _ => return invalid_data(format!("unknown test DTB token: {token}")),
                }
            }
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
