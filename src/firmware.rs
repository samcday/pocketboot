use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use blob_wrangler::{ExtractOptions, PartitionResolver, ResolvedPartition};

use crate::{
    ab_slots::{Slot, Slots},
    android_lp,
    cmdline::KernelCommandLine,
    partitions, runtime, settle,
};

const FDT_COMPATIBLE_PATH: &str = "/sys/firmware/devicetree/base/compatible";
const KERNEL_RELEASE_PATH: &str = "/proc/sys/kernel/osrelease";
const EXTRACT_PATH: &str = "/lib/firmware/updates";
const MOUNTS_DIR: &str = "/run/pocketboot/firmware-mounts";
const STORAGE_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) type LoadResult = Result<LoadReport, String>;

#[derive(Debug)]
pub(crate) enum LoadReport {
    Extracted {
        compatible: String,
        files: usize,
        directories: usize,
        missing: usize,
    },
    Unsupported {
        compatibles: Vec<String>,
    },
}

pub(crate) fn spawn(cmdline: KernelCommandLine) -> async_executor::Task<LoadResult> {
    runtime::spawn(async move { runtime::unblock(move || load(cmdline)).await })
}

pub(crate) fn log_completion(result: &LoadResult) {
    match result {
        Ok(LoadReport::Extracted {
            compatible,
            files,
            directories,
            missing,
        }) => tracing::info!(
            compatible = %compatible,
            files,
            directories,
            missing,
            path = EXTRACT_PATH,
            "firmware extraction complete"
        ),
        Ok(LoadReport::Unsupported { compatibles }) => tracing::warn!(
            compatibles = %compatibles.join(","),
            "no bundled firmware configuration matches this device"
        ),
        Err(err) => tracing::warn!(error = %err, "firmware extraction failed"),
    }
}

fn load(cmdline: KernelCommandLine) -> LoadResult {
    let settled = settle::wait_for_local_flash(STORAGE_SETTLE_TIMEOUT);
    if settled.timed_out {
        tracing::warn!(
            elapsed_ms = settled.elapsed.as_millis(),
            disks = settled.disks,
            partitions = settled.partitions,
            snapshot = %settled.summary,
            "firmware storage settle timed out"
        );
    } else {
        tracing::info!(
            elapsed_ms = settled.elapsed.as_millis(),
            disks = settled.disks,
            partitions = settled.partitions,
            snapshot = %settled.summary,
            "firmware storage settled"
        );
    }

    let compatibles = read_fdt_strings(FDT_COMPATIBLE_PATH)
        .map_err(|err| format!("read device compatibles: {err}"))?;
    let Some(bundled) = blob_wrangler::bundled_config(&compatibles)
        .map_err(|err| format!("parse bundled firmware configuration: {err}"))?
    else {
        return Ok(LoadReport::Unsupported { compatibles });
    };

    let current_slot = match Slots::new(cmdline).current_slot() {
        Ok(slot) => slot,
        Err(err) => {
            tracing::warn!(error = ?err, "failed to determine slot for firmware extraction");
            None
        }
    };
    let mappings = map_dynamic_partitions(&bundled.config, current_slot)
        .map_err(|err| format!("map Android dynamic partitions: {err}"))?;
    let options = ExtractOptions {
        extract_path: PathBuf::from(EXTRACT_PATH),
        mounts_dir: PathBuf::from(MOUNTS_DIR),
        running_kernel_release: read_trimmed(KERNEL_RELEASE_PATH),
    };

    let extraction = {
        let resolver = PocketbootResolver {
            current_slot,
            mappings: mappings.as_ref(),
        };
        blob_wrangler::extract(&bundled.config, &resolver, &options)
    };

    if let Some(mappings) = mappings
        && let Err(err) = mappings.cleanup()
    {
        tracing::warn!(error = ?err, "failed to clean up Android dynamic partition mappings");
    }

    let report = extraction.map_err(|err| format!("extract firmware: {err}"))?;
    Ok(LoadReport::Extracted {
        compatible: bundled.compatible.to_string(),
        files: report.files.len(),
        directories: report.directories.len(),
        missing: report.missing.len(),
    })
}

