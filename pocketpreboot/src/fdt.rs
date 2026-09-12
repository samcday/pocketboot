#![cfg_attr(test, allow(dead_code))]

use core::slice;

pub(crate) const FDT_BEGIN_NODE: u32 = 1;
pub(crate) const FDT_END_NODE: u32 = 2;
pub(crate) const FDT_PROP: u32 = 3;
pub(crate) const FDT_NOP: u32 = 4;
pub(crate) const FDT_END: u32 = 9;

const FDT_MAGIC: u32 = 0xd00dfeed;
const HEADER_SIZE: usize = 40;
const MAX_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    BadMagic,
    BadOffset,
    BadToken,
    BadValue,
    BufferTooSmall,
    NotFound,
    TooDeep,
    Unterminated,
}

pub(crate) type Result<T> = core::result::Result<T, Error>;

#[derive(Clone, Copy)]
pub(crate) struct Header {
    pub(crate) totalsize: usize,
    pub(crate) off_dt_struct: usize,
    pub(crate) off_dt_strings: usize,
    pub(crate) off_mem_rsvmap: usize,
    pub(crate) version: u32,
    pub(crate) last_comp_version: u32,
    pub(crate) boot_cpuid_phys: u32,
    pub(crate) size_dt_strings: usize,
    pub(crate) size_dt_struct: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct Node {
    pub(crate) offset: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct Prop<'a> {
    pub(crate) name: &'a [u8],
    pub(crate) value: &'a [u8],
}

pub(crate) struct Reader<'a> {
    data: &'a [u8],
    header: Header,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Result<Self> {
        let header = Header::parse(data)?;
        Ok(Self {
            data: &data[..header.totalsize],
            header,
        })
    }

    pub(crate) unsafe fn from_ptr(ptr: usize) -> Result<Self> {
        if ptr == 0 {
            return Err(Error::BadOffset);
        }

        let header = unsafe { slice::from_raw_parts(ptr as *const u8, HEADER_SIZE) };
        if read_be32(header, 0)? != FDT_MAGIC {
            return Err(Error::BadMagic);
        }

        let totalsize = read_be32(header, 4)? as usize;
        // The firmware owns the source buffer; reject impossible lengths before
        // forming a slice over it. Preboot has a 512 KiB destination buffer.
        if !(HEADER_SIZE..=512 * 1024).contains(&totalsize) {
            return Err(Error::BadOffset);
        }
        ptr.checked_add(totalsize).ok_or(Error::BadOffset)?;
        let data = unsafe { slice::from_raw_parts(ptr as *const u8, totalsize) };
        Self::new(data)
    }

    pub(crate) fn data_addr(&self) -> usize {
        self.data.as_ptr() as usize
    }

    pub(crate) fn header(&self) -> Header {
        self.header
    }

