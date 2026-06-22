use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use abootimg_oxide::{HeaderV0, HeaderV0Versioned, OsVersionPatch};
use sha1::{Digest, Sha1};

use crate::Result;

use super::preboot;
use super::{
    FeatureSet, KernelDevice,
    config::{self, BootImgConfig, DtbhConfig, PrebootConfig, QcdtConfig},
    ensure_file,
    kernel::{kernel_arch, kernel_dtb_stem},
    target_dir, workspace_root,
};

#[cfg(test)]
use super::config::DEFAULT_BOOTIMG_KERNEL_IMAGE;

const ANDROID_BOOT_MAGIC: &[u8; 8] = b"ANDROID!";
const SEANDROID_ENFORCE: &[u8] = b"SEANDROIDENFORCE";
const DTBH_MAGIC: &[u8; 4] = b"DTBH";
const DTBH_VERSION: u32 = 2;
const DTBH_RECORD_SPACE: u32 = 0x20;
const ARM64_IMAGE_MIN_SIZE: usize = 64;
const ARM64_IMAGE_SIZE_OFFSET: usize = 16;
const ARM64_IMAGE_MAGIC_OFFSET: usize = 56;
const ARM64_IMAGE_MAGIC: &[u8; 4] = b"ARM\x64";

#[derive(clap::Args, Debug)]
pub(crate) struct BootImgArgs {
    #[arg(value_name = "VENDOR/DEVICE")]
    device: KernelDevice,
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
}

pub(crate) fn run(args: BootImgArgs) -> Result<()> {
    bootimg(args)
}

fn bootimg(args: BootImgArgs) -> Result<()> {
    let workspace_root = workspace_root()?;
    let target_dir = target_dir(&workspace_root);

    let device_config = config::load_device_config(&workspace_root, &args.device)?;
    let arch = kernel_arch(&device_config.kernel)?;
    let dtb_stem = kernel_dtb_stem(&device_config.kernel, &args.device)?;
    let out_dir = target_dir
        .join("kernel")
        .join(&args.device.vendor)
        .join(&args.device.stem);
    let dtb = out_dir
        .join(format!("arch/{arch}/boot/dts"))
        .join(&args.device.vendor)
        .join(format!("{dtb_stem}.dtb"));
    let output = args.output.unwrap_or_else(|| out_dir.join("boot.img"));
    let config = device_config.bootimg.as_ref().ok_or_else(|| {
        format!(
            "missing [bootimg] table in {}",
            device_config.device_path.display()
        )
    })?;
    let payload_image = bootimg_kernel_image(config, &device_config.device_path, &out_dir, &arch)?;
    ensure_file(&payload_image, "kernel image")?;
    ensure_file(&dtb, "device tree blob")?;
    let image = if let Some(preboot) = &config.preboot {
        stage_preboot_kernel(
            &workspace_root,
            &args.device,
            preboot,
            &payload_image,
            &out_dir.join("pocketpreboot-kernel.img"),
        )?
    } else {
        payload_image.clone()
    };
    write_bootimg(config, &device_config.device_path, &image, &dtb, &output)?;

    println!("wrote {}", output.display());
    println!("image {}", image.display());
    if config.preboot.is_some() {
        println!("payload {}", payload_image.display());
    }
    println!("dtb {}", dtb.display());
    println!("config {}", device_config.device_path.display());
    Ok(())
}

fn bootimg_kernel_image(
    config: &BootImgConfig,
    config_path: &Path,
    out_dir: &Path,
    arch: &str,
) -> Result<PathBuf> {
    let mut components = Path::new(&config.kernel_image).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None) => Ok(out_dir
            .join(format!("arch/{arch}/boot"))
            .join(&config.kernel_image)),
        _ => Err(format!(
            "{}: kernel_image must be a file name under arch/{arch}/boot",
            config_path.display()
        )),
    }
}

