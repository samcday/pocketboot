//! `physread ADDRESS LENGTH`: dump a physical address range through `/dev/mem`.
//!
//! The tool maps only the one page-aligned range that contains the request, with
//! `PROT_READ | MAP_SHARED`, reads exactly the requested 32-bit words with
//! volatile loads and writes their bytes to stdout. It never writes memory,
//! never reads outside the request and has no default address. Diagnostics and
//! errors go to stderr only, so stdout stays a clean byte stream.
//!
//! Linux only, and only where the kernel exposes the range through `/dev/mem`
//! (`CONFIG_DEVMEM`); see `README.md` for access-policy constraints. Read-only
//! does not mean harmless: an inaccessible physical address may fault, hang or
//! reset the device. This tool deliberately installs no recovery signal handler.

use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{self, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    process::ExitCode,
    ptr,
};

/// Physical memory device; see `README.md` for the kernel options it needs.
const DEV_MEM: &str = "/dev/mem";
/// Physical ranges are read as aligned 32-bit words.
const WORD_SIZE: u64 = 4;
/// Exit status for a bad command line; runtime failures use `ExitCode::FAILURE`.
const USAGE_EXIT: u8 = 2;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(message)) => {
            eprintln!("physread: {message}");
            eprintln!("usage: physread ADDRESS LENGTH");
            ExitCode::from(USAGE_EXIT)
        }
        Err(Error::Runtime(message)) => {
            eprintln!("physread: {message}");
            ExitCode::FAILURE
        }
    }
}

/// What went wrong, split only so the exit status separates a mistyped command
/// line (2) from `/dev/mem` and stdout failures (1).
#[derive(Debug)]
enum Error {
    Usage(String),
    Runtime(String),
}

/// Parse the command line, plan the one mapping, then read and write.
fn run() -> Result<(), Error> {
    let arguments = parse_arguments(std::env::args_os().skip(1).collect())?;
    let plan = Plan::new(arguments.address, arguments.length, page_size()?)?;
    dump(plan)
}

/// The two command-line operands, already parsed.
struct Arguments {
    address: u64,
    length: u64,
}

/// Parse `ADDRESS LENGTH`, the only accepted command line.
fn parse_arguments(raw: Vec<OsString>) -> Result<Arguments, Error> {
    let mut texts = Vec::with_capacity(raw.len());
    for argument in raw {
        texts.push(
            argument
                .into_string()
                .map_err(|_| Error::Usage("argument is not valid UTF-8".to_string()))?,
        );
    }
    let [address, length] = texts.as_slice() else {
        return Err(Error::Usage(format!(
            "expected ADDRESS LENGTH, got {} argument(s)",
            texts.len()
        )));
    };
    Ok(Arguments {
        address: parse_u64(address)?,
        length: parse_u64(length)?,
    })
}

/// Parse a non-negative integer: a `0x`/`0X` prefix selects hexadecimal and
/// everything else is decimal, so a bare number means the bytes it looks like.
fn parse_u64(text: &str) -> Result<u64, Error> {
    let (digits, radix) = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(digits) => (digits, 16),
        None => (text, 10),
    };
    if digits.is_empty() || digits.starts_with('+') {
        return Err(Error::Usage(format!("`{text}` requires unsigned digits")));
    }
    u64::from_str_radix(digits, radix)
        .map_err(|error| Error::Usage(format!("`{text}` is not a valid number: {error}")))
}

/// The single `mmap` a request needs, and the words to read from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    /// Page-aligned start of the page containing `address`; the `mmap` offset.
    offset: u64,
    /// Bytes between `offset` and `address`, where the first word starts.
    delta: usize,
    /// Bytes to map: `delta + length`, so page slack is the only excess.
    map_length: usize,
    /// Requested 32-bit words, in address order.
    words: usize,
}

