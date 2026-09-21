// Android logical-partition metadata support for early, read-only firmware access.
//
// The on-disk format definitions are derived from AOSP's Apache-2.0 licensed
// fs_mgr/liblp/include/liblp/metadata_format.h. No code from the GPL
// make-dynpart-mappings utility is included here.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, RawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::partitions::{self, Partition};

const SECTOR_SIZE: u64 = 512;
const PARTITION_RESERVED_BYTES: u64 = 4096;
const GEOMETRY_SIZE: usize = 4096;
const GEOMETRY_STRUCT_SIZE: usize = 52;
const GEOMETRY_MAGIC: u32 = 0x616c_4467;
const HEADER_MAGIC: u32 = 0x414c_5030;
const HEADER_MAJOR: u16 = 10;
const HEADER_V1_0_SIZE: usize = 128;
const HEADER_V1_2_SIZE: usize = 256;
const MAX_METADATA_SIZE: usize = 16 * 1024 * 1024;
const MAX_METADATA_SLOTS: u32 = 3;

const PARTITION_ENTRY_SIZE: usize = 52;
const EXTENT_ENTRY_SIZE: usize = 24;
const GROUP_ENTRY_SIZE: usize = 48;
const BLOCK_DEVICE_ENTRY_SIZE: usize = 64;

const PARTITION_ATTR_READONLY: u32 = 1 << 0;
const PARTITION_ATTR_SLOT_SUFFIXED: u32 = 1 << 1;
const PARTITION_ATTR_UPDATED: u32 = 1 << 2;
const PARTITION_ATTR_DISABLED: u32 = 1 << 3;
const PARTITION_ATTR_MASK_V0: u32 = PARTITION_ATTR_READONLY | PARTITION_ATTR_SLOT_SUFFIXED;
const PARTITION_ATTR_MASK_V1: u32 = PARTITION_ATTR_UPDATED | PARTITION_ATTR_DISABLED;
const BLOCK_DEVICE_SLOT_SUFFIXED: u32 = 1 << 0;
const GROUP_SLOT_SUFFIXED: u32 = 1 << 0;

const TARGET_LINEAR: u32 = 0;
const TARGET_ZERO: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetadataCopy {
    Primary,
    Backup,
}

#[derive(Clone, Debug)]
struct Geometry {
    metadata_max_size: usize,
    metadata_slot_count: u32,
    logical_block_size: u32,
}

#[derive(Clone, Copy, Debug)]
struct TableDescriptor {
    offset: usize,
    entries: usize,
    entry_size: usize,
}

impl TableDescriptor {
    fn bytes(self) -> io::Result<usize> {
        self.entries
            .checked_mul(self.entry_size)
            .ok_or_else(|| invalid_data("LP table size overflows"))
    }
}

#[derive(Clone, Debug)]
struct LogicalPartition {
    name: String,
    attributes: u32,
    first_extent: usize,
    extent_count: usize,
}

#[derive(Clone, Debug)]
struct Extent {
    sectors: u64,
    target: ExtentTarget,
}

#[derive(Clone, Debug)]
enum ExtentTarget {
    Linear {
        physical_sector: u64,
        block_device: usize,
    },
    Zero,
}

#[derive(Clone, Debug)]
struct PhysicalDevice {
    first_logical_sector: u64,
    size_bytes: u64,
    name: String,
    flags: u32,
}

#[derive(Clone, Debug)]
struct Metadata {
    copy: MetadataCopy,
    geometry: Geometry,
    minor_version: u16,
    partitions: Vec<LogicalPartition>,
    extents: Vec<Extent>,
    block_devices: Vec<PhysicalDevice>,
}

#[derive(Clone, Debug)]
enum DmTarget {
    Linear {
        logical_start: u64,
        sectors: u64,
        devnum: String,
        physical_start: u64,
    },
    Zero {
        logical_start: u64,
        sectors: u64,
    },
}

#[derive(Clone, Debug)]
struct MappingPlan {
    base_name: String,
    dm_name: String,
    dm_uuid: String,
    targets: Vec<DmTarget>,
}

#[derive(Debug)]
struct Mapping {
    uuid: String,
    path: PathBuf,
}

pub(crate) struct MappedPartitions {
    mapper: Option<DeviceMapper>,
    mappings: BTreeMap<String, Mapping>,
    cleaned: bool,
}

impl MappedPartitions {
    pub(crate) fn path_for(&self, base_name: &str) -> Option<&Path> {
        self.mappings
            .get(base_name)
            .map(|mapping| mapping.path.as_path())
    }

    pub(crate) fn cleanup(mut self) -> io::Result<()> {
        let result = self.cleanup_inner();
        self.cleaned = true;
        result
    }

