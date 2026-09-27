//! `oem sha256:<partition>[:<offset>:<length>]` hashes a partition byte range
//! on the device, so a host can verify flashed data without reading it back
//! over USB.
//!
//! Offset and length are decimal or `0x` hex; without them the whole partition
//! is hashed. Before reading, dirty data from every block device node is
//! written back (`sync(2)`), which covers writes through the whole-disk node or
//! a UMS LUN as well as the partition itself. The partition's clean page cache
//! is then dropped, like `blockdev --flushbufs`, so the digest covers what the
//! medium returns rather than what is still in memory from the write.
//!
//! A fastboot response carries at most 60 payload bytes, too few for a
//! 64-character hex digest, so the result is delivered twice:
//! - staged as a `sha256sum` line, `<hex>  <argument>\n`, for
//!   `fastboot get_staged /dev/stdout`;
//! - in the OKAY payload as `sha256-b64=<base64 digest>`, for hosts that read
//!   the final response directly.
//!
//! Like the other diagnostic producers, the command replaces any staged data.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::fd::AsRawFd,
    path::Path,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use crate::fastboot::{CommandContext, CommandResult};

use super::partitions;

const COMMAND_PREFIX: &str = "oem sha256:";
const READ_CHUNK: usize = 1024 * 1024;
const PROGRESS_BYTES: u64 = 256 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const MIB: u64 = 1024 * 1024;
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

type Sha256Digest = [u8; 32];

pub(super) fn handle(context: &mut CommandContext<'_>, command: &str) -> io::Result<CommandResult> {
    context.clear_staged();
    let request = parse_request(command)?;
    let partition = partitions::find(request.partition)?;
    let range = request.range_within(partition.size_bytes)?;

    context.info(start_message(range, &partition.kernel_name))?;
    let flush_started = Instant::now();
    let mut source = open_uncached(&partition.dev_path)?;
    let hash_started = Instant::now();
    let mut progress = ProgressGate::new(hash_started);
    let digest = hash_range(&mut source, range, |hashed| {
        if hashed < range.length && progress.due(hashed, Instant::now()) {
            context.info(format!(
                "hashed {} of {} MiB",
                hashed / MIB,
                range.length / MIB
            ))?;
        }
        Ok(())
    })?;
    let hash_elapsed = hash_started.elapsed();
    let digest_hex = hex(&digest);

    tracing::info!(
        partition = partition.fastboot_name(),
        dev = %partition.dev_path.display(),
        offset = range.offset,
        length = range.length,
        flush_ms = hash_started.duration_since(flush_started).as_millis() as u64,
        hash_ms = hash_elapsed.as_millis() as u64,
        sha256 = %digest_hex,
        "partition range hashed"
    );
    context.stage("sha256", staged_line(&digest, request.spec));
    context.info(format!(
        "hashed {} bytes in {:.1}s ({:.1} MiB/s)",
        range.length,
        hash_elapsed.as_secs_f64(),
        mib_per_sec(range.length, hash_elapsed)
    ))?;
    context.info(b"run fastboot get_staged /dev/stdout to view")?;
    context.okay(okay_payload(&digest))?;
    Ok(CommandResult::continue_())
}

#[derive(Debug, Eq, PartialEq)]
struct HashRequest<'a> {
    spec: &'a str,
    partition: &'a str,
    range: Option<ByteRange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteRange {
    offset: u64,
    length: u64,
}

impl HashRequest<'_> {
    fn range_within(&self, size: u64) -> io::Result<ByteRange> {
        let Some(range) = self.range else {
            return Ok(ByteRange {
                offset: 0,
                length: size,
            });
        };
        let end = range
            .offset
            .checked_add(range.length)
            .ok_or_else(|| invalid_input("sha256 range end overflows"))?;
        if end > size {
            return Err(invalid_input(format!(
                "range end {end} exceeds {} size {size}",
                self.partition
            )));
        }
        Ok(range)
    }
}