impl Plan {
    /// Check a request and compute the mapping it needs. `page_size` is the
    /// host's `sysconf(_SC_PAGESIZE)`.
    fn new(address: u64, length: u64, page_size: u64) -> Result<Self, Error> {
        if !address.is_multiple_of(WORD_SIZE) {
            return Err(Error::Usage(format!(
                "address {address:#x} is not {WORD_SIZE}-byte aligned"
            )));
        }
        if length == 0 {
            return Err(Error::Usage("length is zero".to_string()));
        }
        if !length.is_multiple_of(WORD_SIZE) {
            return Err(Error::Usage(format!(
                "length {length:#x} is not a multiple of {WORD_SIZE}"
            )));
        }
        if address.checked_add(length).is_none() {
            return Err(Error::Usage(format!(
                "address {address:#x} + length {length:#x} overflows 64 bits"
            )));
        }
        // The page-aligned offset (and so `delta`) must remain word-aligned.
        if page_size == 0 || !page_size.is_multiple_of(WORD_SIZE) {
            return Err(Error::Runtime(format!(
                "page size {page_size} cannot carry {WORD_SIZE}-byte aligned reads"
            )));
        }
        let offset = address / page_size * page_size;
        if offset > libc::off_t::MAX as u64 {
            return Err(Error::Usage(format!(
                "address {address:#x} is beyond the mmap offset range"
            )));
        }
        let delta = usize::try_from(address - offset).map_err(|_| {
            Error::Usage(format!(
                "page offset of address {address:#x} does not fit this host"
            ))
        })?;
        let length = usize::try_from(length)
            .map_err(|_| Error::Usage(format!("length {length:#x} does not fit this host")))?;
        // `mmap` rounds the length up to whole pages internally; ask it for no
        // more than the request needs.
        let map_length = delta
            .checked_add(length)
            .ok_or_else(|| Error::Usage("mapping length overflows this host".to_string()))?;
        Ok(Plan {
            offset,
            delta,
            map_length,
            words: length / WORD_SIZE as usize,
        })
    }
}

/// A live, read-only mapping of the range a `Plan` requested. Unmapped on drop,
/// including on every error path.
struct Mapping {
    base: *mut libc::c_void,
    plan: Plan,
}

impl Mapping {
    /// Map the planned range from `file` with `PROT_READ | MAP_SHARED`.
    fn map(file: &File, plan: Plan) -> Result<Self, Error> {
        // SAFETY: the descriptor is live, the offset is page-aligned and fits
        // off_t, and the length is nonzero. A successful virtual mapping does
        // not guarantee that the underlying physical addresses can be read.
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                plan.map_length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                plan.offset as libc::off_t,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(Error::Runtime(format!(
                "mmap {:#x} bytes at {:#x}: {}",
                plan.map_length,
                plan.offset,
                io::Error::last_os_error()
            )));
        }
        Ok(Mapping { base, plan })
    }

    /// Read word `index` once. Physical accessibility is a platform assumption,
    /// not something that a successful mmap can establish.
    fn word(&self, index: usize) -> u32 {
        assert!(index < self.plan.words);
        // SAFETY: `Plan` bounds the request to `plan.words` 4-byte words inside
        // the `plan.map_length` mapped bytes at `base`, and `delta` is 4-byte
        // aligned. Wrapping pointer arithmetic does not assume this external
        // mapping is a Rust allocation. The user must select readable memory;
        // a hardware fault cannot be made safe by this helper.
        unsafe {
            ptr::read_volatile(
                self.base
                    .cast::<u8>()
                    .wrapping_add(self.plan.delta)
                    .cast::<u32>()
                    .wrapping_add(index),
            )
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `base` and `plan.map_length` are the successful `mmap` result
        // and its length, still mapped because nothing else unmapped them.
        if unsafe { libc::munmap(self.base, self.plan.map_length) } != 0 {
            // The requested bytes were already written, so report this without
            // changing the exit status.
            eprintln!(
                "physread: munmap {:#x} bytes at {:p}: {}",
                self.plan.map_length,
                self.base,
                io::Error::last_os_error()
            );
        }
    }
}