    fn cleanup_inner(&mut self) -> io::Result<()> {
        let Some(mapper) = &self.mapper else {
            return Ok(());
        };
        let mut first_error = None;
        for mapping in self.mappings.values().rev() {
            if let Err(err) = mapper.remove(&mapping.uuid)
                && first_error.is_none()
            {
                first_error = Some(err);
            }
            match fs::remove_file(&mapping.path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
        self.mappings.clear();
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

impl Drop for MappedPartitions {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(err) = self.cleanup_inner()
        {
            tracing::warn!(error = %err, "failed to clean up firmware device-mapper mappings");
        }
    }
}

pub(crate) fn map_read_only(
    metadata_device: &Partition,
    metadata_slot: u32,
    requested_base_names: &[String],
) -> io::Result<MappedPartitions> {
    let metadata = read_metadata(metadata_device, metadata_slot)?;
    let suffix = slot_suffix(metadata_slot)?;
    tracing::info!(
        device = %metadata_device.dev_path.display(),
        metadata_slot,
        metadata_copy = ?metadata.copy,
        metadata_minor = metadata.minor_version,
        "validated Android logical-partition metadata"
    );

    let physical = resolve_physical_devices(&metadata, metadata_device, suffix)?;
    let plans = mapping_plans(&metadata, &physical, metadata_slot, requested_base_names)?;
    if plans.is_empty() {
        return Ok(MappedPartitions {
            mapper: None,
            mappings: BTreeMap::new(),
            cleaned: true,
        });
    }

    let mapper = DeviceMapper::open()?;
    mapper.check_version()?;
    let mut mapped = MappedPartitions {
        mapper: Some(mapper),
        mappings: BTreeMap::new(),
        cleaned: false,
    };

    for plan in plans {
        if mapped.mappings.contains_key(&plan.base_name) {
            return Err(invalid_data(format!(
                "duplicate LP mapping for {:?}",
                plan.base_name
            )));
        }
        let mapper = mapped.mapper.as_ref().expect("mapper is present");
        let mapping = mapper.create_read_only(&plan)?;
        tracing::info!(
            partition = plan.base_name,
            mapping = plan.dm_name,
            path = %mapping.path.display(),
            "created verified read-only firmware mapping"
        );
        mapped.mappings.insert(plan.base_name, mapping);
    }

    Ok(mapped)
}

fn read_metadata(device: &Partition, slot: u32) -> io::Result<Metadata> {
    let file = File::open(&device.dev_path)?;
    let geometry = read_geometry(&file)?;
    if slot >= geometry.metadata_slot_count {
        return Err(invalid_data(format!(
            "metadata slot {slot} is outside slot count {}",
            geometry.metadata_slot_count
        )));
    }

    let primary = metadata_offset(&geometry, slot, MetadataCopy::Primary)?;
    let backup = metadata_offset(&geometry, slot, MetadataCopy::Backup)?;
    let primary_result = read_metadata_at(&file, primary, &geometry, MetadataCopy::Primary);
    match primary_result {
        Ok(metadata) => Ok(metadata),
        Err(primary_error) => {
            tracing::warn!(
                error = %primary_error,
                slot,
                "primary Android LP metadata is invalid; trying matching backup"
            );
            read_metadata_at(&file, backup, &geometry, MetadataCopy::Backup).map_err(
                |backup_error| {
                    invalid_data(format!(
                        "primary LP metadata invalid ({primary_error}); matching backup invalid ({backup_error})"
                    ))
                },
            )
        }
    }
}

fn read_geometry(file: &File) -> io::Result<Geometry> {
    let mut primary = vec![0u8; GEOMETRY_SIZE];
    read_exact_at(file, PARTITION_RESERVED_BYTES, &mut primary)?;
    match parse_geometry(&primary) {
        Ok(geometry) => Ok(geometry),
        Err(primary_error) => {
            let mut backup = vec![0u8; GEOMETRY_SIZE];
            read_exact_at(
                file,
                PARTITION_RESERVED_BYTES + GEOMETRY_SIZE as u64,
                &mut backup,
            )?;
            parse_geometry(&backup).map_err(|backup_error| {
                invalid_data(format!(
                    "primary LP geometry invalid ({primary_error}); backup invalid ({backup_error})"
                ))
            })
        }
    }
}

fn parse_geometry(block: &[u8]) -> io::Result<Geometry> {
    if block.len() < GEOMETRY_STRUCT_SIZE {
        return Err(invalid_data("LP geometry block is truncated"));
    }
    if le_u32(block, 0)? != GEOMETRY_MAGIC {
        return Err(invalid_data("invalid LP geometry magic"));
    }
    if le_u32(block, 4)? as usize != GEOMETRY_STRUCT_SIZE {
        return Err(invalid_data("unsupported LP geometry structure size"));
    }
    verify_checksum(
        &block[..GEOMETRY_STRUCT_SIZE],
        8..40,
        &block[8..40],
        "geometry",
    )?;

    let metadata_max_size = le_u32(block, 40)? as usize;
    if metadata_max_size == 0
        || metadata_max_size > MAX_METADATA_SIZE
        || !metadata_max_size.is_multiple_of(SECTOR_SIZE as usize)
    {
        return Err(invalid_data(format!(
            "invalid LP metadata maximum size {metadata_max_size}"
        )));
    }
    let metadata_slot_count = le_u32(block, 44)?;
    if !(1..=MAX_METADATA_SLOTS).contains(&metadata_slot_count) {
        return Err(invalid_data(format!(
            "invalid LP metadata slot count {metadata_slot_count}"
        )));
    }
    let logical_block_size = le_u32(block, 48)?;
    if logical_block_size == 0 || !(logical_block_size as u64).is_multiple_of(SECTOR_SIZE) {
        return Err(invalid_data(format!(
            "invalid LP logical block size {logical_block_size}"
        )));
    }
    Ok(Geometry {
        metadata_max_size,
        metadata_slot_count,
        logical_block_size,
    })
}

fn metadata_offset(geometry: &Geometry, slot: u32, copy: MetadataCopy) -> io::Result<u64> {
    let metadata_start = PARTITION_RESERVED_BYTES
        .checked_add((GEOMETRY_SIZE * 2) as u64)
        .ok_or_else(|| invalid_data("LP metadata start overflows"))?;
    let copy_slot = match copy {
        MetadataCopy::Primary => slot,
        MetadataCopy::Backup => geometry
            .metadata_slot_count
            .checked_add(slot)
            .ok_or_else(|| invalid_data("LP backup slot overflows"))?,
    };
    metadata_start
        .checked_add(
            (geometry.metadata_max_size as u64)
                .checked_mul(copy_slot as u64)
                .ok_or_else(|| invalid_data("LP metadata offset overflows"))?,
        )
        .ok_or_else(|| invalid_data("LP metadata offset overflows"))
}

fn read_metadata_at(
    file: &File,
    offset: u64,
    geometry: &Geometry,
    copy: MetadataCopy,
) -> io::Result<Metadata> {
    let mut bytes = vec![0u8; geometry.metadata_max_size];
    read_exact_at(file, offset, &mut bytes)?;
    parse_metadata(&bytes, geometry.clone(), copy)
}

fn parse_metadata(bytes: &[u8], geometry: Geometry, copy: MetadataCopy) -> io::Result<Metadata> {
    if le_u32(bytes, 0)? != HEADER_MAGIC {
        return Err(invalid_data("invalid LP metadata header magic"));
    }
    let major = le_u16(bytes, 4)?;
    let minor = le_u16(bytes, 6)?;
    if major != HEADER_MAJOR || minor > 2 {
        return Err(invalid_data(format!(
            "unsupported LP metadata version {major}.{minor}"
        )));
    }
    let header_size = le_u32(bytes, 8)? as usize;
    let expected_header_size = if minor >= 2 {
        HEADER_V1_2_SIZE
    } else {
        HEADER_V1_0_SIZE
    };
    if header_size != expected_header_size || header_size > bytes.len() {
        return Err(invalid_data(format!(
            "invalid LP header size {header_size} for version {major}.{minor}"
        )));
    }
    verify_checksum(
        &bytes[..header_size],
        12..44,
        checked_slice(bytes, 12, 32, "header checksum")?,
        "header",
    )?;

    let tables_size = le_u32(bytes, 44)? as usize;
    let tables_end = header_size
        .checked_add(tables_size)
        .ok_or_else(|| invalid_data("LP tables size overflows"))?;
    if tables_end > bytes.len() {
        return Err(invalid_data("LP tables exceed metadata buffer"));
    }
    let tables = &bytes[header_size..tables_end];
    let expected_tables_checksum = checked_slice(bytes, 48, 32, "tables checksum")?;
    if Sha256::digest(tables).as_slice() != expected_tables_checksum {
        return Err(invalid_data("LP tables checksum mismatch"));
    }

    let partition_desc = parse_descriptor(bytes, 80, PARTITION_ENTRY_SIZE, "partition")?;
    let extent_desc = parse_descriptor(bytes, 92, EXTENT_ENTRY_SIZE, "extent")?;
    let group_desc = parse_descriptor(bytes, 104, GROUP_ENTRY_SIZE, "group")?;
    let block_desc = parse_descriptor(bytes, 116, BLOCK_DEVICE_ENTRY_SIZE, "block device")?;
    validate_table_layout(
        tables_size,
        [partition_desc, extent_desc, group_desc, block_desc],
    )?;
    if block_desc.entries == 0 {
        return Err(invalid_data("LP metadata has no block devices"));
    }

    validate_groups(tables, group_desc)?;
    let block_devices = parse_block_devices(tables, block_desc)?;
    let extents = parse_extents(tables, extent_desc)?;
    for (index, extent) in extents.iter().enumerate() {
        if let ExtentTarget::Linear { block_device, .. } = &extent.target
            && *block_device >= block_devices.len()
        {
            return Err(invalid_data(format!(
                "LP extent {index} references missing block device {block_device}"
            )));
        }
    }
    let partitions = parse_partitions(tables, partition_desc, minor, group_desc.entries, &extents)?;

    Ok(Metadata {
        copy,
        geometry,
        minor_version: minor,
        partitions,
        extents,
        block_devices,
    })
}

fn parse_descriptor(
    bytes: &[u8],
    offset: usize,
    expected_entry_size: usize,
    name: &str,
) -> io::Result<TableDescriptor> {
    let descriptor = TableDescriptor {
        offset: le_u32(bytes, offset)? as usize,
        entries: le_u32(bytes, offset + 4)? as usize,
        entry_size: le_u32(bytes, offset + 8)? as usize,
    };
    if descriptor.entry_size != expected_entry_size {
        return Err(invalid_data(format!(
            "invalid LP {name} entry size {}",
            descriptor.entry_size
        )));
    }
    Ok(descriptor)
}

fn validate_table_layout(tables_size: usize, descriptors: [TableDescriptor; 4]) -> io::Result<()> {
    let mut ranges = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        let end = descriptor
            .offset
            .checked_add(descriptor.bytes()?)
            .ok_or_else(|| invalid_data("LP table range overflows"))?;
        if end > tables_size {
            return Err(invalid_data("LP table exceeds declared tables size"));
        }
        if end != descriptor.offset {
            ranges.push(descriptor.offset..end);
        }
    }
    ranges.sort_by_key(|range| range.start);
    let mut cursor = 0;
    for range in ranges {
        if range.start != cursor {
            return Err(invalid_data("LP tables contain a gap or overlap"));
        }
        cursor = range.end;
    }
    if cursor != tables_size {
        return Err(invalid_data("LP tables do not cover tables_size"));
    }
    Ok(())
}

fn validate_groups(tables: &[u8], descriptor: TableDescriptor) -> io::Result<()> {
    for index in 0..descriptor.entries {
        let entry = table_entry(tables, descriptor, index, "group")?;
        let _name = fixed_name(entry, 0, 36, false, "group name")?;
        let flags = le_u32(entry, 36)?;
        if flags & !GROUP_SLOT_SUFFIXED != 0 {
            return Err(invalid_data(format!(
                "LP group {index} has unsupported flags {flags:#x}"
            )));
        }
    }
    Ok(())
}

fn parse_block_devices(
    tables: &[u8],
    descriptor: TableDescriptor,
) -> io::Result<Vec<PhysicalDevice>> {
    let mut devices = Vec::with_capacity(descriptor.entries);
    let mut names = BTreeSet::new();
    for index in 0..descriptor.entries {
        let entry = table_entry(tables, descriptor, index, "block device")?;
        let name = fixed_name(entry, 24, 36, true, "block device name")?;
        if !names.insert(name.clone()) {
            return Err(invalid_data(format!(
                "duplicate LP block device name {name:?}"
            )));
        }
        let flags = le_u32(entry, 60)?;
        if flags & !BLOCK_DEVICE_SLOT_SUFFIXED != 0 {
            return Err(invalid_data(format!(
                "LP block device {name:?} has unsupported flags {flags:#x}"
            )));
        }
        let size_bytes = le_u64(entry, 16)?;
        if size_bytes == 0 || size_bytes % SECTOR_SIZE != 0 {
            return Err(invalid_data(format!(
                "LP block device {name:?} has invalid size {size_bytes}"
            )));
        }
        devices.push(PhysicalDevice {
            first_logical_sector: le_u64(entry, 0)?,
            size_bytes,
            name,
            flags,
        });
    }
    Ok(devices)
}

fn parse_extents(tables: &[u8], descriptor: TableDescriptor) -> io::Result<Vec<Extent>> {
    let mut extents = Vec::with_capacity(descriptor.entries);
    for index in 0..descriptor.entries {
        let entry = table_entry(tables, descriptor, index, "extent")?;
        let sectors = le_u64(entry, 0)?;
        if sectors == 0 {
            return Err(invalid_data(format!("LP extent {index} is empty")));
        }
        let target_type = le_u32(entry, 8)?;
        let target_data = le_u64(entry, 12)?;
        let target_source = le_u32(entry, 20)? as usize;
        let target = match target_type {
            TARGET_LINEAR => ExtentTarget::Linear {
                physical_sector: target_data,
                block_device: target_source,
            },
            TARGET_ZERO if target_data == 0 && target_source == 0 => ExtentTarget::Zero,
            TARGET_ZERO => {
                return Err(invalid_data(format!(
                    "LP zero extent {index} has nonzero target fields"
                )));
            }
            other => {
                return Err(invalid_data(format!(
                    "LP extent {index} has unsupported target type {other}"
                )));
            }
        };
        extents.push(Extent { sectors, target });
    }
    Ok(extents)
}

fn parse_partitions(
    tables: &[u8],
    descriptor: TableDescriptor,
    minor: u16,
    group_count: usize,
    extents: &[Extent],
) -> io::Result<Vec<LogicalPartition>> {
    let valid_attributes = PARTITION_ATTR_MASK_V0
        | if minor >= 1 {
            PARTITION_ATTR_MASK_V1
        } else {
            0
        };
    let mut partitions = Vec::with_capacity(descriptor.entries);
    let mut names = BTreeSet::new();
    for index in 0..descriptor.entries {
        let entry = table_entry(tables, descriptor, index, "partition")?;
        let name = fixed_name(entry, 0, 36, false, "partition name")?;
        if !names.insert(name.clone()) {
            return Err(invalid_data(format!(
                "duplicate LP partition name {name:?}"
            )));
        }
        let attributes = le_u32(entry, 36)?;
        if attributes & !valid_attributes != 0 {
            return Err(invalid_data(format!(
                "LP partition {name:?} has unsupported attributes {attributes:#x}"
            )));
        }
        let first_extent = le_u32(entry, 40)? as usize;
        let extent_count = le_u32(entry, 44)? as usize;
        let extent_end = first_extent
            .checked_add(extent_count)
            .ok_or_else(|| invalid_data("LP partition extent range overflows"))?;
        if extent_end > extents.len() {
            return Err(invalid_data(format!(
                "LP partition {name:?} extents exceed the extent table"
            )));
        }
        if attributes & PARTITION_ATTR_DISABLED == 0 && extent_count == 0 {
            return Err(invalid_data(format!(
                "active LP partition {name:?} has no extents"
            )));
        }
        let group = le_u32(entry, 48)? as usize;
        if group >= group_count {
            return Err(invalid_data(format!(
                "LP partition {name:?} references missing group {group}"
            )));
        }
        partitions.push(LogicalPartition {
            name,
            attributes,
            first_extent,
            extent_count,
        });
    }
    Ok(partitions)
}

fn resolve_physical_devices(
    metadata: &Metadata,
    metadata_device: &Partition,
    suffix: &str,
) -> io::Result<Vec<Partition>> {
    let mut resolved = Vec::with_capacity(metadata.block_devices.len());
    for (index, device) in metadata.block_devices.iter().enumerate() {
        let effective_name = if device.flags & BLOCK_DEVICE_SLOT_SUFFIXED != 0 {
            format!("{}{suffix}", device.name)
        } else {
            device.name.clone()
        };
        let partition = partitions::find(&effective_name).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("resolve LP block device {effective_name:?}: {err}"),
            )
        })?;
        if index == 0 && partition.dev_path != metadata_device.dev_path {
            return Err(invalid_data(format!(
                "LP metadata names first block device {:?}, but metadata was read from {}",
                effective_name,
                metadata_device.dev_path.display()
            )));
        }
        if partition.size_bytes < device.size_bytes {
            return Err(invalid_data(format!(
                "LP block device {:?} is {} bytes, smaller than metadata declaration {}",
                effective_name, partition.size_bytes, device.size_bytes
            )));
        }
        if device.first_logical_sector > device.size_bytes / SECTOR_SIZE {
            return Err(invalid_data(format!(
                "LP block device {:?} first logical sector is out of bounds",
                effective_name
            )));
        }
        resolved.push(partition);
    }
    Ok(resolved)
}

