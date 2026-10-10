use std::{
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
};

use abootimg_oxide::{Dtbh, Header, HeaderV0Versioned, Qcdt};

use crate::{
    fastboot::{CommandContext, CommandResult},
    kexec::{self, KexecImage},
};

const KEXEC_LOADED: &str = "/sys/kernel/kexec_loaded";

pub(super) fn handle_boot(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<CommandResult> {
    let image = prepare_staged_boot_image(context)?;
    image.load()?;
    context.okay_then_exit(b"booting", kexec::exec_loaded_image)
}

pub(super) fn handle_kexec_load(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<CommandResult> {
    let image = prepare_staged_boot_image(context)?;
    image.load()?;
    context.okay(b"loaded")?;
    Ok(CommandResult::continue_())
}

pub(super) fn handle_kexec_status(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<CommandResult> {
    let status = fs::read_to_string(KEXEC_LOADED)?;
    context.okay(status.trim().as_bytes())?;
    Ok(CommandResult::continue_())
}

fn prepare_staged_boot_image(context: &CommandContext<'_>) -> io::Result<KexecImage> {
    prepare_boot_image(context.staged_file()?)
}

fn prepare_boot_image(mut boot_img: fs::File) -> io::Result<KexecImage> {
    let header = Header::parse(&mut boot_img)
        .map_err(|err| invalid_data(format!("parse Android boot image: {err}")))?;

    if header.kernel_size() == 0 {
        return Err(invalid_data("boot image has no kernel"));
    }

    let cmdline = android_cmdline(header.cmdline())?;
    tracing::info!(
        header_version = header.header_version(),
        cmdline = %cmdline,
        "parsed Android boot image"
    );
    let kernel = extract_section(
        &mut boot_img,
        "boot-kernel",
        header.kernel_position(),
        header.kernel_size(),
    )?;
    let initrd = if header.ramdisk_size() == 0 {
        None
    } else {
        Some(extract_section(
            &mut boot_img,
            "boot-ramdisk",
            header.ramdisk_position(),
            header.ramdisk_size(),
        )?)
    };
    let dtb = extract_dtb(&mut boot_img, &header)?;

    KexecImage::new(kernel, initrd, dtb, &cmdline)
}

fn extract_dtb(boot_img: &mut fs::File, header: &Header) -> io::Result<Option<fs::File>> {
    if let Some((position, size)) = boot_dtb_section(header) {
        if size == 0 {
            return Ok(None);
        }

        tracing::info!(position, bytes = size, "extracting boot image DTB section");
        return extract_section(boot_img, "boot-dtb", position, size).map(Some);
    }

    extract_vendor_dt_dtb(boot_img, header)
}

fn boot_dtb_section(header: &Header) -> Option<(usize, u32)> {
    match header {
        Header::V0(header) => match header.versioned {
            HeaderV0Versioned::V2 { dtb_size, .. } => {
                header.dtb_position().map(|position| (position, dtb_size))
            }
            HeaderV0Versioned::V0 | HeaderV0Versioned::V1 { .. } => None,
        },
        Header::V0VendorDt(_) => None,
        Header::V3(_) => None,
    }
}

fn extract_vendor_dt_dtb(boot_img: &mut fs::File, header: &Header) -> io::Result<Option<fs::File>> {
    let Some(position) = header.vendor_dt_position() else {
        return Ok(None);
    };
    let size = header
        .vendor_dt_size()
        .ok_or_else(|| invalid_data("boot image vendor-dt section has no size"))?;
    let size_usize = usize::try_from(size)
        .map_err(|_| invalid_data("boot image vendor-dt size does not fit usize"))?;
    let mut vendor_dt = vec![0; size_usize];

    tracing::info!(
        position,
        bytes = size,
        "extracting boot image vendor-dt section"
    );
    boot_img.seek(SeekFrom::Start(u64::try_from(position).map_err(|_| {
        invalid_data("boot image vendor-dt position does not fit u64")
    })?))?;
    boot_img.read_exact(&mut vendor_dt).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("boot image vendor-dt section is truncated: {err}"),
        )
    })?;

    let (kind, entries, version, dtb_range) = if vendor_dt.starts_with(b"QCDT") {
        let qcdt = Qcdt::parse(&vendor_dt)
            .map_err(|err| invalid_data(format!("parse boot image QCDT vendor-dt: {err}")))?;
        let dtb_range = qcdt.single_entry_fdt_range().map_err(|err| {
            invalid_data(format!("select boot image QCDT vendor-dt entry: {err}"))
        })?;
        ("QCDT", qcdt.num_entries(), qcdt.version(), dtb_range)
    } else if vendor_dt.starts_with(b"DTBH") {
        let dtbh = Dtbh::parse(&vendor_dt)
            .map_err(|err| invalid_data(format!("parse boot image DTBH vendor-dt: {err}")))?;
        let dtb_range = dtbh.single_entry_fdt_range().map_err(|err| {
            invalid_data(format!("select boot image DTBH vendor-dt entry: {err}"))
        })?;
        ("DTBH", dtbh.num_entries(), dtbh.version(), dtb_range)
    } else {
        let magic = vendor_dt.get(..4).unwrap_or(&vendor_dt);
        return Err(invalid_data(format!(
            "unsupported boot image vendor-dt magic: {magic:?}"
        )));
    };
    tracing::info!(
        kind = kind,
        entries = entries,
        version = version,
        bytes = dtb_range.len(),
        "selected boot image vendor-dt DTB"
    );

    payload_from_slice("boot-dtb", &vendor_dt[dtb_range]).map(Some)
}