    pub(crate) fn struct_block(&self) -> Result<&'a [u8]> {
        checked_slice(
            self.data,
            self.header.off_dt_struct,
            self.header.size_dt_struct,
        )
    }

    pub(crate) fn strings(&self) -> Result<&'a [u8]> {
        checked_slice(
            self.data,
            self.header.off_dt_strings,
            self.header.size_dt_strings,
        )
    }

    pub(crate) fn reserve_map(&self) -> Result<&'a [u8]> {
        let mut cursor = self.header.off_mem_rsvmap;
        loop {
            let entry = checked_slice(self.data, cursor, 16)?;
            let address = u64::from_be_bytes(entry[0..8].try_into().unwrap());
            let size = u64::from_be_bytes(entry[8..16].try_into().unwrap());
            cursor = cursor.checked_add(16).ok_or(Error::BadOffset)?;
            if address == 0 && size == 0 {
                return checked_slice(
                    self.data,
                    self.header.off_mem_rsvmap,
                    cursor - self.header.off_mem_rsvmap,
                );
            }
        }
    }

    pub(crate) fn root(&self) -> Result<Node> {
        let structure = self.struct_block()?;
        if read_be32(structure, 0)? != FDT_BEGIN_NODE {
            return Err(Error::BadToken);
        }
        Ok(Node { offset: 0 })
    }

    pub(crate) fn find_path(&self, path: &[u8]) -> Result<Node> {
        if path == b"/" {
            return self.root();
        }
        if !path.starts_with(b"/") {
            return Err(Error::BadValue);
        }

        let structure = self.struct_block()?;
        let target_components = count_path_components(path)?;
        let mut cursor = 0usize;
        let mut depth = 0usize;
        let mut matches = [false; MAX_DEPTH];

        loop {
            let token_start = cursor;
            let token = read_be32(structure, cursor)?;
            cursor = cursor.checked_add(4).ok_or(Error::BadOffset)?;

            match token {
                FDT_BEGIN_NODE => {
                    let name_start = cursor;
                    let name_end = find_nul(structure, name_start)?;
                    let name = &structure[name_start..name_end];
                    cursor = align_usize(name_end.checked_add(1).ok_or(Error::BadOffset)?, 4)?;

                    let new_depth = depth.checked_add(1).ok_or(Error::TooDeep)?;
                    if new_depth > MAX_DEPTH {
                        return Err(Error::TooDeep);
                    }

                    let is_match = if new_depth == 1 {
                        name.is_empty()
                    } else if matches[depth - 1] {
                        component_at(path, new_depth - 2).is_some_and(|component| component == name)
                    } else {
                        false
                    };
                    matches[new_depth - 1] = is_match;
                    depth = new_depth;

                    if is_match && target_components == depth - 1 {
                        return Ok(Node {
                            offset: token_start,
                        });
                    }
                }
                FDT_END_NODE => {
                    if depth == 0 {
                        return Err(Error::BadToken);
                    }
                    depth -= 1;
                }
                FDT_PROP => cursor = property_parts(structure, cursor)?.next,
                FDT_NOP => {}
                FDT_END => return Err(Error::NotFound),
                _ => return Err(Error::BadToken),
            }
        }
    }

    pub(crate) fn prop(&self, node: Node, name: &[u8]) -> Result<&'a [u8]> {
        for prop in self.props(node)? {
            let prop = prop?;
            if prop.name == name {
                return Ok(prop.value);
            }
        }
        Err(Error::NotFound)
    }

    pub(crate) fn prop_str(&self, node: Node, name: &[u8]) -> Result<&'a [u8]> {
        let value = self.prop(node, name)?;
        cstr(value).ok_or(Error::BadValue)
    }

    pub(crate) fn prop_u32(&self, node: Node, name: &[u8]) -> Result<u32> {
        let value = self.prop(node, name)?;
        read_be32(value, 0)
    }

    pub(crate) fn node_phandle(&self, node: Node) -> Result<u32> {
        self.prop_u32(node, b"phandle")
            .or_else(|_| self.prop_u32(node, b"linux,phandle"))
    }

    pub(crate) fn find_phandle(&self, phandle: u32) -> Result<Node> {
        let structure = self.struct_block()?;
        let mut cursor = 0usize;
        loop {
            let token_start = cursor;
            let token = read_be32(structure, cursor)?;
            cursor = cursor.checked_add(4).ok_or(Error::BadOffset)?;

            match token {
                FDT_BEGIN_NODE => {
                    let name_end = find_nul(structure, cursor)?;
                    cursor = align_usize(name_end.checked_add(1).ok_or(Error::BadOffset)?, 4)?;
                    let node = Node {
                        offset: token_start,
                    };
                    if self.node_phandle(node).ok() == Some(phandle) {
                        return Ok(node);
                    }
                }
                FDT_END_NODE | FDT_NOP => {}
                FDT_PROP => cursor = property_parts(structure, cursor)?.next,
                FDT_END => return Err(Error::NotFound),
                _ => return Err(Error::BadToken),
            }
        }
    }

    pub(crate) fn subnodes(&self, node: Node) -> Result<Subnodes<'a, '_>> {
        let cursor = node_content_start(self.struct_block()?, node.offset)?;
        Ok(Subnodes {
            reader: self,
            cursor,
            done: false,
        })
    }

    pub(crate) fn props(&self, node: Node) -> Result<Props<'a, '_>> {
        let cursor = node_content_start(self.struct_block()?, node.offset)?;
        Ok(Props {
            reader: self,
            cursor,
            done: false,
        })
    }

    pub(crate) fn node_name(&self, node: Node) -> Result<&'a [u8]> {
        node_name(self.struct_block()?, node.offset)
    }
}

impl Header {
    fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE {
            return Err(Error::BadOffset);
        }
        if read_be32(data, 0)? != FDT_MAGIC {
            return Err(Error::BadMagic);
        }