fn mapping_plans(
    metadata: &Metadata,
    physical: &[Partition],
    slot: u32,
    requested_base_names: &[String],
) -> io::Result<Vec<MappingPlan>> {
    let suffix = slot_suffix(slot)?;
    let requested = requested_base_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if requested.len() != requested_base_names.len() {
        return Err(invalid_data("duplicate requested LP base name"));
    }
    for name in &requested {
        validate_mapping_component(name)?;
    }

    let mut plans = Vec::new();
    let mut seen_bases = BTreeSet::new();
    for partition in &metadata.partitions {
        let effective_name = if partition.attributes & PARTITION_ATTR_SLOT_SUFFIXED != 0 {
            format!("{}{suffix}", partition.name)
        } else {
            partition.name.clone()
        };
        let base_name = effective_name
            .strip_suffix(suffix)
            .unwrap_or(&effective_name);
        if !requested.contains(base_name) {
            continue;
        }
        if partition.attributes & PARTITION_ATTR_DISABLED != 0 {
            return Err(invalid_data(format!(
                "requested LP partition {effective_name:?} is disabled"
            )));
        }
        if !seen_bases.insert(base_name.to_string()) {
            return Err(invalid_data(format!(
                "multiple LP partitions match requested base {base_name:?}"
            )));
        }

        let mut logical_start = 0u64;
        let mut targets = Vec::with_capacity(partition.extent_count);
        for extent in &metadata.extents
            [partition.first_extent..partition.first_extent + partition.extent_count]
        {
            let target = match &extent.target {
                ExtentTarget::Linear {
                    physical_sector,
                    block_device,
                } => {
                    let declared = metadata.block_devices.get(*block_device).ok_or_else(|| {
                        invalid_data(format!(
                            "LP partition {effective_name:?} references missing block device {block_device}"
                        ))
                    })?;
                    let resolved = physical.get(*block_device).ok_or_else(|| {
                        invalid_data("resolved LP block device list is incomplete")
                    })?;
                    let physical_end = physical_sector
                        .checked_add(extent.sectors)
                        .ok_or_else(|| invalid_data("LP physical extent overflows"))?;
                    if *physical_sector < declared.first_logical_sector
                        || physical_end > declared.size_bytes / SECTOR_SIZE
                        || physical_end > resolved.size_bytes / SECTOR_SIZE
                    {
                        return Err(invalid_data(format!(
                            "LP partition {effective_name:?} has an out-of-bounds physical extent"
                        )));
                    }
                    DmTarget::Linear {
                        logical_start,
                        sectors: extent.sectors,
                        devnum: resolved.devnum.clone(),
                        physical_start: *physical_sector,
                    }
                }
                ExtentTarget::Zero => DmTarget::Zero {
                    logical_start,
                    sectors: extent.sectors,
                },
            };
            logical_start = logical_start
                .checked_add(extent.sectors)
                .ok_or_else(|| invalid_data("LP logical extent overflows"))?;
            targets.push(target);
        }
        let bytes = logical_start
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| invalid_data("LP logical partition size overflows"))?;
        if bytes == 0 || bytes % metadata.geometry.logical_block_size as u64 != 0 {
            return Err(invalid_data(format!(
                "LP partition {effective_name:?} has invalid logical size {bytes}"
            )));
        }

        plans.push(MappingPlan {
            base_name: base_name.to_string(),
            dm_name: format!("pocketboot-fw-{base_name}-{}", &suffix[1..]),
            dm_uuid: format!("POCKETBOOT-FW-{}-{base_name}", &suffix[1..]),
            targets,
        });
    }
    Ok(plans)
}

