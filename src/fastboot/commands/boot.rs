use std::{
    fs,
    io::{self, Read, Seek, SeekFrom},
};

use abootimg_oxide::Header;

use crate::{
    fastboot::{CommandContext, PostResponseAction},
    kexec::{self, KexecImage},
};

const KEXEC_LOADED: &str = "/sys/kernel/kexec_loaded";

pub(super) fn handle_boot(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<Option<PostResponseAction>> {
    let image = prepare_staged_boot_image(context)?;
    image.load()?;
    context.okay_then(b"booting", kexec::exec_loaded_image)
}

pub(super) fn handle_kexec_load(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<Option<PostResponseAction>> {
    let image = prepare_staged_boot_image(context)?;
    image.load()?;
    context.okay(b"loaded")?;
    Ok(None)
}

pub(super) fn handle_kexec_status(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<Option<PostResponseAction>> {
    let status = fs::read_to_string(KEXEC_LOADED)?;
    context.okay(status.trim().as_bytes())?;
    Ok(None)
}

fn prepare_staged_boot_image(context: &CommandContext<'_>) -> io::Result<KexecImage> {
    let mut boot_img = context.staged_file()?;
    let header = Header::parse(&mut boot_img)
        .map_err(|err| invalid_data(format!("parse Android boot image: {err}")))?;

    if header.kernel_size() == 0 {
        return Err(invalid_data("boot image has no kernel"));
    }

    let cmdline = android_cmdline(header.cmdline())?;
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

    KexecImage::new(kernel, initrd, &cmdline)
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