/// Map `plan` in `/dev/mem`, read its words and write the bytes to stdout.
fn dump(plan: Plan) -> Result<(), Error> {
    let file = OpenOptions::new()
        .read(true)
        // Use the kernel's O_SYNC mapping attributes without requesting writes.
        .custom_flags(libc::O_SYNC)
        .open(DEV_MEM)
        .map_err(|error| Error::Runtime(format!("open {DEV_MEM}: {error}")))?;
    let mapping = Mapping::map(&file, plan)?;
    // `mmap` keeps the range mapped after the descriptor closes, so `/dev/mem`
    // is closed before the potentially large write below.
    drop(file);
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    for index in 0..plan.words {
        let word = mapping.word(index);
        // Preserve memory byte order rather than interpreting the words.
        stdout
            .write_all(&word.to_ne_bytes())
            .map_err(|error| Error::Runtime(format!("write stdout: {error}")))?;
    }
    stdout
        .flush()
        .map_err(|error| Error::Runtime(format!("write stdout: {error}")))
}

/// The host's `mmap` alignment, asked for rather than assumed: a 64 KiB-page
/// kernel only accepts page offsets of its own choosing.
fn page_size() -> Result<u64, Error> {
    // SAFETY: `sysconf` has no preconditions; `_SC_PAGESIZE` returns the page
    // size or -1, which `try_from` rejects.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).map_err(|_| Error::Runtime("sysconf(_SC_PAGESIZE) failed".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    // Only arithmetic and a private memfd are exercised, never `/dev/mem`.

    const PAGE: u64 = 4096;

    #[test]
    fn parses_decimal_and_prefixed_hexadecimal() {
        assert_eq!(parse_u64("0").unwrap(), 0);
        assert_eq!(parse_u64("16").unwrap(), 16);
        assert_eq!(parse_u64("0x10").unwrap(), 16);
        assert_eq!(parse_u64("0X10").unwrap(), 16);
        assert_eq!(parse_u64("0xdeadBEEF").unwrap(), 0xdead_beef);
        assert_eq!(parse_u64("18446744073709551615").unwrap(), u64::MAX);
        assert_eq!(parse_u64("0xffffffffffffffff").unwrap(), u64::MAX);
    }

    #[test]
    fn rejects_numbers_it_cannot_represent_exactly() {
        for text in [
            "",
            "0x",
            "0X",
            "-1",
            "+",
            "+16",
            "0x+10",
            "0X+10",
            "12x",
            "0b101",
            "1_000",
            "1e9",
            "0xgg",
            " 1",
            "18446744073709551616",
            "0x10000000000000000",
        ] {
            assert!(parse_u64(text).is_err(), "`{text}` should be rejected");
        }
    }

    #[test]
    fn accepts_exactly_two_operands() {
        let arguments =
            parse_arguments(["0x1000".into(), "8".into()].into_iter().collect()).unwrap();
        assert_eq!(arguments.address, 0x1000);
        assert_eq!(arguments.length, 8);
        for missing in [vec![], vec!["0x1000".into()]] {
            assert!(matches!(parse_arguments(missing), Err(Error::Usage(_))));
        }
        assert!(matches!(
            parse_arguments(vec!["0x1000".into(), "8".into(), "4".into()]),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn plans_the_page_holding_the_first_word() {
        assert_eq!(
            Plan::new(0, 4, PAGE).unwrap(),
            Plan {
                offset: 0,
                delta: 0,
                map_length: 4,
                words: 1,
            }
        );
        assert_eq!(
            Plan::new(0x1000, 8, PAGE).unwrap(),
            Plan {
                offset: 0x1000,
                delta: 0,
                map_length: 8,
                words: 2,
            }
        );
        assert_eq!(
            Plan::new(0x1004, 4, PAGE).unwrap(),
            Plan {
                offset: 0x1000,
                delta: 4,
                map_length: 8,
                words: 1,
            }
        );
    }

    #[test]
    fn plans_a_range_that_spans_pages() {
        assert_eq!(
            Plan::new(0x0ffc, 8, PAGE).unwrap(),
            Plan {
                offset: 0,
                delta: 0xffc,
                map_length: 0x1004,
                words: 2,
            }
        );
        assert_eq!(
            Plan::new(0x2000_0004, 0x2000, PAGE).unwrap(),
            Plan {
                offset: 0x2000_0000,
                delta: 4,
                map_length: 0x2004,
                words: 0x800,
            }
        );
    }

    /// However the request falls in its page, the mapping covers it exactly:
    /// the first word starts at `address`, every requested word ends inside the
    /// mapping, and the only excess is page slack.
    #[test]
    fn mapping_covers_exactly_the_requested_words() {
        for (address, length) in [(0, 4), (0x24, 0x1c), (0xffc, 0x1008), (0x1234_5678, 0x40)] {
            let plan = Plan::new(address, length, PAGE).unwrap();
            assert_eq!(plan.offset + plan.delta as u64, address);
            assert_eq!(plan.delta as u64 + length, plan.map_length as u64);
            assert_eq!(plan.words as u64 * WORD_SIZE, length);
            assert_eq!(plan.offset % PAGE, 0);
            assert!(plan.delta < PAGE as usize);
            assert!((length as usize) <= plan.map_length);
            assert!(plan.map_length < (length + PAGE) as usize);
        }
    }

    fn memfd() -> File {
        // SAFETY: the name is NUL-terminated and the flags are valid.
        let fd = unsafe { libc::memfd_create(c"physread-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create: {}", io::Error::last_os_error());
        // SAFETY: this test takes ownership of the new descriptor exactly once.
        unsafe { File::from_raw_fd(fd) }
    }

    /// The read path with a real `mmap`: a private file stands in for
    /// `/dev/mem`, which these host tests must never open.
    #[test]
    fn reads_the_planned_words_from_the_mapping() {
        let mut file = memfd();
        let pattern: Vec<u8> = (0..0x2000u32).map(|byte| byte as u8).collect();
        file.write_all(&pattern).unwrap();
        // 0x0ffc..0x1004 crosses a page boundary, so `delta`, the word offsets
        // and the mapped length all have to be right to read the pattern.
        let plan = Plan::new(0x0ffc, 8, PAGE).unwrap();
        let mapping = Mapping::map(&file, plan).unwrap();
        for index in 0..plan.words {
            let at = 0x0ffc + index * 4;
            let expected = u32::from_ne_bytes(pattern[at..at + 4].try_into().unwrap());
            assert_eq!(mapping.word(index), expected, "word {index}");
        }
        drop(mapping);
    }

    #[cfg(all(target_pointer_width = "32", target_env = "musl"))]
    #[test]
    fn musl_rejects_an_offset_that_would_wrap_mmap2() {
        let file = memfd();
        file.set_len(PAGE).unwrap();
        let plan = Plan::new(1u64 << 44, 4, PAGE).unwrap();
        // This fits off_t, but not mmap2's 32-bit count of 4096-byte units.
        // Musl checks OFF_MASK before the syscall. If it merely truncated,
        // the mapping would succeed against page zero of this file.
        assert!(matches!(Mapping::map(&file, plan), Err(Error::Runtime(_))));
    }

    #[test]
    fn rejects_unaligned_zero_or_odd_sized_requests() {
        for address in [1, 2, 3, 6] {
            assert!(matches!(Plan::new(address, 4, PAGE), Err(Error::Usage(_))));
        }
        for length in [1, 2, 3, 6] {
            assert!(matches!(
                Plan::new(0x1000, length, PAGE),
                Err(Error::Usage(_))
            ));
        }
        assert!(matches!(Plan::new(0x1000, 0, PAGE), Err(Error::Usage(_))));
    }

    #[test]
    fn rejects_ranges_that_overflow_or_leave_the_mmap_offset_range() {
        // The last aligned word ends one past `u64::MAX`.
        assert!(matches!(
            Plan::new(u64::MAX - 3, 4, PAGE),
            Err(Error::Usage(_))
        ));
        // Representable in `u64`, but `off_t` is signed, so no `mmap` offset
        // reaches it.
        assert!(matches!(
            Plan::new(u64::MAX - 7, 4, PAGE),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn refuses_a_page_size_that_cannot_align_word_reads() {
        assert!(matches!(Plan::new(0x1000, 4, 0), Err(Error::Runtime(_))));
        assert!(matches!(Plan::new(0x1000, 4, 2), Err(Error::Runtime(_))));
    }

    /// The page size a real host reports must pass `Plan`'s checks.
    #[test]
    fn the_host_page_size_is_usable() {
        assert!(Plan::new(0x1000, 4, page_size().unwrap()).is_ok());
    }
}