fn slot_suffix(slot: u32) -> io::Result<&'static str> {
    match slot {
        0 => Ok("_a"),
        1 => Ok("_b"),
        2 => Ok("_c"),
        _ => Err(invalid_data(format!("unsupported LP metadata slot {slot}"))),
    }
}

fn validate_mapping_component(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 36
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(invalid_data(format!(
            "invalid requested LP partition name {name:?}"
        )));
    }
    Ok(())
}

fn table_entry<'a>(
    tables: &'a [u8],
    descriptor: TableDescriptor,
    index: usize,
    name: &str,
) -> io::Result<&'a [u8]> {
    let relative = index
        .checked_mul(descriptor.entry_size)
        .and_then(|offset| descriptor.offset.checked_add(offset))
        .ok_or_else(|| invalid_data(format!("LP {name} entry offset overflows")))?;
    checked_slice(tables, relative, descriptor.entry_size, name)
}

fn fixed_name(
    bytes: &[u8],
    offset: usize,
    length: usize,
    allow_dash: bool,
    description: &str,
) -> io::Result<String> {
    let field = checked_slice(bytes, offset, length, description)?;
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if end == 0 || field[end..].iter().any(|byte| *byte != 0) {
        return Err(invalid_data(format!("invalid {description}")));
    }
    let name = &field[..end];
    let valid = name.iter().all(|byte| {
        byte.is_ascii_alphanumeric()
            || *byte == b'_'
            || (allow_dash && matches!(*byte, b'-' | b'.'))
    });
    if !valid {
        return Err(invalid_data(format!("invalid {description}")));
    }
    Ok(String::from_utf8(name.to_vec()).expect("validated ASCII name"))
}