fn map_dynamic_partitions(
    config: &blob_wrangler::Config,
    current_slot: Option<Slot>,
) -> io::Result<Option<android_lp::MappedPartitions>> {
    let Some(metadata_partition) = config.dynpart() else {
        return Ok(None);
    };

    let current_slot = current_slot.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "current A/B slot is required for dynamic firmware partitions",
        )
    })?;
    let metadata_name = format!("{metadata_partition}{}", current_slot.suffix());
    let metadata = partitions::find_optional(&metadata_name)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("dynamic partition metadata {metadata_name:?} not found"),
        )
    })?;
    let metadata_slot = slot_index(current_slot);
    let requested = config
        .referenced_partitions()
        .map(partition_base)
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    tracing::info!(
        partition = metadata.name(),
        path = %metadata.dev_path.display(),
        metadata_slot,
        requested = requested.len(),
        "mapping Android dynamic partitions"
    );
    android_lp::map_read_only(&metadata, metadata_slot, &requested).map(Some)
}

struct PocketbootResolver<'a> {
    current_slot: Option<Slot>,
    mappings: Option<&'a android_lp::MappedPartitions>,
}

impl PartitionResolver for PocketbootResolver<'_> {
    fn resolve(&self, partition: &str) -> io::Result<Option<ResolvedPartition>> {
        if partition_slot_name(partition)
            .zip(self.current_slot)
            .is_some_and(|(requested, current)| requested != current)
        {
            tracing::warn!(
                partition,
                current_slot = self.current_slot.map(Slot::name),
                "refusing to resolve firmware from the inactive slot"
            );
            return Ok(None);
        }

        if let Some(path) = self
            .mappings
            .and_then(|mappings| mappings.path_for(partition_base(partition)))
        {
            return Ok(Some(ResolvedPartition::BlockDevice(path.to_path_buf())));
        }

        Ok(find_physical_partition(partition, self.current_slot)?
            .map(|partition| ResolvedPartition::BlockDevice(partition.dev_path)))
    }
}

fn find_physical_partition(
    name: &str,
    current_slot: Option<Slot>,
) -> io::Result<Option<partitions::Partition>> {
    for candidate in partition_candidates(name, current_slot) {
        if let Some(partition) = partitions::find_optional(&candidate)? {
            return Ok(Some(partition));
        }
    }
    Ok(None)
}

fn partition_candidates(name: &str, current_slot: Option<Slot>) -> Vec<String> {
    if let Some(requested_slot) = partition_slot_name(name) {
        if current_slot.is_some_and(|current_slot| current_slot != requested_slot) {
            return Vec::new();
        }
        return vec![name.to_string()];
    }

    match current_slot {
        Some(slot) => vec![format!("{name}{}", slot.suffix()), name.to_string()],
        None => vec![name.to_string()],
    }
}

fn partition_slot_name(name: &str) -> Option<Slot> {
    name.strip_suffix("_a")
        .map(|_| Slot::A)
        .or_else(|| name.strip_suffix("_b").map(|_| Slot::B))
}

fn partition_base(name: &str) -> &str {
    name.strip_suffix("_a")
        .or_else(|| name.strip_suffix("_b"))
        .unwrap_or(name)
}

fn slot_index(slot: Slot) -> u32 {
    match slot {
        Slot::A => 0,
        Slot::B => 1,
    }
}

fn read_fdt_strings(path: impl AsRef<Path>) -> io::Result<Vec<String>> {
    let bytes = fs::read(path)?;
    let values = bytes
        .split(|byte| *byte == b'\0')
        .filter(|value| !value.is_empty())
        .map(|value| {
            std::str::from_utf8(value)
                .map(str::trim)
                .map(str::to_string)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if values.is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "device compatible property is empty",
        ))
    } else {
        Ok(values)
    }
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_slot_is_preferred_before_shared_partition() {
        assert_eq!(
            partition_candidates("vendor", Some(Slot::B)),
            ["vendor_b", "vendor"]
        );
    }

    #[test]
    fn explicitly_slotted_partition_is_not_rewritten() {
        assert_eq!(
            partition_candidates("vendor_a", Some(Slot::A)),
            ["vendor_a"]
        );
    }

    #[test]
    fn inactive_explicit_slot_has_no_physical_candidate() {
        assert!(partition_candidates("vendor_a", Some(Slot::B)).is_empty());
    }

    #[test]
    fn resolver_rejects_inactive_explicit_slot_before_mapping_lookup() {
        let resolver = PocketbootResolver {
            current_slot: Some(Slot::B),
            mappings: None,
        };

        assert_eq!(resolver.resolve("vendor_a").unwrap(), None);
    }

    #[test]
    fn parses_ordered_fdt_compatible_strings() {
        let path = std::env::temp_dir().join(format!(
            "pocketboot-firmware-compatible-{}",
            std::process::id()
        ));
        fs::write(&path, b"google,sargo\0qcom,sdm670\0").unwrap();

        let values = read_fdt_strings(&path).unwrap();
        let _ = fs::remove_file(path);

        assert_eq!(values, ["google,sargo", "qcom,sdm670"]);
    }
}