fn extract_section(
    boot_img: &mut fs::File,
    name: &str,
    position: usize,
    size: u32,
) -> io::Result<fs::File> {
    let position = u64::try_from(position)
        .map_err(|_| invalid_data(format!("{name} position does not fit u64")))?;
    let size = u64::from(size);
    let mut payload = kexec::create_payload_memfd(name)?;
    payload.set_len(size)?;

    boot_img.seek(SeekFrom::Start(position))?;
    let copied = io::copy(&mut boot_img.take(size), &mut payload)?;
    if copied != size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("boot image {name} section is truncated"),
        ));
    }

    payload.seek(SeekFrom::Start(0))?;
    kexec::reopen_payload_readonly(payload)
}

fn payload_from_slice(name: &str, data: &[u8]) -> io::Result<fs::File> {
    let mut payload = kexec::create_payload_memfd(name)?;
    payload.write_all(data)?;
    payload.seek(SeekFrom::Start(0))?;
    kexec::reopen_payload_readonly(payload)
}

fn android_cmdline(bytes: &[u8]) -> io::Result<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == b'\0')
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end])
        .map(|cmdline| cmdline.trim_end().to_string())
        .map_err(|err| invalid_data(format!("boot image cmdline is not UTF-8: {err}")))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kexec::tests::{assert_payloads, dtb_fixture, gzip, raw_arm64_image};

    // Encode the on-disk Android header independently of the parser. The v0
    // layout matches Ferrari: 2048-byte pages and a one-byte ramdisk.
    fn android_image(kernel: &[u8], explicit_dtb: Option<&[u8]>) -> Vec<u8> {
        let mut image = vec![0u8; 2048];
        image[..8].copy_from_slice(b"ANDROID!");
        image[8..12].copy_from_slice(&(kernel.len() as u32).to_le_bytes());
        image[12..16].copy_from_slice(&0x80080000u32.to_le_bytes());
        image[16..20].copy_from_slice(&1u32.to_le_bytes());
        image[36..40].copy_from_slice(&2048u32.to_le_bytes());
        image[64..72].copy_from_slice(b"panic=1\0");
        if let Some(dtb) = explicit_dtb {
            image[40..44].copy_from_slice(&2u32.to_le_bytes());
            image[1644..1648].copy_from_slice(&1660u32.to_le_bytes());
            image[1648..1652].copy_from_slice(&(dtb.len() as u32).to_le_bytes());
        }
        image.extend_from_slice(kernel);
        image.resize(image.len().next_multiple_of(2048), 0);
        image.push(0); // ramdisk_size=1
        image.resize(image.len().next_multiple_of(2048), 0);
        if let Some(dtb) = explicit_dtb {
            image.extend_from_slice(dtb);
            image.resize(image.len().next_multiple_of(2048), 0);
        }
        image
    }

    #[test]
    fn android_v0_gzip_with_appended_dtb_uses_the_packaged_tree() {
        let raw = raw_arm64_image();
        let dtb = dtb_fixture();
        let section = [gzip(&raw), dtb.clone()].concat();
        let boot = android_image(&section, None);
        let image = prepare_boot_image(payload_from_slice("boot", &boot).unwrap()).unwrap();
        assert_payloads(&image, &raw, Some(&dtb));
    }

    #[test]
    fn android_v0_preserves_the_whole_preboot_envelope_and_inner_bss() {
        let mut envelope = raw_arm64_image();
        envelope[16..24].copy_from_slice(&0x10000u64.to_le_bytes()); // shim runtime
        envelope.resize(0x180000, 0); // inner Image at physical 0x80200000
        let mut inner = raw_arm64_image();
        inner[16..24].copy_from_slice(&0x200000u64.to_le_bytes()); // includes BSS
        envelope.extend_from_slice(&inner);
        envelope.resize(0x380000, 0);
        let dtb = dtb_fixture();
        let section = [gzip(&envelope), dtb.clone()].concat();
        let boot = android_image(&section, None);
        let image = prepare_boot_image(payload_from_slice("preboot", &boot).unwrap()).unwrap();
        // In particular, do not truncate to the outer image_size (shim only).
        assert_payloads(&image, &envelope, Some(&dtb));
    }

    #[test]
    fn android_v2_explicit_dtb_wins_over_the_appended_tree() {
        let raw = raw_arm64_image();
        let appended = dtb_fixture();
        let mut explicit = appended.clone();
        explicit[28..32].copy_from_slice(&0x100u32.to_be_bytes());
        let section = [gzip(&raw), appended].concat();
        let boot = android_image(&section, Some(&explicit));
        let image = prepare_boot_image(payload_from_slice("v2", &boot).unwrap()).unwrap();
        assert_payloads(&image, &raw, Some(&explicit));
    }

    #[test]
    fn android_gzip_without_dtb_preserves_the_live_tree_fallback() {
        let raw = raw_arm64_image();
        let boot = android_image(&gzip(&raw), None);
        let image = prepare_boot_image(payload_from_slice("plain", &boot).unwrap()).unwrap();
        assert_payloads(&image, &raw, None);
    }

    #[test]
    fn truncated_android_kernel_section_is_rejected_before_preparation() {
        let section = [gzip(&raw_arm64_image()), dtb_fixture()].concat();
        let mut boot = android_image(&section, None);
        boot.truncate(2048 + section.len() - 1);
        assert!(prepare_boot_image(payload_from_slice("short", &boot).unwrap()).is_err());
    }

    /// Optional real-artifact check. Expected files should be independently
    /// extracted (e.g. with Python's zlib) and are never loaded with kexec.
    #[test]
    #[ignore = "requires POCKETBOOT_TEST_BOOT_IMAGE, POCKETBOOT_TEST_KERNEL and POCKETBOOT_TEST_DTB"]
    fn inspect_supplied_android_image() {
        let path = |name| std::env::var_os(name).expect(name);
        let boot = fs::File::open(path("POCKETBOOT_TEST_BOOT_IMAGE")).unwrap();
        let kernel = fs::read(path("POCKETBOOT_TEST_KERNEL")).unwrap();
        let dtb = fs::read(path("POCKETBOOT_TEST_DTB")).unwrap();
        let image = prepare_boot_image(boot).unwrap();
        assert_payloads(&image, &kernel, Some(&dtb));
        println!(
            "validated Android image: kernel={} bytes, DTB={} bytes; no kexec performed",
            kernel.len(),
            dtb.len()
        );
    }
}