fn verify_checksum(
    bytes: &[u8],
    zero_range: std::ops::Range<usize>,
    expected: &[u8],
    description: &str,
) -> io::Result<()> {
    if expected.len() != 32 || zero_range.end > bytes.len() {
        return Err(invalid_data(format!(
            "invalid LP {description} checksum field"
        )));
    }
    let mut hasher = Sha256::new();
    hasher.update(&bytes[..zero_range.start]);
    hasher.update([0u8; 32]);
    hasher.update(&bytes[zero_range.end..]);
    if hasher.finalize().as_slice() != expected {
        return Err(invalid_data(format!("LP {description} checksum mismatch")));
    }
    Ok(())
}

fn read_exact_at(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buffer.is_empty() {
        let count = file.read_at(buffer, offset)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "LP metadata device is truncated",
            ));
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| invalid_data("LP read offset overflows"))?;
        buffer = &mut buffer[count..];
    }
    Ok(())
}

fn checked_slice<'a>(
    bytes: &'a [u8],
    offset: usize,
    length: usize,
    description: &str,
) -> io::Result<&'a [u8]> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| invalid_data(format!("{description} range overflows")))?;
    bytes
        .get(offset..end)
        .ok_or_else(|| invalid_data(format!("{description} is truncated")))
}

fn le_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    let value = checked_slice(bytes, offset, 2, "u16")?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = checked_slice(bytes, offset, 4, "u32")?;
    Ok(u32::from_le_bytes(value.try_into().expect("four bytes")))
}

fn le_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
    let value = checked_slice(bytes, offset, 8, "u64")?;
    Ok(u64::from_le_bytes(value.try_into().expect("eight bytes")))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

const DM_CONTROL: &str = "/dev/mapper/control";
const DM_MAPPER_DIR: &str = "/dev/mapper";
const DM_NAME_LEN: usize = 128;
const DM_UUID_LEN: usize = 129;
const DM_TARGET_TYPE_LEN: usize = 16;
const DM_IOCTL_TYPE: u32 = 0xfd;
const DM_VERSION_CMD: u32 = 0;
const DM_DEV_CREATE_CMD: u32 = 3;
const DM_DEV_REMOVE_CMD: u32 = 4;
const DM_DEV_SUSPEND_CMD: u32 = 6;
const DM_DEV_STATUS_CMD: u32 = 7;
const DM_TABLE_LOAD_CMD: u32 = 9;
const DM_READONLY_FLAG: u32 = 1 << 0;
const DM_SUSPEND_FLAG: u32 = 1 << 1;
const DM_ACTIVE_PRESENT_FLAG: u32 = 1 << 5;

#[repr(C)]
#[derive(Clone, Copy)]
struct DmIoctl {
    version: [u32; 3],
    data_size: u32,
    data_start: u32,
    target_count: u32,
    open_count: i32,
    flags: u32,
    event_nr: u32,
    padding: u32,
    dev: u64,
    name: [u8; DM_NAME_LEN],
    uuid: [u8; DM_UUID_LEN],
    data: [u8; 7],
}