fn parse_request(command: &str) -> io::Result<HashRequest<'_>> {
    let spec = command
        .strip_prefix(COMMAND_PREFIX)
        .ok_or_else(|| invalid_input("invalid sha256 command"))?;
    let fields = spec.split(':').collect::<Vec<_>>();
    let (partition, range) = match fields.as_slice() {
        [partition] => (*partition, None),
        [partition, offset, length] => (
            *partition,
            Some(ByteRange {
                offset: parse_number(offset, "offset")?,
                length: parse_number(length, "length")?,
            }),
        ),
        _ => {
            return Err(invalid_input(
                "usage: oem sha256:<part>[:<offset>:<length>]",
            ));
        }
    };
    if partition.is_empty() {
        return Err(invalid_input("sha256 partition is empty"));
    }

    Ok(HashRequest {
        spec,
        partition,
        range,
    })
}

fn parse_number(field: &str, label: &str) -> io::Result<u64> {
    let (digits, radix) = match field
        .strip_prefix("0x")
        .or_else(|| field.strip_prefix("0X"))
    {
        Some(hex) => (hex, 16),
        None => (field, 10),
    };
    if digits.is_empty() || !digits.chars().all(|digit| digit.is_digit(radix)) {
        return Err(invalid_input(format!(
            "{label} {field:?} is not decimal or 0x hex"
        )));
    }
    u64::from_str_radix(digits, radix)
        .map_err(|err| invalid_input(format!("{label} {field:?}: {err}")))
}

/// Open `path` read-only after writing back all dirty block device data and
/// dropping the partition's clean page cache, so the reads that follow come
/// from the medium.
fn open_uncached(path: &Path) -> io::Result<File> {
    let file = File::open(path).map_err(|err| device_error("open", path, err))?;
    // The partition node has its own page cache. Data written through the
    // whole-disk node or a UMS LUN that keeps the disk open can still be dirty
    // in another node's cache, so write back every block device. sync(2)
    // reports nothing; sync_all below reports this node's writeback errors.
    unsafe { libc::sync() };
    file.sync_all()
        .map_err(|err| device_error("flush", path, err))?;
    fadvise(&file, libc::POSIX_FADV_DONTNEED)
        .map_err(|err| device_error("drop cache of", path, err))?;
    if let Err(err) = fadvise(&file, libc::POSIX_FADV_SEQUENTIAL) {
        tracing::debug!(error = %err, dev = %path.display(), "sequential readahead hint failed");
    }
    Ok(file)
}

/// Wrap a device error with its action and path. The dispatcher classifies a
/// raw EIO as a USB disconnect, so an unwrapped media error would never reach
/// the host as FAIL.
fn device_error(action: &str, path: &Path, err: io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("{action} {}: {err}", path.display()))
}

fn fadvise(file: &File, advice: libc::c_int) -> io::Result<()> {
    match unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, advice) } {
        0 => Ok(()),
        err => Err(io::Error::from_raw_os_error(err)),
    }
}

/// Hash exactly `range` of `source`, calling `on_progress` with the running
/// byte count after every chunk. Source errors are wrapped with their byte
/// position; `on_progress` errors pass through unchanged.
fn hash_range(
    source: &mut (impl Read + Seek),
    range: ByteRange,
    mut on_progress: impl FnMut(u64) -> io::Result<()>,
) -> io::Result<Sha256Digest> {
    source
        .seek(SeekFrom::Start(range.offset))
        .map_err(|err| read_error(err, range.offset))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; READ_CHUNK];
    let mut hashed = 0;
    while hashed < range.length {
        let chunk_len = (range.length - hashed).min(READ_CHUNK as u64) as usize;
        let chunk = &mut buffer[..chunk_len];
        source
            .read_exact(chunk)
            .map_err(|err| read_error(err, range.offset + hashed))?;
        hasher.update(chunk);
        hashed += chunk_len as u64;
        on_progress(hashed)?;
    }
    Ok(hasher.finalize().into())
}

fn read_error(err: io::Error, position: u64) -> io::Error {
    io::Error::new(err.kind(), format!("read at byte {position}: {err}"))
}

/// Decides when a long hash owes the host an INFO line: after every
/// `PROGRESS_BYTES` or `PROGRESS_INTERVAL`, whichever comes first.
struct ProgressGate {
    reported_bytes: u64,
    reported_at: Instant,
}