        let header = Self {
            totalsize: read_be32(data, 4)? as usize,
            off_dt_struct: read_be32(data, 8)? as usize,
            off_dt_strings: read_be32(data, 12)? as usize,
            off_mem_rsvmap: read_be32(data, 16)? as usize,
            version: read_be32(data, 20)?,
            last_comp_version: read_be32(data, 24)?,
            boot_cpuid_phys: read_be32(data, 28)?,
            size_dt_strings: read_be32(data, 32)? as usize,
            size_dt_struct: read_be32(data, 36)? as usize,
        };

        if header.totalsize < HEADER_SIZE || header.totalsize > data.len() {
            return Err(Error::BadOffset);
        }
        let data = &data[..header.totalsize];
        checked_slice(data, header.off_dt_struct, header.size_dt_struct)?;
        checked_slice(data, header.off_dt_strings, header.size_dt_strings)?;
        if header.version < 17
            || header.last_comp_version > 17
            || header.off_dt_struct < HEADER_SIZE
            || header.off_dt_struct % 4 != 0
            || header.off_dt_strings < HEADER_SIZE
            || header.off_mem_rsvmap < HEADER_SIZE
            || header.off_mem_rsvmap % 8 != 0
            || header.off_mem_rsvmap >= header.totalsize
        {
            return Err(Error::BadOffset);
        }

        Ok(header)
    }
}

pub(crate) struct Subnodes<'a, 'r> {
    reader: &'r Reader<'a>,
    cursor: usize,
    done: bool,
}

impl<'a> Iterator for Subnodes<'a, '_> {
    type Item = Result<Node>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let structure = match self.reader.struct_block() {
            Ok(structure) => structure,
            Err(err) => return Some(Err(err)),
        };

        loop {
            let token_start = self.cursor;
            let token = match read_be32(structure, self.cursor) {
                Ok(token) => token,
                Err(err) => return Some(Err(err)),
            };
            self.cursor += 4;

            match token {
                FDT_BEGIN_NODE => match skip_node_subtree(structure, token_start) {
                    Ok(next) => {
                        self.cursor = next;
                        return Some(Ok(Node {
                            offset: token_start,
                        }));
                    }
                    Err(err) => return Some(Err(err)),
                },
                FDT_END_NODE | FDT_END => {
                    self.done = true;
                    return None;
                }
                FDT_PROP => match property_parts(structure, self.cursor) {
                    Ok(parts) => self.cursor = parts.next,
                    Err(err) => return Some(Err(err)),
                },
                FDT_NOP => {}
                _ => return Some(Err(Error::BadToken)),
            }
        }
    }
}

pub(crate) struct Props<'a, 'r> {
    reader: &'r Reader<'a>,
    cursor: usize,
    done: bool,
}