impl Default for DmIoctl {
    fn default() -> Self {
        Self {
            version: [4, 0, 0],
            data_size: std::mem::size_of::<Self>() as u32,
            data_start: std::mem::size_of::<Self>() as u32,
            target_count: 0,
            open_count: 0,
            flags: 0,
            event_nr: 0,
            padding: 0,
            dev: 0,
            name: [0; DM_NAME_LEN],
            uuid: [0; DM_UUID_LEN],
            data: [0; 7],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DmTargetSpec {
    sector_start: u64,
    length: u64,
    status: i32,
    next: u32,
    target_type: [u8; DM_TARGET_TYPE_LEN],
}

struct IoctlBuffer {
    words: Vec<u64>,
    len: usize,
}

impl IoctlBuffer {
    fn new(len: usize) -> io::Result<Self> {
        let words = len
            .checked_add(7)
            .ok_or_else(|| invalid_data("device-mapper ioctl buffer overflows"))?
            / 8;
        Ok(Self {
            words: vec![0; words],
            len,
        })
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast(), self.len) }
    }

    fn header(&self) -> DmIoctl {
        unsafe { self.words.as_ptr().cast::<DmIoctl>().read() }
    }

    fn set_header(&mut self, header: DmIoctl) {
        unsafe { self.words.as_mut_ptr().cast::<DmIoctl>().write(header) }
    }
}

struct DeviceMapper {
    control: File,
}

impl DeviceMapper {
    fn open() -> io::Result<Self> {
        let control = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(DM_CONTROL)?;
        Ok(Self { control })
    }

    fn check_version(&self) -> io::Result<()> {
        let response = self.command(DM_VERSION_CMD, DmIoctl::default(), &[])?;
        if response.version[0] != 4 {
            return Err(invalid_data(format!(
                "unsupported device-mapper ioctl version {}.{}.{}",
                response.version[0], response.version[1], response.version[2]
            )));
        }
        Ok(())
    }

    fn create_read_only(&self, plan: &MappingPlan) -> io::Result<Mapping> {
        if plan.dm_name.len() >= DM_NAME_LEN || plan.dm_uuid.len() >= DM_UUID_LEN {
            return Err(invalid_data("device-mapper name or UUID is too long"));
        }
        let path = PathBuf::from(DM_MAPPER_DIR).join(&plan.dm_name);
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("device-mapper node {} already exists", path.display()),
            ));
        }

        let mut create = DmIoctl::default();
        set_c_field(&mut create.name, &plan.dm_name)?;
        set_c_field(&mut create.uuid, &plan.dm_uuid)?;
        let created = self.command(DM_DEV_CREATE_CMD, create, &[])?;

        let result = (|| {
            let table_data = encode_targets(&plan.targets)?;
            let mut load = DmIoctl::default();
            set_c_field(&mut load.uuid, &plan.dm_uuid)?;
            load.flags = DM_READONLY_FLAG;
            load.target_count = plan.targets.len() as u32;
            self.command(DM_TABLE_LOAD_CMD, load, &table_data)?;

            let mut resume = DmIoctl::default();
            set_c_field(&mut resume.uuid, &plan.dm_uuid)?;
            resume.flags = DM_READONLY_FLAG;
            self.command(DM_DEV_SUSPEND_CMD, resume, &[])?;

            let mut status = DmIoctl::default();
            set_c_field(&mut status.uuid, &plan.dm_uuid)?;
            let status = self.command(DM_DEV_STATUS_CMD, status, &[])?;
            if status.flags & DM_ACTIVE_PRESENT_FLAG == 0
                || status.flags & DM_READONLY_FLAG == 0
                || status.flags & DM_SUSPEND_FLAG != 0
            {
                return Err(invalid_data(format!(
                    "device-mapper mapping {:?} did not become active and read-only",
                    plan.dm_name
                )));
            }
            if status.dev != created.dev && created.dev != 0 {
                return Err(invalid_data("device-mapper device number changed"));
            }
            create_block_node(&path, status.dev)?;
            Ok(Mapping {
                uuid: plan.dm_uuid.clone(),
                path,
            })
        })();

        if result.is_err() {
            let _ = self.remove(&plan.dm_uuid);
        }
        result
    }

    fn remove(&self, uuid: &str) -> io::Result<()> {
        let mut request = DmIoctl::default();
        set_c_field(&mut request.uuid, uuid)?;
        self.command(DM_DEV_REMOVE_CMD, request, &[]).map(|_| ())
    }

    fn command(&self, command: u32, mut header: DmIoctl, data: &[u8]) -> io::Result<DmIoctl> {
        let header_size = std::mem::size_of::<DmIoctl>();
        let total = header_size
            .checked_add(data.len())
            .ok_or_else(|| invalid_data("device-mapper request size overflows"))?;
        if total > u32::MAX as usize {
            return Err(invalid_data("device-mapper request is too large"));
        }
        header.data_size = total as u32;
        header.data_start = header_size as u32;
        let mut buffer = IoctlBuffer::new(total)?;
        buffer.set_header(header);
        buffer.bytes_mut()[header_size..].copy_from_slice(data);
        dm_ioctl(self.control.as_raw_fd(), command, &mut buffer)?;
        Ok(buffer.header())
    }
}