impl ProgressGate {
    fn new(now: Instant) -> Self {
        Self {
            reported_bytes: 0,
            reported_at: now,
        }
    }

    fn due(&mut self, hashed: u64, now: Instant) -> bool {
        let due = hashed.saturating_sub(self.reported_bytes) >= PROGRESS_BYTES
            || now.saturating_duration_since(self.reported_at) >= PROGRESS_INTERVAL;
        if due {
            self.reported_bytes = hashed;
            self.reported_at = now;
        }
        due
    }
}

fn mib_per_sec(bytes: u64, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs > 0.0 {
        bytes as f64 / MIB as f64 / secs
    } else {
        0.0
    }
}

/// The first INFO line. It names the kernel device rather than its `/dev` path
/// so the line fits one response for 13-digit (up to 2 TB) values.
fn start_message(range: ByteRange, kernel_name: &str) -> String {
    format!(
        "hashing {} bytes at {} of {kernel_name}",
        range.length, range.offset
    )
}

/// The staged result: a `sha256sum`-style line for `fastboot get_staged`.
fn staged_line(digest: &Sha256Digest, spec: &str) -> Vec<u8> {
    format!("{}  {spec}\n", hex(digest)).into_bytes()
}

fn okay_payload(digest: &Sha256Digest) -> String {
    format!("sha256-b64={}", base64(digest))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn base64(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().enumerate().fold(0u32, |group, (index, byte)| {
            group | u32::from(*byte) << (16 - 8 * index)
        });
        for sextet in 0..4 {
            if sextet <= chunk.len() {
                let index = (group >> (18 - 6 * sextet)) & 0x3f;
                encoded.push(char::from(BASE64_ALPHABET[index as usize]));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kexec;
    use std::{
        fs,
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };

    // Reference digests computed with python3 hashlib, not with sha2.
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    // sha256(bytes((i * 31 + 7) & 0xff for i in range(1 MiB + 1)))
    const PATTERN_1MIB_PLUS_1_SHA256: &str =
        "0d09f3eabb5c78e5d435c41e3b3755f1c4d038657f739d0be0083c24e71235df";
    // The same pattern over 1 MiB + 9 bytes, hashing bytes [3, 3 + 1 MiB + 1).
    const PATTERN_SLICE_SHA256: &str =
        "fa9834e0cab807b8610d31644165fb998da65c450fcfb785632f0a7a4363e592";

    #[test]
    fn parses_whole_partition_request() {
        assert_eq!(
            parse_request("oem sha256:userdata").unwrap(),
            HashRequest {
                spec: "userdata",
                partition: "userdata",
                range: None,
            }
        );
    }

    #[test]
    fn parses_decimal_range() {
        let request = parse_request("oem sha256:mmcblk1p4:1024:67108864").unwrap();

        assert_eq!(request.spec, "mmcblk1p4:1024:67108864");
        assert_eq!(request.partition, "mmcblk1p4");
        assert_eq!(
            request.range,
            Some(ByteRange {
                offset: 1024,
                length: 67_108_864,
            })
        );
    }

    #[test]
    fn parses_hex_range() {
        let request = parse_request("oem sha256:mmcblk1p4:0x4000000:0X4000000").unwrap();

        assert_eq!(
            request.range,
            Some(ByteRange {
                offset: 0x400_0000,
                length: 0x400_0000,
            })
        );
    }

    #[test]
    fn parses_numbers() {
        assert_eq!(parse_number("0", "offset").unwrap(), 0);
        assert_eq!(parse_number("0x0", "offset").unwrap(), 0);
        assert_eq!(parse_number("0xdeadBEEF", "offset").unwrap(), 0xdead_beef);
        assert_eq!(
            parse_number("18446744073709551615", "offset").unwrap(),
            u64::MAX
        );
    }

    #[test]
    fn rejects_malformed_numbers() {
        for field in [
            "",
            "0x",
            "+5",
            "-1",
            "0x+5",
            "1_000",
            " 1",
            "12a",
            "0xg",
            "0b101",
            "18446744073709551616",
            "0x10000000000000000",
        ] {
            let err = parse_number(field, "offset").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{field:?}");
        }
    }

    #[test]
    fn rejects_malformed_requests() {
        for command in [
            "oem sha256:",
            "oem sha256::0:1",
            "oem sha256:boot:0",
            "oem sha256:boot:0:1:2",
            "oem sha256:boot::1",
            "oem sha256:boot:0:",
            "oem sha256:boot:x:1",
            "oem cat:boot",
        ] {
            let err = parse_request(command).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{command:?}");
        }
    }

    #[test]
    fn defaults_to_whole_partition() {
        let request = parse_request("oem sha256:boot").unwrap();

        assert_eq!(
            request.range_within(4096).unwrap(),
            ByteRange {
                offset: 0,
                length: 4096,
            }
        );
    }

    #[test]
    fn accepts_range_ending_at_partition_end() {
        let request = parse_request("oem sha256:boot:1024:3072").unwrap();

        assert_eq!(
            request.range_within(4096).unwrap(),
            ByteRange {
                offset: 1024,
                length: 3072,
            }
        );
    }

    #[test]
    fn rejects_ranges_past_partition_end() {
        for command in [
            "oem sha256:boot:1024:3073",
            "oem sha256:boot:4097:0",
            "oem sha256:boot:0x1:0xffffffffffffffff",
        ] {
            let err = parse_request(command)
                .unwrap()
                .range_within(4096)
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{command:?}");
        }
    }

    #[test]
    fn hashes_known_buffer() {
        let mut source = payload_file(b"abc");

        let digest = hash_range(&mut source, whole(3), |_| Ok(())).unwrap();

        assert_eq!(hex(&digest), ABC_SHA256);
    }

    #[test]
    fn hashes_empty_range() {
        let mut source = payload_file(b"abc");
        let range = ByteRange {
            offset: 3,
            length: 0,
        };

        let digest = hash_range(&mut source, range, |_| Ok(())).unwrap();

        assert_eq!(hex(&digest), EMPTY_SHA256);
    }

    #[test]
    fn hashes_multi_chunk_buffer_and_reports_each_chunk() {
        let mut source = payload_file(&pattern(READ_CHUNK + 1));
        let mut reports = Vec::new();

        let digest = hash_range(&mut source, whole(READ_CHUNK as u64 + 1), |hashed| {
            reports.push(hashed);
            Ok(())
        })
        .unwrap();

        assert_eq!(hex(&digest), PATTERN_1MIB_PLUS_1_SHA256);
        assert_eq!(reports, [READ_CHUNK as u64, READ_CHUNK as u64 + 1]);
    }

    #[test]
    fn hashes_exactly_the_requested_range() {
        let mut source = payload_file(&pattern(READ_CHUNK + 9));
        let range = ByteRange {
            offset: 3,
            length: READ_CHUNK as u64 + 1,
        };

        let digest = hash_range(&mut source, range, |_| Ok(())).unwrap();

        assert_eq!(hex(&digest), PATTERN_SLICE_SHA256);
    }

    #[test]
    fn fails_when_source_is_shorter_than_range() {
        let mut source = payload_file(b"abc");
        let range = ByteRange {
            offset: 1,
            length: 3,
        };

        let err = hash_range(&mut source, range, |_| Ok(())).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("read at byte 1"), "{err}");
    }

    #[test]
    fn media_errors_are_not_mistaken_for_usb_disconnects() {
        let mut source = FailingSource(libc::EIO);

        let err = hash_range(&mut source, whole(3), |_| Ok(())).unwrap_err();

        assert!(!crate::fastboot::is_usb_disconnect(&err), "{err}");
        assert!(err.to_string().contains("read at byte 0"), "{err}");
    }

    #[test]
    fn device_errors_are_not_mistaken_for_usb_disconnects() {
        let path = Path::new("/dev/mmcblk1p4");

        let err = device_error("flush", path, io::Error::from_raw_os_error(libc::EIO));

        assert!(!crate::fastboot::is_usb_disconnect(&err), "{err}");
        assert!(
            err.to_string().starts_with("flush /dev/mmcblk1p4: "),
            "{err}"
        );
    }

    #[test]
    fn progress_errors_pass_through_unwrapped() {
        let mut source = payload_file(b"abc");

        let err = hash_range(&mut source, whole(3), |_| {
            Err(io::Error::from_raw_os_error(libc::ESHUTDOWN))
        })
        .unwrap_err();

        assert_eq!(err.raw_os_error(), Some(libc::ESHUTDOWN));
    }

    #[test]
    fn hashes_regular_file_opened_uncached() {
        let temp = TempFile::new(b"xabcx");
        let mut source = open_uncached(&temp.path).unwrap();
        let range = ByteRange {
            offset: 1,
            length: 3,
        };

        let digest = hash_range(&mut source, range, |_| Ok(())).unwrap();

        assert_eq!(hex(&digest), ABC_SHA256);
    }

    #[test]
    fn progress_is_due_every_256_mib_without_time_passing() {
        let start = Instant::now();
        let mut gate = ProgressGate::new(start);

        assert!(!gate.due(MIB, start));
        assert!(!gate.due(PROGRESS_BYTES - 1, start));
        assert!(gate.due(PROGRESS_BYTES, start));
        assert!(!gate.due(PROGRESS_BYTES + MIB, start));
        assert!(gate.due(2 * PROGRESS_BYTES, start));
    }

    #[test]
    fn progress_is_due_every_5_seconds_on_slow_reads() {
        let start = Instant::now();
        let mut gate = ProgressGate::new(start);

        assert!(!gate.due(MIB, start + Duration::from_millis(4999)));
        assert!(gate.due(2 * MIB, start + PROGRESS_INTERVAL));
        assert!(!gate.due(3 * MIB, start + Duration::from_secs(9)));
        assert!(gate.due(4 * MIB, start + 2 * PROGRESS_INTERVAL));
    }

    #[test]
    fn encodes_rfc4648_base64_vectors() {
        for (input, expected) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input), expected, "{input:?}");
        }
    }

    #[test]
    fn staged_line_matches_sha256sum_format() {
        let mut source = payload_file(b"abc");
        let digest = hash_range(&mut source, whole(3), |_| Ok(())).unwrap();

        assert_eq!(
            staged_line(&digest, "mmcblk1p4:0:3"),
            format!("{ABC_SHA256}  mmcblk1p4:0:3\n").into_bytes()
        );
    }

    #[test]
    fn start_message_fits_one_fastboot_response() {
        // A 2 TB card: 13-digit length and offset, two-digit partition number.
        let range = ByteRange {
            offset: 1_999_999_999_999,
            length: 1_999_999_999_999,
        };

        let message = start_message(range, "mmcblk1p12");

        assert_eq!(
            message,
            "hashing 1999999999999 bytes at 1999999999999 of mmcblk1p12"
        );
        assert!(message.len() <= crate::fastboot::RESPONSE_PAYLOAD_MAX);
    }

    #[test]
    fn okay_payload_fits_one_fastboot_response() {
        let mut source = payload_file(b"abc");
        let digest = hash_range(&mut source, whole(3), |_| Ok(())).unwrap();
        let payload = okay_payload(&digest);

        // python3: base64.b64encode(hashlib.sha256(b"abc").digest())
        assert_eq!(
            payload,
            "sha256-b64=ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
        );
        assert!(payload.len() <= crate::fastboot::RESPONSE_PAYLOAD_MAX);
    }

    fn whole(length: u64) -> ByteRange {
        ByteRange { offset: 0, length }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index * 31 + 7) as u8).collect()
    }

    struct FailingSource(i32);

    impl Read for FailingSource {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(self.0))
        }
    }

    impl Seek for FailingSource {
        fn seek(&mut self, _position: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    fn payload_file(data: &[u8]) -> File {
        let mut file = kexec::create_payload_memfd("sha256-test").unwrap();
        file.write_all(data).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }

    struct TempFile {
        path: std::path::PathBuf,
    }

    impl TempFile {
        fn new(contents: &[u8]) -> Self {
            let mut path = std::env::temp_dir();
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            path.push(format!(
                "pocketboot-sha256-test-{}-{nonce}",
                std::process::id()
            ));
            fs::write(&path, contents).unwrap();
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