impl<'a> Iterator for Props<'a, '_> {
    type Item = Result<Prop<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let structure = match self.reader.struct_block() {
            Ok(structure) => structure,
            Err(err) => return Some(Err(err)),
        };
        let strings = match self.reader.strings() {
            Ok(strings) => strings,
            Err(err) => return Some(Err(err)),
        };

        loop {
            let token = match read_be32(structure, self.cursor) {
                Ok(token) => token,
                Err(err) => return Some(Err(err)),
            };
            self.cursor += 4;

            match token {
                FDT_PROP => {
                    let parts = match property_parts(structure, self.cursor) {
                        Ok(parts) => parts,
                        Err(err) => return Some(Err(err)),
                    };
                    self.cursor = parts.next;
                    let name = match string_at(strings, parts.nameoff) {
                        Some(name) => name,
                        None => return Some(Err(Error::BadOffset)),
                    };
                    return Some(Ok(Prop {
                        name,
                        value: &structure[parts.value_start..parts.value_end],
                    }));
                }
                FDT_BEGIN_NODE | FDT_END_NODE | FDT_END => {
                    self.done = true;
                    return None;
                }
                FDT_NOP => {}
                _ => return Some(Err(Error::BadToken)),
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PropertyParts {
    pub(crate) nameoff: u32,
    pub(crate) value_start: usize,
    pub(crate) value_end: usize,
    pub(crate) next: usize,
}

pub(crate) struct Writer<'a> {
    output: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(output: &'a mut [u8]) -> Self {
        Self { output, pos: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn reserve(&mut self, size: usize) -> Result<usize> {
        let start = self.pos;
        self.write_zeros(size)?;
        Ok(start)
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self
            .pos
            .checked_add(bytes.len())
            .ok_or(Error::BufferTooSmall)?;
        if end > self.output.len() {
            return Err(Error::BufferTooSmall);
        }
        self.output[self.pos..end].copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    pub(crate) fn write_zeros(&mut self, len: usize) -> Result<()> {
        let end = self.pos.checked_add(len).ok_or(Error::BufferTooSmall)?;
        if end > self.output.len() {
            return Err(Error::BufferTooSmall);
        }
        self.output[self.pos..end].fill(0);
        self.pos = end;
        Ok(())
    }

    pub(crate) fn write_be32(&mut self, value: u32) -> Result<()> {
        self.write(&value.to_be_bytes())
    }

    pub(crate) fn pad_to(&mut self, align: usize) -> Result<()> {
        let padded = align_usize(self.pos, align)?;
        self.write_zeros(padded - self.pos)
    }

    pub(crate) fn finish(self) -> &'a mut [u8] {
        let len = self.pos;
        &mut self.output[..len]
    }
}

pub(crate) fn write_prop(writer: &mut Writer<'_>, nameoff: u32, value: &[u8]) -> Result<()> {
    writer.write_be32(FDT_PROP)?;
    writer.write_be32(value.len() as u32)?;
    writer.write_be32(nameoff)?;
    writer.write(value)?;
    writer.pad_to(4)
}

pub(crate) fn write_header(
    writer: &mut Writer<'_>,
    header: Header,
    totalsize: usize,
    off_dt_struct: usize,
    off_dt_strings: usize,
    size_dt_struct: usize,
    size_dt_strings: usize,
) -> Result<()> {
    write_be32_at(writer.output, 0, FDT_MAGIC)?;
    write_be32_at(writer.output, 4, u32_len(totalsize)?)?;
    write_be32_at(writer.output, 8, u32_len(off_dt_struct)?)?;
    write_be32_at(writer.output, 12, u32_len(off_dt_strings)?)?;
    write_be32_at(writer.output, 16, HEADER_SIZE as u32)?;
    write_be32_at(writer.output, 20, header.version)?;
    write_be32_at(writer.output, 24, header.last_comp_version)?;
    write_be32_at(writer.output, 28, header.boot_cpuid_phys)?;
    write_be32_at(writer.output, 32, u32_len(size_dt_strings)?)?;
    write_be32_at(writer.output, 36, u32_len(size_dt_struct)?)?;
    Ok(())
}

pub(crate) fn read_be32(data: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).ok_or(Error::BadOffset)?;
    if end > data.len() {
        return Err(Error::BadOffset);
    }
    Ok(u32::from_be_bytes(data[offset..end].try_into().unwrap()))
}

pub(crate) fn read_cells(data: &[u8], offset: usize, cells: usize) -> Result<u64> {
    if cells == 0 || cells > 2 {
        return Err(Error::BadValue);
    }

    let mut value = 0u64;
    for index in 0..cells {
        value = (value << 32) | read_be32(data, offset + index * 4)? as u64;
    }
    Ok(value)
}

pub(crate) fn cstr(value: &[u8]) -> Option<&[u8]> {
    let len = value.iter().position(|byte| *byte == 0)?;
    Some(&value[..len])
}

pub(crate) fn stringlist_contains(value: &[u8], needle: &[u8]) -> bool {
    let mut cursor = 0usize;
    while cursor < value.len() {
        let Some(relative_end) = value[cursor..].iter().position(|byte| *byte == 0) else {
            return false;
        };
        let end = cursor + relative_end;
        if &value[cursor..end] == needle {
            return true;
        }
        cursor = end + 1;
    }
    false
}

pub(crate) fn node_name(structure: &[u8], node_offset: usize) -> Result<&[u8]> {
    if read_be32(structure, node_offset)? != FDT_BEGIN_NODE {
        return Err(Error::BadToken);
    }
    let name_start = node_offset.checked_add(4).ok_or(Error::BadOffset)?;
    let name_end = find_nul(structure, name_start)?;
    Ok(&structure[name_start..name_end])
}

pub(crate) fn node_content_start(structure: &[u8], node_offset: usize) -> Result<usize> {
    if read_be32(structure, node_offset)? != FDT_BEGIN_NODE {
        return Err(Error::BadToken);
    }
    let name_start = node_offset.checked_add(4).ok_or(Error::BadOffset)?;
    let name_end = find_nul(structure, name_start)?;
    align_usize(name_end.checked_add(1).ok_or(Error::BadOffset)?, 4)
}

pub(crate) fn property_parts(structure: &[u8], cursor: usize) -> Result<PropertyParts> {
    let len = read_be32(structure, cursor)?;
    let nameoff = read_be32(structure, cursor + 4)?;
    let value_start = cursor.checked_add(8).ok_or(Error::BadOffset)?;
    let value_end = value_start
        .checked_add(len as usize)
        .ok_or(Error::BadOffset)?;
    let next = align_usize(value_end, 4)?;
    if next > structure.len() {
        return Err(Error::BadOffset);
    }
    Ok(PropertyParts {
        nameoff,
        value_start,
        value_end,
        next,
    })
}

pub(crate) fn string_at(strings: &[u8], offset: u32) -> Option<&[u8]> {
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

pub(crate) fn find_string(strings: &[u8], needle: &[u8]) -> Option<u32> {
    let mut cursor = 0usize;
    while cursor < strings.len() {
        let relative_end = strings[cursor..].iter().position(|byte| *byte == 0)?;
        let end = cursor + relative_end;
        if &strings[cursor..end] == needle {
            return u32::try_from(cursor).ok();
        }
        cursor = end + 1;
    }
    None
}

pub(crate) fn find_nul(data: &[u8], start: usize) -> Result<usize> {
    if start > data.len() {
        return Err(Error::BadOffset);
    }
    data[start..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|pos| start + pos)
        .ok_or(Error::Unterminated)
}

pub(crate) fn align_usize(value: usize, align: usize) -> Result<usize> {
    if align == 0 || !align.is_power_of_two() {
        return Err(Error::BadValue);
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
        .ok_or(Error::BadOffset)
}

pub(crate) fn skip_node_subtree(structure: &[u8], start: usize) -> Result<usize> {
    let mut cursor = start;
    let mut depth = 0usize;

    loop {
        let token = read_be32(structure, cursor)?;
        cursor = cursor.checked_add(4).ok_or(Error::BadOffset)?;

        match token {
            FDT_BEGIN_NODE => {
                let name_end = find_nul(structure, cursor)?;
                cursor = align_usize(name_end.checked_add(1).ok_or(Error::BadOffset)?, 4)?;
                depth = depth.checked_add(1).ok_or(Error::BadOffset)?;
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return Err(Error::BadToken);
                }
                depth -= 1;
                if depth == 0 {
                    return Ok(cursor);
                }
            }
            FDT_PROP => cursor = property_parts(structure, cursor)?.next,
            FDT_NOP => {}
            FDT_END => return Err(Error::Unterminated),
            _ => return Err(Error::BadToken),
        }
    }
}

pub(crate) fn basename_eq(name: &[u8], basename: &[u8]) -> bool {
    let end = name
        .iter()
        .position(|byte| *byte == b'@')
        .unwrap_or(name.len());
    &name[..end] == basename
}

pub(crate) fn checked_slice(data: &[u8], offset: usize, size: usize) -> Result<&[u8]> {
    let end = offset.checked_add(size).ok_or(Error::BadOffset)?;
    if end > data.len() {
        return Err(Error::BadOffset);
    }
    Ok(&data[offset..end])
}

pub(crate) fn u32_len(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::BadValue)
}

fn write_be32_at(data: &mut [u8], offset: usize, value: u32) -> Result<()> {
    let end = offset.checked_add(4).ok_or(Error::BadOffset)?;
    if end > data.len() {
        return Err(Error::BadOffset);
    }
    data[offset..end].copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn count_path_components(path: &[u8]) -> Result<usize> {
    let mut count = 0usize;
    let mut cursor = 1usize;
    while cursor < path.len() {
        if path[cursor] == b'/' {
            return Err(Error::BadValue);
        }
        count += 1;
        while cursor < path.len() && path[cursor] != b'/' {
            cursor += 1;
        }
        if cursor < path.len() {
            cursor += 1;
        }
    }
    Ok(count)
}

fn component_at(path: &[u8], index: usize) -> Option<&[u8]> {
    let mut current = 0usize;
    let mut cursor = 1usize;
    while cursor < path.len() {
        let start = cursor;
        while cursor < path.len() && path[cursor] != b'/' {
            cursor += 1;
        }
        if current == index {
            return Some(&path[start..cursor]);
        }
        current += 1;
        if cursor < path.len() {
            cursor += 1;
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn test_dtb(build_root: impl FnOnce(&mut Vec<u8>, &mut Vec<u8>)) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut structure = Vec::new();

        write_begin_node_vec(&mut structure, b"");
        build_root(&mut structure, &mut strings);
        write_be32_vec(&mut structure, FDT_END_NODE);
        write_be32_vec(&mut structure, FDT_END);

        build_dtb_vec(&structure, &strings)
    }

    pub(crate) fn ensure_string_vec(strings: &mut Vec<u8>, name: &[u8]) -> u32 {
        if let Some(offset) = find_string(strings, name) {
            return offset;
        }
        let offset = strings.len();
        strings.extend_from_slice(name);
        strings.push(0);
        offset as u32
    }

    pub(crate) fn write_begin_node_vec(output: &mut Vec<u8>, name: &[u8]) {
        write_be32_vec(output, FDT_BEGIN_NODE);
        output.extend_from_slice(name);
        output.push(0);
        while output.len() % 4 != 0 {
            output.push(0);
        }
    }

    pub(crate) fn write_prop_vec(output: &mut Vec<u8>, nameoff: u32, value: &[u8]) {
        write_be32_vec(output, FDT_PROP);
        write_be32_vec(output, value.len() as u32);
        write_be32_vec(output, nameoff);
        output.extend_from_slice(value);
        while output.len() % 4 != 0 {
            output.push(0);
        }
    }

    pub(crate) fn write_be32_vec(output: &mut Vec<u8>, value: u32) {
        output.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn write_be64_vec(output: &mut Vec<u8>, value: u64) {
        output.extend_from_slice(&value.to_be_bytes());
    }

    fn build_dtb_vec(structure: &[u8], strings: &[u8]) -> Vec<u8> {
        let mut output = vec![0; HEADER_SIZE];
        output.extend_from_slice(&[0; 16]);
        while output.len() % 4 != 0 {
            output.push(0);
        }
        let off_dt_struct = output.len();
        output.extend_from_slice(structure);
        let off_dt_strings = output.len();
        output.extend_from_slice(strings);
        let totalsize = output.len();

        write_be32_at(&mut output, 0, FDT_MAGIC).unwrap();
        write_be32_at(&mut output, 4, totalsize as u32).unwrap();
        write_be32_at(&mut output, 8, off_dt_struct as u32).unwrap();
        write_be32_at(&mut output, 12, off_dt_strings as u32).unwrap();
        write_be32_at(&mut output, 16, HEADER_SIZE as u32).unwrap();
        write_be32_at(&mut output, 20, 17).unwrap();
        write_be32_at(&mut output, 24, 16).unwrap();
        write_be32_at(&mut output, 28, 0).unwrap();
        write_be32_at(&mut output, 32, strings.len() as u32).unwrap();
        write_be32_at(&mut output, 36, structure.len() as u32).unwrap();
        output
    }

    #[test]
    fn finds_paths_and_alias_targets() {
        let dtb = test_dtb(|structure, strings| {
            let serial0 = ensure_string_vec(strings, b"serial0");
            write_begin_node_vec(structure, b"aliases");
            write_prop_vec(structure, serial0, b"/soc@0/serial@78b0000\0");
            write_be32_vec(structure, FDT_END_NODE);

            write_begin_node_vec(structure, b"soc@0");
            write_begin_node_vec(structure, b"serial@78b0000");
            write_be32_vec(structure, FDT_END_NODE);
            write_be32_vec(structure, FDT_END_NODE);
        });
        let reader = Reader::new(&dtb).unwrap();
        let aliases = reader.find_path(b"/aliases").unwrap();
        assert_eq!(
            reader.prop_str(aliases, b"serial0").unwrap(),
            b"/soc@0/serial@78b0000"
        );
        assert!(reader.find_path(b"/soc@0/serial@78b0000").is_ok());
    }
}
