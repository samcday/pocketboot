//! Physical RAM placement shared by the native kexec loaders.

use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PhysRange {
    pub(super) start: u64,
    pub(super) end: u64,
}

impl PhysRange {
    pub(super) fn new(start: u64, end: u64) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    pub(super) fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

pub(super) fn parse_iomem(iomem: &str) -> Vec<PhysRange> {
    let mut ranges = Vec::new();
    for line in iomem.lines() {
        let Some((range, name)) = parse_iomem_line(line) else {
            continue;
        };
        match name {
            "System RAM" => add_range(&mut ranges, range),
            // kexec relocates these pages; they are not permanent reservations.
            "Kernel code" | "Kernel data" | "Kernel bss" => {}
            _ => subtract_range(&mut ranges, range),
        }
    }
    ranges.sort_by_key(|range| range.start);
    ranges
}

pub(super) fn parse_iomem_line(line: &str) -> Option<(PhysRange, &str)> {
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

pub(super) fn subtract_range(ranges: &mut Vec<PhysRange>, remove: PhysRange) {
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

pub(super) fn find_region(
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

pub(super) fn align_up(value: u64, align: u64) -> Option<u64> {
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

pub(super) fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

pub(super) fn checked_add(left: u64, right: u64) -> io::Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "physical address overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iomem_keeps_relocatable_kernel_pages_but_excludes_reservations() {
        assert_eq!(
            parse_iomem(
                "40000000-4fffffff : System RAM\n\
                   40008000-407fffff : Kernel code\n\
                   40800000-408fffff : Kernel data\n\
                   41000000-41000fff : reserved\n"
            ),
            [
                PhysRange {
                    start: 0x40000000,
                    end: 0x41000000
                },
                PhysRange {
                    start: 0x41001000,
                    end: 0x50000000
                },
            ]
        );
    }

    #[test]
    fn placement_aligns_and_skips_occupied_ranges() {
        let ram = [PhysRange {
            start: 1,
            end: 0x10000,
        }];
        let used = [PhysRange {
            start: 0x2000,
            end: 0x4001,
        }];
        assert_eq!(
            find_region(&ram, &used, 0x3000, 0x1000, 0, 0x10000),
            Some(0x5000)
        );
        assert_eq!(find_region(&ram, &used, 0x3000, 0x1000, 0, 0x7000), None);
        assert_eq!(find_region(&ram, &[], 0, 0x1000, 0, u64::MAX), None);
    }

    #[test]
    fn malformed_or_overflowing_ranges_are_not_ram() {
        assert!(parse_iomem_line("ffffffffffffffff-ffffffffffffffff : System RAM").is_none());
        assert!(parse_iomem_line("2000-1000 : System RAM").is_none());
        assert!(parse_iomem_line("not an address : System RAM").is_none());
        assert!(align_up(u64::MAX, 4096).is_none());
    }
}