fn dm_ioctl(fd: RawFd, command: u32, buffer: &mut IoctlBuffer) -> io::Result<()> {
    let request = ioctl_readwrite(
        DM_IOCTL_TYPE,
        command,
        std::mem::size_of::<DmIoctl>() as u32,
    );
    let result =
        unsafe { libc::ioctl(fd, request as libc::Ioctl, buffer.bytes_mut().as_mut_ptr()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

const fn ioctl_readwrite(kind: u32, command: u32, size: u32) -> u32 {
    (3 << 30) | (size << 16) | (kind << 8) | command
}

fn encode_targets(targets: &[DmTarget]) -> io::Result<Vec<u8>> {
    if targets.is_empty() {
        return Err(invalid_data("device-mapper table has no targets"));
    }
    let mut records = Vec::with_capacity(targets.len());
    for target in targets {
        let (logical_start, sectors, target_type, parameters) = match target {
            DmTarget::Linear {
                logical_start,
                sectors,
                devnum,
                physical_start,
            } => (
                *logical_start,
                *sectors,
                "linear",
                format!("{devnum} {physical_start}"),
            ),
            DmTarget::Zero {
                logical_start,
                sectors,
            } => (*logical_start, *sectors, "zero", String::new()),
        };
        let raw_len = std::mem::size_of::<DmTargetSpec>()
            .checked_add(parameters.len())
            .and_then(|length| length.checked_add(1))
            .ok_or_else(|| invalid_data("device-mapper target size overflows"))?;
        let record_len = align_to_8(raw_len)?;
        records.push((logical_start, sectors, target_type, parameters, record_len));
    }

    let total = records.iter().try_fold(0usize, |total, record| {
        total
            .checked_add(record.4)
            .ok_or_else(|| invalid_data("device-mapper table size overflows"))
    })?;
    let mut output = vec![0u8; total];
    let mut offset = 0;
    for (logical_start, sectors, target_type, parameters, record_len) in records {
        let mut spec = DmTargetSpec {
            sector_start: logical_start,
            length: sectors,
            status: 0,
            next: record_len as u32,
            target_type: [0; DM_TARGET_TYPE_LEN],
        };
        set_c_field(&mut spec.target_type, target_type)?;
        unsafe {
            output
                .as_mut_ptr()
                .add(offset)
                .cast::<DmTargetSpec>()
                .write_unaligned(spec);
        }
        let params_start = offset + std::mem::size_of::<DmTargetSpec>();
        output[params_start..params_start + parameters.len()]
            .copy_from_slice(parameters.as_bytes());
        offset += record_len;
    }
    Ok(output)
}

fn align_to_8(value: usize) -> io::Result<usize> {
    value
        .checked_add(7)
        .map(|value| value & !7)
        .ok_or_else(|| invalid_data("alignment overflows"))
}

fn set_c_field<const N: usize>(field: &mut [u8; N], value: &str) -> io::Result<()> {
    if value.as_bytes().contains(&0) || value.len() >= N {
        return Err(invalid_data("invalid device-mapper string"));
    }
    field.fill(0);
    field[..value.len()].copy_from_slice(value.as_bytes());
    Ok(())
}

fn create_block_node(path: &Path, encoded_dev: u64) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| invalid_data("device-mapper path contains NUL"))?;
    let mode = libc::S_IFBLK | 0o600;
    let result = unsafe { libc::mknod(path.as_ptr(), mode, encoded_dev as libc::dev_t) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const METADATA_MAX: usize = 4096;
    const SLOT_COUNT: u32 = 2;

    #[test]
    fn metadata_offsets_group_all_primaries_before_backups() {
        let geometry = Geometry {
            metadata_max_size: METADATA_MAX,
            metadata_slot_count: SLOT_COUNT,
            logical_block_size: 4096,
        };
        assert_eq!(
            metadata_offset(&geometry, 0, MetadataCopy::Primary).unwrap(),
            12288
        );
        assert_eq!(
            metadata_offset(&geometry, 1, MetadataCopy::Primary).unwrap(),
            16384
        );
        assert_eq!(
            metadata_offset(&geometry, 0, MetadataCopy::Backup).unwrap(),
            20480
        );
        assert_eq!(
            metadata_offset(&geometry, 1, MetadataCopy::Backup).unwrap(),
            24576
        );
    }

    #[test]
    fn slot_one_falls_back_to_its_matching_backup() {
        let temp = TempImage::new();
        let mut image = fixture_image();
        let slot_one_primary = 12288 + METADATA_MAX;
        image[slot_one_primary..slot_one_primary + METADATA_MAX].fill(0xa5);
        fs::write(&temp.path, image).unwrap();
        let metadata = read_metadata(&temp.partition(), 1).unwrap();
        assert_eq!(metadata.copy, MetadataCopy::Backup);
        assert_eq!(metadata.partitions[0].name, "vendor");
    }

    #[test]
    fn corrupt_tables_checksum_is_rejected() {
        let geometry = fixture_geometry();
        let mut metadata = fixture_metadata();
        metadata[HEADER_V1_2_SIZE + 3] ^= 0xff;
        let err = parse_metadata(&metadata, geometry, MetadataCopy::Primary).unwrap_err();
        assert!(err.to_string().contains("tables checksum"));
    }

    #[test]
    fn maps_only_current_slot_requested_partition() {
        let metadata = parse_metadata(
            &fixture_metadata(),
            fixture_geometry(),
            MetadataCopy::Primary,
        )
        .unwrap();
        let backing = Partition {
            kernel_name: "mmcblk0p69".into(),
            partname: Some("system_b".into()),
            dev_path: PathBuf::from("/dev/mmcblk0p69"),
            devnum: "179:69".into(),
            size_bytes: 1024 * 1024,
            read_only: false,
        };
        let plans = mapping_plans(&metadata, &[backing], 1, &["vendor".into()]).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].base_name, "vendor");
        assert_eq!(plans[0].dm_name, "pocketboot-fw-vendor-b");
        match &plans[0].targets[0] {
            DmTarget::Linear {
                devnum,
                physical_start,
                sectors,
                ..
            } => {
                assert_eq!(devnum, "179:69");
                assert_eq!(*physical_start, 40);
                assert_eq!(*sectors, 8);
            }
            target => panic!("unexpected target: {target:?}"),
        }
    }

    #[test]
    fn serializes_aligned_read_only_table_targets() {
        let data = encode_targets(&[
            DmTarget::Linear {
                logical_start: 0,
                sectors: 8,
                devnum: "179:69".into(),
                physical_start: 40,
            },
            DmTarget::Zero {
                logical_start: 8,
                sectors: 8,
            },
        ])
        .unwrap();
        assert_eq!(data.len() % 8, 0);
        let first = unsafe { data.as_ptr().cast::<DmTargetSpec>().read_unaligned() };
        assert_eq!(first.sector_start, 0);
        assert_eq!(first.length, 8);
        assert_eq!(first.next as usize % 8, 0);
        let second = unsafe {
            data.as_ptr()
                .add(first.next as usize)
                .cast::<DmTargetSpec>()
                .read_unaligned()
        };
        assert_eq!(second.sector_start, 8);
        assert_eq!(second.next as usize, data.len() - first.next as usize);
    }

    #[test]
    fn device_mapper_abi_matches_kernel_uapi() {
        assert_eq!(std::mem::size_of::<DmIoctl>(), 312);
        assert_eq!(std::mem::size_of::<DmTargetSpec>(), 40);
        assert_eq!(
            ioctl_readwrite(DM_IOCTL_TYPE, DM_VERSION_CMD, 312),
            0xc138_fd00
        );
        assert_eq!(
            ioctl_readwrite(DM_IOCTL_TYPE, DM_TABLE_LOAD_CMD, 312),
            0xc138_fd09
        );
    }

    fn fixture_geometry() -> Geometry {
        Geometry {
            metadata_max_size: METADATA_MAX,
            metadata_slot_count: SLOT_COUNT,
            logical_block_size: 4096,
        }
    }

    fn fixture_image() -> Vec<u8> {
        let mut image = vec![0u8; 1024 * 1024];
        let geometry = geometry_block();
        image[4096..8192].copy_from_slice(&geometry);
        image[8192..12288].copy_from_slice(&geometry);
        let metadata = fixture_metadata();
        for offset in [12288, 16384, 20480, 24576] {
            image[offset..offset + METADATA_MAX].copy_from_slice(&metadata);
        }
        image
    }

    fn geometry_block() -> Vec<u8> {
        let mut block = vec![0u8; GEOMETRY_SIZE];
        put_u32(&mut block, 0, GEOMETRY_MAGIC);
        put_u32(&mut block, 4, GEOMETRY_STRUCT_SIZE as u32);
        put_u32(&mut block, 40, METADATA_MAX as u32);
        put_u32(&mut block, 44, SLOT_COUNT);
        put_u32(&mut block, 48, 4096);
        fill_checksum(&mut block[..GEOMETRY_STRUCT_SIZE], 8..40);
        block
    }

    fn fixture_metadata() -> Vec<u8> {
        let mut partition = vec![0u8; PARTITION_ENTRY_SIZE];
        put_name(&mut partition, 0, 36, "vendor");
        put_u32(
            &mut partition,
            36,
            PARTITION_ATTR_READONLY | PARTITION_ATTR_SLOT_SUFFIXED,
        );
        put_u32(&mut partition, 40, 0);
        put_u32(&mut partition, 44, 1);
        put_u32(&mut partition, 48, 0);

        let mut extent = vec![0u8; EXTENT_ENTRY_SIZE];
        put_u64(&mut extent, 0, 8);
        put_u32(&mut extent, 8, TARGET_LINEAR);
        put_u64(&mut extent, 12, 40);
        put_u32(&mut extent, 20, 0);

        let mut group = vec![0u8; GROUP_ENTRY_SIZE];
        put_name(&mut group, 0, 36, "default");

        let mut block = vec![0u8; BLOCK_DEVICE_ENTRY_SIZE];
        put_u64(&mut block, 0, 40);
        put_u32(&mut block, 8, 4096);
        put_u64(&mut block, 16, 1024 * 1024);
        put_name(&mut block, 24, 36, "system");
        put_u32(&mut block, 60, BLOCK_DEVICE_SLOT_SUFFIXED);

        let tables = [partition, extent, group, block].concat();
        let mut metadata = vec![0u8; METADATA_MAX];
        put_u32(&mut metadata, 0, HEADER_MAGIC);
        put_u16(&mut metadata, 4, HEADER_MAJOR);
        put_u16(&mut metadata, 6, 2);
        put_u32(&mut metadata, 8, HEADER_V1_2_SIZE as u32);
        put_u32(&mut metadata, 44, tables.len() as u32);
        metadata[48..80].copy_from_slice(&Sha256::digest(&tables));
        put_descriptor(&mut metadata, 80, 0, 1, PARTITION_ENTRY_SIZE);
        put_descriptor(
            &mut metadata,
            92,
            PARTITION_ENTRY_SIZE,
            1,
            EXTENT_ENTRY_SIZE,
        );
        put_descriptor(
            &mut metadata,
            104,
            PARTITION_ENTRY_SIZE + EXTENT_ENTRY_SIZE,
            1,
            GROUP_ENTRY_SIZE,
        );
        put_descriptor(
            &mut metadata,
            116,
            PARTITION_ENTRY_SIZE + EXTENT_ENTRY_SIZE + GROUP_ENTRY_SIZE,
            1,
            BLOCK_DEVICE_ENTRY_SIZE,
        );
        metadata[HEADER_V1_2_SIZE..HEADER_V1_2_SIZE + tables.len()].copy_from_slice(&tables);
        fill_checksum(&mut metadata[..HEADER_V1_2_SIZE], 12..44);
        metadata
    }

    fn put_descriptor(
        bytes: &mut [u8],
        offset: usize,
        table_offset: usize,
        entries: usize,
        entry_size: usize,
    ) {
        put_u32(bytes, offset, table_offset as u32);
        put_u32(bytes, offset + 4, entries as u32);
        put_u32(bytes, offset + 8, entry_size as u32);
    }

    fn fill_checksum(bytes: &mut [u8], range: std::ops::Range<usize>) {
        bytes[range.clone()].fill(0);
        let digest = Sha256::digest(&*bytes);
        bytes[range].copy_from_slice(&digest);
    }

    fn put_name(bytes: &mut [u8], offset: usize, length: usize, name: &str) {
        assert!(name.len() < length);
        bytes[offset..offset + length].fill(0);
        bytes[offset..offset + name.len()].copy_from_slice(name.as_bytes());
    }

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    struct TempImage {
        path: PathBuf,
    }

    impl TempImage {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Self {
                path: std::env::temp_dir().join(format!(
                    "pocketboot-lp-test-{}-{nonce}.img",
                    std::process::id()
                )),
            }
        }

        fn partition(&self) -> Partition {
            Partition {
                kernel_name: "mmcblk0p69".into(),
                partname: Some("system_b".into()),
                dev_path: self.path.clone(),
                devnum: "179:69".into(),
                size_bytes: 1024 * 1024,
                read_only: false,
            }
        }
    }

    impl Drop for TempImage {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
