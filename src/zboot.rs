use std::io;

const DOS_SIGNATURE: &[u8; 4] = b"MZ\0\0";
const ZBOOT_MAGIC: &[u8; 4] = b"zimg";
const LINUX_PE_MAGIC: &[u8; 4] = &0x8182_23cdu32.to_le_bytes();
const PAYLOAD_OFFSET_OFFSET: usize = 8;
const PAYLOAD_SIZE_OFFSET: usize = 12;
const COMPRESSION_OFFSET: usize = 24;
const COMPRESSION_LEN: usize = 8;
const LINUX_PE_MAGIC_OFFSET: usize = 0x38;
const ZBOOT_MIN_HEADER_SIZE: usize = 0x40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Compression {
    Gzip,
    Zstd,
    Unsupported,
}

#[derive(Debug)]
pub(crate) struct Image<'a> {
    payload: &'a [u8],
    compression: Compression,
    compression_name: String,
    payload_offset: usize,
}

impl<'a> Image<'a> {
    pub(crate) fn parse(data: &'a [u8]) -> io::Result<Option<Self>> {
        if data.get(..DOS_SIGNATURE.len()) != Some(DOS_SIGNATURE)
            || data.get(4..8) != Some(ZBOOT_MAGIC)
        {
            return Ok(None);
        }
        if data.len() < ZBOOT_MIN_HEADER_SIZE {
            return invalid_data("zboot header is truncated");
        }
        if data.get(LINUX_PE_MAGIC_OFFSET..LINUX_PE_MAGIC_OFFSET + LINUX_PE_MAGIC.len())
            != Some(LINUX_PE_MAGIC)
        {
            return invalid_data("zboot header has invalid Linux PE magic");
        }

        let payload_offset =
            read_u32(data, PAYLOAD_OFFSET_OFFSET, "zboot payload offset")? as usize;
        let payload_size = read_u32(data, PAYLOAD_SIZE_OFFSET, "zboot payload size")? as usize;
        let payload_end = payload_offset.checked_add(payload_size).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "zboot payload end overflows")
        })?;
        let payload = data.get(payload_offset..payload_end).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "zboot payload exceeds image")
        })?;
        if payload.is_empty() {
            return invalid_data("zboot payload is empty");
        }

        let compression_name = compression_name(data)?;
        let compression = match compression_name.as_str() {
            "gzip" => Compression::Gzip,
            "zstd" => Compression::Zstd,
            _ => Compression::Unsupported,
        };

        Ok(Some(Self {
            payload,
            compression,
            compression_name,
            payload_offset,
        }))
    }

    pub(crate) fn payload(&self) -> &'a [u8] {
        self.payload
    }

    pub(crate) fn compression(&self) -> Compression {
        self.compression
    }

    pub(crate) fn compression_name(&self) -> &str {
        &self.compression_name
    }

    pub(crate) fn payload_offset(&self) -> usize {
        self.payload_offset
    }
}

fn compression_name(data: &[u8]) -> io::Result<String> {
    let raw = data
        .get(COMPRESSION_OFFSET..COMPRESSION_OFFSET + COMPRESSION_LEN)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "zboot compression field is truncated",
            )
        })?;
    let len = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    if len == 0 {
        return invalid_data("zboot compression field is empty");
    }
    std::str::from_utf8(&raw[..len])
        .map(str::to_string)
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("zboot compression field is not UTF-8: {err}"),
            )
        })
}

fn read_u32(data: &[u8], offset: usize, description: &str) -> io::Result<u32> {
    let bytes = data.get(offset..offset + 4).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} is truncated"),
        )
    })?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn invalid_data<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_non_zboot_images() {
        assert!(Image::parse(b"MZ\0\0notz").unwrap().is_none());
    }

    #[test]
    fn parses_zstd_payload() {
        let payload = b"payload";
        let image = test_zboot("zstd", payload);
        let zboot = Image::parse(&image).unwrap().unwrap();

        assert_eq!(zboot.compression(), Compression::Zstd);
        assert_eq!(zboot.compression_name(), "zstd");
        assert_eq!(zboot.payload(), payload);
        assert_eq!(zboot.payload_offset(), 0x40);
    }

    #[test]
    fn accepts_linux_pe_magic_bytes() {
        let image = test_zboot("zstd", b"payload");

        assert_eq!(&image[LINUX_PE_MAGIC_OFFSET..LINUX_PE_MAGIC_OFFSET + 4], b"\xcd\x23\x82\x81");
        assert!(Image::parse(&image).unwrap().is_some());
    }

    #[test]
    fn rejects_out_of_bounds_payload() {
        let mut image = test_zboot("gzip", b"payload");
        image[PAYLOAD_SIZE_OFFSET..PAYLOAD_SIZE_OFFSET + 4].copy_from_slice(&4096u32.to_le_bytes());

        let err = Image::parse(&image).unwrap_err();

        assert!(err.to_string().contains("payload exceeds"));
    }

    #[test]
    fn preserves_unsupported_compression_name() {
        let image = test_zboot("lz4", b"payload");
        let zboot = Image::parse(&image).unwrap().unwrap();

        assert_eq!(zboot.compression(), Compression::Unsupported);
        assert_eq!(zboot.compression_name(), "lz4");
    }

    pub(crate) fn test_zboot(compression: &str, payload: &[u8]) -> Vec<u8> {
        let payload_offset = ZBOOT_MIN_HEADER_SIZE;
        let mut image = vec![0; payload_offset + payload.len()];
        image[..DOS_SIGNATURE.len()].copy_from_slice(DOS_SIGNATURE);
        image[4..8].copy_from_slice(ZBOOT_MAGIC);
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
        image[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);
        image
    }
}