fn stage_preboot_kernel(
    workspace_root: &Path,
    device: &KernelDevice,
    config: &PrebootConfig,
    kernel: &Path,
    output: &Path,
) -> Result<PathBuf> {
    let preboot = preboot::build_device_preboot(
        workspace_root,
        device,
        preboot::DEFAULT_TARGET,
        None,
        FeatureSet::default(),
    )?;
    let mut staged = fs::read(&preboot.binary)
        .map_err(|err| format!("read {}: {err}", preboot.binary.display()))?;
    let preboot_image_size = arm64_image_size(&staged, "pocketpreboot image")?;

    let kernel = fs::read(kernel).map_err(|err| format!("read {}: {err}", kernel.display()))?;
    arm64_image_size(&kernel, "preboot payload kernel")?;

    let payload_offset =
        preboot_payload_offset(config.load_addr, preboot_image_size, config.payload_align)?;
    if payload_offset < staged.len() {
        return Err(format!(
            "preboot payload offset 0x{payload_offset:x} is inside pocketpreboot file size 0x{:x}",
            staged.len()
        ));
    }
    staged.resize(payload_offset, 0);
    staged.extend_from_slice(&kernel);

    fs::write(output, &staged).map_err(|err| format!("write {}: {err}", output.display()))?;
    println!("preboot {}", preboot.binary.display());
    println!("preboot-features {}", preboot.features);
    println!("preboot-payload-offset 0x{payload_offset:x}");
    Ok(output.to_path_buf())
}

fn preboot_payload_offset(load_addr: u64, image_size: u64, payload_align: u64) -> Result<usize> {
    let end = load_addr
        .checked_add(image_size)
        .ok_or_else(|| "preboot image end address overflows u64".to_string())?;
    let payload_addr = align_up_u64(end, payload_align)?;
    let offset = payload_addr
        .checked_sub(load_addr)
        .ok_or_else(|| "preboot payload offset underflows u64".to_string())?;
    usize::try_from(offset).map_err(|_| format!("preboot payload offset is too large: {offset}"))
}

fn align_up_u64(value: u64, alignment: u64) -> Result<u64> {
    if alignment == 0 || !alignment.is_power_of_two() {
        return Err(format!(
            "preboot payload_align must be a non-zero power of two: 0x{alignment:x}"
        ));
    }

    let mask = alignment - 1;
    value
        .checked_add(mask)
        .map(|value| value & !mask)
        .ok_or_else(|| "aligned address overflows u64".to_string())
}

