use std::io;

const DOS_MAGIC: &[u8; 2] = b"MZ";
const PE_SIGNATURE: &[u8; 4] = b"PE\0\0";
const DOS_E_LFANEW_OFFSET: usize = 0x3c;
const COFF_HEADER_SIZE: usize = 20;
const SECTION_HEADER_SIZE: usize = 40;
const PE32_PLUS_MAGIC: u16 = 0x20b;

pub(crate) const IMAGE_FILE_MACHINE_ARM64: u16 = 0xaa64;

#[derive(Debug)]
pub(crate) struct Image<'a> {
    data: &'a [u8],
    machine: u16,
    sections: Vec<Section<'a>>,
}

impl<'a> Image<'a> {
    pub(crate) fn parse(data: &'a [u8]) -> io::Result<Option<Self>> {
        if data.get(..DOS_MAGIC.len()) != Some(DOS_MAGIC) {
            return Ok(None);
        }

        let pe_offset = read_u32(data, DOS_E_LFANEW_OFFSET, "DOS e_lfanew")? as usize;
        let signature = checked_slice(data, pe_offset, PE_SIGNATURE.len(), "PE signature")?;
        if signature != PE_SIGNATURE {
            return invalid_data("MZ image does not contain a PE/COFF signature");
        }

        let coff_offset = checked_add(pe_offset, PE_SIGNATURE.len(), "COFF header offset")?;
        checked_slice(data, coff_offset, COFF_HEADER_SIZE, "COFF header")?;
        let machine = read_u16(data, coff_offset, "COFF machine")?;
        let section_count = read_u16(data, coff_offset + 2, "COFF section count")? as usize;
        let optional_header_size =
            read_u16(data, coff_offset + 16, "COFF optional header size")? as usize;

        let optional_header_offset =
            checked_add(coff_offset, COFF_HEADER_SIZE, "optional header offset")?;
        checked_slice(
            data,
            optional_header_offset,
            optional_header_size,
            "optional header",
        )?;
        let optional_magic = read_u16(data, optional_header_offset, "optional header magic")?;
        if optional_magic != PE32_PLUS_MAGIC {
            return invalid_data(format!(
                "PE/COFF optional header is not PE32+: 0x{optional_magic:04x}"
            ));
        }

        let section_table_offset = checked_add(
            optional_header_offset,
            optional_header_size,
            "section table offset",
        )?;
        let section_table_size =
            checked_mul(section_count, SECTION_HEADER_SIZE, "section table size")?;
        checked_slice(
            data,
            section_table_offset,
            section_table_size,
            "section table",
        )?;

        let mut sections = Vec::with_capacity(section_count);
        for index in 0..section_count {
            let offset = section_table_offset + index * SECTION_HEADER_SIZE;
            let raw = checked_slice(data, offset, SECTION_HEADER_SIZE, "section header")?;
            let mut name = [0; 8];
            name.copy_from_slice(&raw[..8]);
            let size = u32::from_le_bytes(raw[16..20].try_into().unwrap()) as usize;
            let data_offset = u32::from_le_bytes(raw[20..24].try_into().unwrap()) as usize;
            let section_data = checked_slice(data, data_offset, size, "section data")?;

            sections.push(Section {
                name,
                data: section_data,
                raw_offset: data_offset,
            });
        }

        Ok(Some(Self {
            data,
            machine,
            sections,
        }))
    }

    pub(crate) fn data(&self) -> &'a [u8] {
        self.data
    }

    pub(crate) fn machine(&self) -> u16 {
        self.machine
    }

    pub(crate) fn sections(&self) -> &[Section<'a>] {
        &self.sections
    }
}

#[derive(Debug)]
pub(crate) struct Section<'a> {
    name: [u8; 8],
    data: &'a [u8],
    raw_offset: usize,
}

impl<'a> Section<'a> {
    pub(crate) fn name(&self) -> String {
        let len = self
            .name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.name.len());
        String::from_utf8_lossy(&self.name[..len]).into_owned()
    }

    pub(crate) fn data(&self) -> &'a [u8] {
        self.data
    }

    pub(crate) fn raw_offset(&self) -> usize {
        self.raw_offset
    }
}

