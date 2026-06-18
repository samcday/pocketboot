use std::{
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
};

use abootimg_oxide::{Dtbh, Header, HeaderV0Versioned, Qcdt};
use flate2::read::MultiGzDecoder;

use crate::{
    fastboot::{CommandContext, CommandResult},
    kexec::{self, KexecImage},
};

const KEXEC_LOADED: &str = "/sys/kernel/kexec_loaded";
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

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
    let mut boot_img = context.staged_file()?;
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
    let kernel = prepare_kernel_payload(kernel)?;
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

fn prepare_kernel_payload(mut kernel: fs::File) -> io::Result<fs::File> {
    if !is_gzip_payload(&mut kernel)? {
        return Ok(kernel);
    }

    tracing::info!("decompressing gzip kernel image");
    let mut decoder = MultiGzDecoder::new(kernel);
    let mut payload = kexec::create_payload_memfd("boot-kernel-uncompressed")?;
    let copied = io::copy(&mut decoder, &mut payload).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("decompress gzip boot kernel section: {err}"),
        )
    })?;
    tracing::info!(bytes = copied, "decompressed gzip kernel image");

    payload.seek(SeekFrom::Start(0))?;
    kexec::reopen_payload_readonly(payload)
}

fn is_gzip_payload(payload: &mut fs::File) -> io::Result<bool> {
    let mut magic = [0; GZIP_MAGIC.len()];
    let read = payload.read(&mut magic)?;
    payload.seek(SeekFrom::Start(0))?;
    Ok(read == magic.len() && magic == GZIP_MAGIC)
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
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    #[test]
    fn raw_kernel_payload_passes_through() {
        let raw = b"raw arm64 Image payload";
        let payload = payload_file("test-raw-kernel", raw);

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
    fn corrupt_gzip_kernel_payload_fails() {
        let payload = payload_file("test-corrupt-gzip-kernel", b"\x1f\x8bnot really gzip");

        let err = prepare_kernel_payload(payload).unwrap_err();

        assert!(
            err.to_string()
                .contains("decompress gzip boot kernel section"),
            "unexpected error: {err}"
        );
    }

    fn payload_file(name: &str, data: &[u8]) -> fs::File {
        let mut file = kexec::create_payload_memfd(name).unwrap();
        file.write_all(data).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        kexec::reopen_payload_readonly(file).unwrap()
    }

    fn read_file(file: &fs::File) -> Vec<u8> {
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
}