fn arm64_image_size(image: &[u8], description: &str) -> Result<u64> {
    if image.len() < ARM64_IMAGE_MIN_SIZE {
        return Err(format!(
            "{description} is smaller than an arm64 Image header: {} < {ARM64_IMAGE_MIN_SIZE}",
            image.len()
        ));
    }
    if &image[ARM64_IMAGE_MAGIC_OFFSET..ARM64_IMAGE_MAGIC_OFFSET + ARM64_IMAGE_MAGIC.len()]
        != ARM64_IMAGE_MAGIC
    {
        return Err(format!("{description} is not a raw arm64 Image"));
    }

    let image_size = u64::from_le_bytes(
        image[ARM64_IMAGE_SIZE_OFFSET..ARM64_IMAGE_SIZE_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    if image_size == 0 {
        return Err(format!(
            "{description} arm64 Image header has zero image_size"
        ));
    }
    Ok(image_size)
}

fn write_bootimg(
    config: &BootImgConfig,
    config_path: &Path,
    image: &Path,
    dtb: &Path,
    output: &Path,
) -> Result<()> {
    if config.page_size == 0 || !config.page_size.is_power_of_two() {
        return Err("boot image page_size must be a non-zero power of two".to_string());
    }
    if config.qcdt.is_some() && config.dtbh.is_some() {
        return Err(format!(
            "{}: qcdt and dtbh are mutually exclusive",
            config_path.display()
        ));
    }
    if config.append_dtb && (config.qcdt.is_some() || config.dtbh.is_some()) {
        return Err(format!(
            "{}: append_dtb cannot be combined with qcdt or dtbh",
            config_path.display()
        ));
    }

    match config.header_version {
        0 => write_bootimg_v0(config, config_path, image, dtb, output),
        2 => write_bootimg_v2(config, config_path, image, dtb, output),
        version => Err(format!(
            "boot image header version {version} is not supported yet; expected 0 or 2"
        )),
    }
}

fn write_bootimg_v2(
    config: &BootImgConfig,
    config_path: &Path,
    image: &Path,
    dtb: &Path,
    output: &Path,
) -> Result<()> {
    if config.qcdt.is_some() {
        return Err(format!(
            "{}: qcdt requires boot image header_version = 0",
            config_path.display()
        ));
    }
    if config.dtbh.is_some() {
        return Err(format!(
            "{}: dtbh requires boot image header_version = 0",
            config_path.display()
        ));
    }
    if config.ramdisk_size != 0 {
        return Err(format!(
            "{}: ramdisk_size is only supported for header_version = 0",
            config_path.display()
        ));
    }
    if config.append_seandroid_enforce {
        return Err(format!(
            "{}: append_seandroid_enforce is only supported for header_version = 0",
            config_path.display()
        ));
    }
    if config.append_dtb {
        return Err(format!(
            "{}: append_dtb is only supported for header_version = 0",
            config_path.display()
        ));
    }

    let kernel_size = file_size_u32(image, "kernel image")?;
    let dtb_size = file_size_u32(dtb, "device tree blob")?;
    let hash_digest = bootimg_hash_digest(image, dtb)?;
    let kernel_addr = boot_addr_u32(config.base, config.kernel_offset, "kernel_addr")?;
    let ramdisk_addr = boot_addr_u32(config.base, config.ramdisk_offset, "ramdisk_addr")?;
    let second_bootloader_addr =
        boot_addr_u32(config.base, config.second_offset, "second_bootloader_addr")?;
    let tags_addr = boot_addr_u32(config.base, config.tags_offset, "tags_addr")?;
    let dtb_addr = config
        .base
        .checked_add(config.dtb_offset)
        .ok_or_else(|| "dtb_addr overflows u64".to_string())?;

    let header = HeaderV0 {
        kernel_size,
        kernel_addr,
        ramdisk_size: 0,
        ramdisk_addr,
        second_bootloader_size: 0,
        second_bootloader_addr,
        tags_addr,
        page_size: config.page_size,
        osversionpatch: OsVersionPatch(0),
        board_name: fixed_bytes(&config.board, "board", config_path)?,
        hash_digest,
        cmdline: Box::new(fixed_bytes(&config.cmdline, "cmdline", config_path)?),
        versioned: HeaderV0Versioned::V2 {
            recovery_dtbo_size: 0,
            recovery_dtbo_addr: 0,
            dtb_size,
            dtb_addr,
        },
    };

    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }

    let mut output_file =
        File::create(output).map_err(|err| format!("create {}: {err}", output.display()))?;
    let mut kernel_file =
        File::open(image).map_err(|err| format!("open {}: {err}", image.display()))?;
    let mut dtb_file = File::open(dtb).map_err(|err| format!("open {}: {err}", dtb.display()))?;
    header
        .full_write(
            &mut output_file,
            Some(&mut kernel_file),
            None::<&mut File>,
            None::<&mut File>,
            None::<&mut File>,
            Some(&mut dtb_file),
        )
        .map_err(|err| format!("write {}: {err}", output.display()))?;
    output_file
        .set_len(header.boot_image_size() as u64)
        .map_err(|err| format!("truncate {}: {err}", output.display()))?;
    output_file
        .flush()
        .map_err(|err| format!("flush {}: {err}", output.display()))?;
    Ok(())
}

fn write_bootimg_v0(
    config: &BootImgConfig,
    config_path: &Path,
    image: &Path,
    dtb: &Path,
    output: &Path,
) -> Result<()> {
    let mut kernel = fs::read(image).map_err(|err| format!("read {}: {err}", image.display()))?;
    let dtb = fs::read(dtb).map_err(|err| format!("read {}: {err}", dtb.display()))?;
    let ramdisk = vec![0; config.ramdisk_size as usize];
    if config.append_dtb {
        kernel.extend_from_slice(&dtb);
    }
    let vendor_dt = match (&config.qcdt, &config.dtbh) {
        (Some(qcdt), None) => build_qcdt(qcdt, config.page_size, &dtb, config_path)?,
        (None, Some(dtbh)) => build_dtbh(dtbh, config.page_size, &dtb, config_path)?,
        (None, None) => Vec::new(),
        (Some(_), Some(_)) => {
            return Err(format!(
                "{}: qcdt and dtbh are mutually exclusive",
                config_path.display()
            ));
        }
    };

    let kernel_size = u32_len(kernel.len(), "kernel image", image)?;
    let ramdisk_size = u32_len(ramdisk.len(), "ramdisk", config_path)?;
    let vendor_dt_size = u32_len(vendor_dt.len(), "vendor DT", config_path)?;
    let kernel_addr = boot_addr_u32(config.base, config.kernel_offset, "kernel_addr")?;
    let ramdisk_addr = boot_addr_u32(config.base, config.ramdisk_offset, "ramdisk_addr")?;
    let second_bootloader_addr =
        boot_addr_u32(config.base, config.second_offset, "second_bootloader_addr")?;
    let tags_addr = boot_addr_u32(config.base, config.tags_offset, "tags_addr")?;
    let page_size = usize::try_from(config.page_size)
        .map_err(|_| format!("{}: page_size does not fit usize", config_path.display()))?;

    let mut header = Vec::new();
    header.extend_from_slice(ANDROID_BOOT_MAGIC);
    write_u32_le(&mut header, kernel_size);
    write_u32_le(&mut header, kernel_addr);
    write_u32_le(&mut header, ramdisk_size);
    write_u32_le(&mut header, ramdisk_addr);
    write_u32_le(&mut header, 0);
    write_u32_le(&mut header, second_bootloader_addr);
    write_u32_le(&mut header, tags_addr);
    write_u32_le(&mut header, config.page_size);
    write_u32_le(&mut header, vendor_dt_size);
    write_u32_le(&mut header, 0);
    header.extend_from_slice(&fixed_bytes::<16>(&config.board, "board", config_path)?);

    let cmdline = fixed_bytes::<{ 512 + 1024 }>(&config.cmdline, "cmdline", config_path)?;
    header.extend_from_slice(&cmdline[..512]);
    header.extend_from_slice(&bootimg_hash_digest_v0(&kernel, &ramdisk, &vendor_dt));
    header.extend_from_slice(&cmdline[512..]);
    pad_vec_to(&mut header, page_size)?;

    let mut bootimg = header;
    append_padded(&mut bootimg, &kernel, page_size)?;
    append_padded(&mut bootimg, &ramdisk, page_size)?;
    append_padded(&mut bootimg, &vendor_dt, page_size)?;
    if config.append_seandroid_enforce {
        bootimg.extend_from_slice(SEANDROID_ENFORCE);
    }

    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(output, bootimg).map_err(|err| format!("write {}: {err}", output.display()))
}

fn build_qcdt(
    config: &QcdtConfig,
    page_size: u32,
    dtb: &[u8],
    config_path: &Path,
) -> Result<Vec<u8>> {
    if config.entries.is_empty() {
        return Err(format!("{}: qcdt.entries is empty", config_path.display()));
    }

    let page_size = usize::try_from(page_size)
        .map_err(|_| format!("{}: page_size does not fit usize", config_path.display()))?;
    let record_size = 24usize;
    let header_size = 12usize
        .checked_add(
            record_size
                .checked_mul(config.entries.len())
                .ok_or_else(|| "QCDT record table size overflows usize".to_string())?,
        )
        .ok_or_else(|| "QCDT header size overflows usize".to_string())?;
    let dtb_offset = align_up_usize(header_size, page_size)?;
    let dtb_size = align_up_usize(dtb.len(), page_size)?;
    let dtb_offset_u32 = u32_len(dtb_offset, "QCDT DTB offset", config_path)?;
    let dtb_size_u32 = u32_len(dtb_size, "QCDT DTB size", config_path)?;

    let mut qcdt = Vec::with_capacity(
        dtb_offset
            .checked_add(dtb_size)
            .ok_or_else(|| "QCDT size overflows usize".to_string())?,
    );
    qcdt.extend_from_slice(b"QCDT");
    write_u32_le(&mut qcdt, 2);
    write_u32_le(
        &mut qcdt,
        u32_len(config.entries.len(), "QCDT entry count", config_path)?,
    );

    for entry in &config.entries {
        write_u32_le(&mut qcdt, entry.msm_id[0]);
        write_u32_le(&mut qcdt, entry.board_id[0]);
        write_u32_le(&mut qcdt, entry.board_id[1]);
        write_u32_le(&mut qcdt, entry.msm_id[1]);
        write_u32_le(&mut qcdt, dtb_offset_u32);
        write_u32_le(&mut qcdt, dtb_size_u32);
    }

    pad_vec_to(&mut qcdt, page_size)?;
    qcdt.extend_from_slice(dtb);
    pad_vec_to(&mut qcdt, page_size)?;
    Ok(qcdt)
}

fn build_dtbh(
    config: &DtbhConfig,
    page_size: u32,
    dtb: &[u8],
    config_path: &Path,
) -> Result<Vec<u8>> {
    if config.entries.is_empty() {
        return Err(format!("{}: dtbh.entries is empty", config_path.display()));
    }

    let page_size = usize::try_from(page_size)
        .map_err(|_| format!("{}: page_size does not fit usize", config_path.display()))?;
    let record_size = 32usize;
    let header_size = 12usize
        .checked_add(
            record_size
                .checked_mul(config.entries.len())
                .ok_or_else(|| "DTBH record table size overflows usize".to_string())?,
        )
        .and_then(|size| size.checked_add(4))
        .ok_or_else(|| "DTBH header size overflows usize".to_string())?;
    let table_size = align_up_usize(header_size, page_size)?;
    let dtb_size = align_up_usize(dtb.len(), page_size)?;
    let dtb_size_u32 = u32_len(dtb_size, "DTBH DTB size", config_path)?;
    let payload_size = dtb_size
        .checked_mul(config.entries.len())
        .ok_or_else(|| "DTBH payload size overflows usize".to_string())?;

    let mut dtbh = Vec::with_capacity(
        table_size
            .checked_add(payload_size)
            .ok_or_else(|| "DTBH size overflows usize".to_string())?,
    );
    dtbh.extend_from_slice(DTBH_MAGIC);
    write_u32_le(&mut dtbh, DTBH_VERSION);
    write_u32_le(
        &mut dtbh,
        u32_len(config.entries.len(), "DTBH entry count", config_path)?,
    );

    let mut dtb_offset = table_size;
    for entry in &config.entries {
        write_u32_le(&mut dtbh, entry.chip);
        write_u32_le(&mut dtbh, config.platform);
        write_u32_le(&mut dtbh, config.subtype);
        write_u32_le(&mut dtbh, entry.hw_rev);
        write_u32_le(&mut dtbh, entry.hw_rev_end);
        write_u32_le(
            &mut dtbh,
            u32_len(dtb_offset, "DTBH DTB offset", config_path)?,
        );
        write_u32_le(&mut dtbh, dtb_size_u32);
        write_u32_le(&mut dtbh, DTBH_RECORD_SPACE);
        dtb_offset = dtb_offset
            .checked_add(dtb_size)
            .ok_or_else(|| "DTBH DTB offset overflows usize".to_string())?;
    }
    write_u32_le(&mut dtbh, 0);
    pad_vec_to(&mut dtbh, page_size)?;

    for _ in &config.entries {
        dtbh.extend_from_slice(dtb);
        pad_vec_to(&mut dtbh, page_size)?;
    }

    Ok(dtbh)
}

fn file_size_u32(path: &Path, description: &str) -> Result<u32> {
    let size = fs::metadata(path)
        .map_err(|err| format!("stat {}: {err}", path.display()))?
        .len();
    u32::try_from(size).map_err(|_| format!("{description} is too large: {}", path.display()))
}

fn bootimg_hash_digest(image: &Path, dtb: &Path) -> Result<[u8; 32]> {
    let mut kernel_file =
        File::open(image).map_err(|err| format!("open {}: {err}", image.display()))?;
    let mut dtb_file = File::open(dtb).map_err(|err| format!("open {}: {err}", dtb.display()))?;

    HeaderV0::compute_hash_digest::<File, Sha1>(
        Some(&mut kernel_file),
        None::<&mut File>,
        None::<&mut File>,
        None::<&mut File>,
        Some(&mut dtb_file),
    )
    .map_err(|err| format!("compute boot image hash digest: {err}"))
}

fn bootimg_hash_digest_v0(kernel: &[u8], ramdisk: &[u8], vendor_dt: &[u8]) -> [u8; 32] {
    let mut hasher = Sha1::new();
    update_bootimg_hash(&mut hasher, kernel);
    update_bootimg_hash(&mut hasher, ramdisk);
    update_bootimg_hash(&mut hasher, &[]);
    if !vendor_dt.is_empty() {
        update_bootimg_hash(&mut hasher, vendor_dt);
    }

    let digest = hasher.finalize();
    let mut output = [0; 32];
    output[..digest.len()].copy_from_slice(&digest);
    output
}

fn update_bootimg_hash(hasher: &mut Sha1, payload: &[u8]) {
    hasher.update(payload);
    hasher.update((payload.len() as u32).to_le_bytes());
}

fn append_padded(output: &mut Vec<u8>, payload: &[u8], alignment: usize) -> Result<()> {
    if payload.is_empty() {
        return Ok(());
    }

    output.extend_from_slice(payload);
    pad_vec_to(output, alignment)
}

fn pad_vec_to(output: &mut Vec<u8>, alignment: usize) -> Result<()> {
    let padded_len = align_up_usize(output.len(), alignment)?;
    output.resize(padded_len, 0);
    Ok(())
}

fn align_up_usize(value: usize, alignment: usize) -> Result<usize> {
    let pad = (alignment - (value % alignment)) % alignment;
    value
        .checked_add(pad)
        .ok_or_else(|| "aligned size overflows usize".to_string())
}

fn u32_len(value: usize, description: &str, context: &Path) -> Result<u32> {
    u32::try_from(value).map_err(|_| {
        format!(
            "{description} is too large near {}: {value} bytes",
            context.display()
        )
    })
}

fn write_u32_le(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn boot_addr_u32(base: u64, offset: u64, name: &str) -> Result<u32> {
    let value = base
        .checked_add(offset)
        .ok_or_else(|| format!("{name} overflows u64"))?;
    u32::try_from(value).map_err(|_| format!("{name} does not fit in u32: 0x{value:x}"))
}

fn fixed_bytes<const N: usize>(value: &str, field: &str, context: &Path) -> Result<[u8; N]> {
    let bytes = value.as_bytes();
    if bytes.len() > N {
        return Err(format!(
            "{field} is too long for Android boot header near {}: {} > {N} bytes",
            context.display(),
            bytes.len()
        ));
    }

    let mut output = [0; N];
    output[..bytes.len()].copy_from_slice(bytes);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::super::config::{DtbhEntry, QcdtEntry};
    use super::*;

    #[test]
    fn qcdt_records_point_to_page_aligned_dtb() {
        let qcdt = build_qcdt(
            &QcdtConfig {
                entries: vec![QcdtEntry {
                    msm_id: [206, 0],
                    board_id: [0xce08ff01, 1],
                }],
            },
            16,
            b"dtb",
            Path::new("test.toml"),
        )
        .unwrap();

        assert_eq!(&qcdt[..4], b"QCDT");
        assert_eq!(u32_at(&qcdt, 4), 2);
        assert_eq!(u32_at(&qcdt, 8), 1);
        assert_eq!(u32_at(&qcdt, 12), 206);
        assert_eq!(u32_at(&qcdt, 16), 0xce08ff01);
        assert_eq!(u32_at(&qcdt, 20), 1);
        assert_eq!(u32_at(&qcdt, 24), 0);
        assert_eq!(u32_at(&qcdt, 28), 48);
        assert_eq!(u32_at(&qcdt, 32), 16);
        assert_eq!(&qcdt[48..51], b"dtb");
        assert_eq!(qcdt.len(), 64);
    }

    #[test]
    fn dtbh_records_point_to_page_aligned_dtb() {
        let dtbh = build_dtbh(
            &DtbhConfig {
                platform: 0x50a6,
                subtype: 0x217584da,
                entries: vec![DtbhEntry {
                    chip: 7870,
                    hw_rev: 0,
                    hw_rev_end: 255,
                }],
            },
            16,
            b"dtb",
            Path::new("test.toml"),
        )
        .unwrap();

        assert_eq!(&dtbh[..4], b"DTBH");
        assert_eq!(u32_at(&dtbh, 4), 2);
        assert_eq!(u32_at(&dtbh, 8), 1);
        assert_eq!(u32_at(&dtbh, 12), 7870);
        assert_eq!(u32_at(&dtbh, 16), 0x50a6);
        assert_eq!(u32_at(&dtbh, 20), 0x217584da);
        assert_eq!(u32_at(&dtbh, 24), 0);
        assert_eq!(u32_at(&dtbh, 28), 255);
        assert_eq!(u32_at(&dtbh, 32), 48);
        assert_eq!(u32_at(&dtbh, 36), 16);
        assert_eq!(u32_at(&dtbh, 40), 0x20);
        assert_eq!(u32_at(&dtbh, 44), 0);
        assert_eq!(&dtbh[48..51], b"dtb");
        assert_eq!(dtbh.len(), 64);
    }

    #[test]
    fn preboot_payload_offset_aligns_from_load_address() {
        assert_eq!(
            preboot_payload_offset(0x4008_0000, 0x4100, 0x20_0000).unwrap(),
            0x18_0000
        );
    }

    #[test]
    fn arm64_image_size_reads_header() {
        let image = arm64_image(0xa60000);

        assert_eq!(arm64_image_size(&image, "test image").unwrap(), 0xa60000);
    }

    #[test]
    fn samsung_a5u_eur_config_is_legacy_qcdt() {
        let config = device_bootimg_config("qcom/msm8916-samsung-a5u-eur");

        let qcdt = config.qcdt.unwrap();
        assert_eq!(config.header_version, 0);
        assert_eq!(config.page_size, 2048);
        assert_eq!(config.base, 0x80000000);
        assert_eq!(config.kernel_image, "Image");
        assert_eq!(config.ramdisk_size, 1);
        assert!(config.append_seandroid_enforce);
        assert_eq!(qcdt.entries.len(), 1);
        assert_eq!(qcdt.entries[0].msm_id, [206, 0]);
        assert_eq!(qcdt.entries[0].board_id, [0xce08ff01, 1]);
    }

    #[test]
    fn samsung_j7xelte_config_is_legacy_dtbh() {
        let config = device_bootimg_config("exynos/exynos7870-j7xelte");

        assert_eq!(config.header_version, 0);
        assert_eq!(config.page_size, 2048);
        assert_eq!(config.base, 0x10000000);
        assert_eq!(config.kernel_image, "Image");
        let preboot = config.preboot.unwrap();
        assert_eq!(preboot.load_addr, 0x40080000);
        assert_eq!(preboot.payload_align, 0x200000);
        assert!(config.qcdt.is_none());
        let dtbh = config.dtbh.unwrap();
        assert_eq!(dtbh.platform, 0x50a6);
        assert_eq!(dtbh.subtype, 0x217584da);
        assert_eq!(dtbh.entries.len(), 1);
        assert_eq!(dtbh.entries[0].chip, 7870);
        assert_eq!(dtbh.entries[0].hw_rev, 0);
        assert_eq!(dtbh.entries[0].hw_rev_end, 255);
    }

    #[test]
    fn daisy_config_is_legacy_appended_dtb() {
        let config = device_bootimg_config("qcom/msm8953-xiaomi-daisy");

        assert_eq!(config.header_version, 0);
        assert_eq!(config.page_size, 2048);
        assert_eq!(config.base, 0x80000000);
        assert_eq!(config.kernel_image, DEFAULT_BOOTIMG_KERNEL_IMAGE);
        assert!(config.append_dtb);
        assert!(config.qcdt.is_none());
        assert!(config.dtbh.is_none());
    }

    #[test]
    fn appended_dtb_is_written_in_kernel_payload() {
        let temp_dir = unique_test_dir("appended-dtb");
        let kernel_path = temp_dir.join("Image.gz");
        let dtb_path = temp_dir.join("board.dtb");
        let output_path = temp_dir.join("boot.img");
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(&kernel_path, b"kernel").unwrap();
        fs::write(&dtb_path, b"dtb").unwrap();

        let config = BootImgConfig {
            header_version: 0,
            page_size: 2048,
            append_dtb: true,
            ..Default::default()
        };
        write_bootimg_v0(
            &config,
            Path::new("test.toml"),
            &kernel_path,
            &dtb_path,
            &output_path,
        )
        .unwrap();

        let bootimg = fs::read(&output_path).unwrap();
        assert_eq!(u32_at(&bootimg, 8), 9);
        assert_eq!(u32_at(&bootimg, 40), 0);
        assert_eq!(&bootimg[2048..2057], b"kerneldtb");

        fs::remove_dir_all(temp_dir).unwrap();
    }

    fn u32_at(data: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("pocketboot-{name}-{}-{nanos}", std::process::id()))
    }

    fn arm64_image(image_size: u64) -> Vec<u8> {
        let mut image = vec![0; ARM64_IMAGE_MIN_SIZE];
        image[ARM64_IMAGE_SIZE_OFFSET..ARM64_IMAGE_SIZE_OFFSET + 8]
            .copy_from_slice(&image_size.to_le_bytes());
        image[ARM64_IMAGE_MAGIC_OFFSET..ARM64_IMAGE_MAGIC_OFFSET + ARM64_IMAGE_MAGIC.len()]
            .copy_from_slice(ARM64_IMAGE_MAGIC);
        image
    }

    fn device_bootimg_config(device_id: &str) -> BootImgConfig {
        let workspace_root = super::super::workspace_root().unwrap();
        let device = super::super::KernelDevice::parse(device_id).unwrap();
        super::super::config::load_device_config(&workspace_root, &device)
            .unwrap()
            .bootimg
            .unwrap()
    }
}