fn read_u16(data: &[u8], offset: usize, description: &str) -> io::Result<u16> {
    let bytes = checked_slice(data, offset, 2, description)?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_u32(data: &[u8], offset: usize, description: &str) -> io::Result<u32> {
    let bytes = checked_slice(data, offset, 4, description)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn checked_slice<'a>(
    data: &'a [u8],
    offset: usize,
    size: usize,
    description: &str,
) -> io::Result<&'a [u8]> {
    let end = checked_add(offset, size, description)?;
    data.get(offset..end).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} exceeds PE image"),
        )
    })
}

fn checked_add(left: usize, right: usize, description: &str) -> io::Result<usize> {
    left.checked_add(right).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} offset overflows"),
        )
    })
}

fn checked_mul(left: usize, right: usize, description: &str) -> io::Result<usize> {
    left.checked_mul(right).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} overflows"),
        )
    })
}

fn invalid_data<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_non_pe_images() {
        assert!(Image::parse(b"not PE").unwrap().is_none());
    }

    #[test]
    fn parses_arm64_pe_sections() {
        let image = test_pe(b"payload");
        let pe = Image::parse(&image).unwrap().unwrap();

        assert_eq!(pe.machine(), IMAGE_FILE_MACHINE_ARM64);
        assert_eq!(pe.sections().len(), 1);
        assert_eq!(pe.sections()[0].name(), ".text");
        assert_eq!(pe.sections()[0].data(), b"payload");
    }

    #[test]
    fn rejects_out_of_bounds_sections() {
        let mut image = test_pe(b"payload");
        let section_header = section_header_offset();
        image[section_header + 16..section_header + 20].copy_from_slice(&4096u32.to_le_bytes());

        let err = Image::parse(&image).unwrap_err();

        assert!(err.to_string().contains("section data exceeds"));
    }

    pub(crate) fn test_pe(section_data: &[u8]) -> Vec<u8> {
        let pe_offset = 0x80usize;
        let optional_header_size = 0xf0usize;
        let section_header_offset =
            pe_offset + PE_SIGNATURE.len() + COFF_HEADER_SIZE + optional_header_size;
        let raw_offset = section_header_offset + SECTION_HEADER_SIZE;
        let mut image = vec![0; raw_offset + section_data.len()];

        image[..DOS_MAGIC.len()].copy_from_slice(DOS_MAGIC);
        image[DOS_E_LFANEW_OFFSET..DOS_E_LFANEW_OFFSET + 4]
            .copy_from_slice(&(pe_offset as u32).to_le_bytes());
        image[pe_offset..pe_offset + PE_SIGNATURE.len()].copy_from_slice(PE_SIGNATURE);

        let coff_offset = pe_offset + PE_SIGNATURE.len();
        image[coff_offset..coff_offset + 2]
            .copy_from_slice(&IMAGE_FILE_MACHINE_ARM64.to_le_bytes());
        image[coff_offset + 2..coff_offset + 4].copy_from_slice(&1u16.to_le_bytes());
        image[coff_offset + 16..coff_offset + 18]
            .copy_from_slice(&(optional_header_size as u16).to_le_bytes());

        let optional_header_offset = coff_offset + COFF_HEADER_SIZE;
        image[optional_header_offset..optional_header_offset + 2]
            .copy_from_slice(&PE32_PLUS_MAGIC.to_le_bytes());

        image[section_header_offset..section_header_offset + 8].copy_from_slice(b".text\0\0\0");
        image[section_header_offset + 16..section_header_offset + 20]
            .copy_from_slice(&(section_data.len() as u32).to_le_bytes());
        image[section_header_offset + 20..section_header_offset + 24]
            .copy_from_slice(&(raw_offset as u32).to_le_bytes());
        image[raw_offset..raw_offset + section_data.len()].copy_from_slice(section_data);

        image
    }

    fn section_header_offset() -> usize {
        0x80 + PE_SIGNATURE.len() + COFF_HEADER_SIZE + 0xf0
    }
}
