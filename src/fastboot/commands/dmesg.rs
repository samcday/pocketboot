use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
};

use crate::{
    fastboot::{CommandContext, CommandResult},
    kmsg,
};

const KMSG: &str = "/dev/kmsg";
const KMSG_RECORD_MAX: usize = 64 * 1024;
const DMESG_CAPTURE_MAX_BYTES: usize = 4 * 1024 * 1024;
const DMESG_CAPTURE_MAX_RECORDS: usize = 32 * 1024;

#[derive(Clone, Copy)]
struct CaptureLimits {
    bytes: usize,
    records: usize,
}

const CAPTURE_LIMITS: CaptureLimits = CaptureLimits {
    bytes: DMESG_CAPTURE_MAX_BYTES,
    records: DMESG_CAPTURE_MAX_RECORDS,
};

pub(super) fn handle(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<CommandResult> {
    context.clear_staged();
    let capture = capture_dmesg()?;
    let records = capture.records;
    let bytes = capture.data.len();
    let overwritten = capture.overwritten;

    context.stage("dmesg", capture.data);
    context.info(format!("{records} dmesg records staged ({bytes} bytes)"))?;
    if overwritten {
        context.info(b"some dmesg records were overwritten before capture")?;
    }
    context.info(b"run fastboot get_staged /dev/stdout to view")?;
    context.okay(b"")?;
    Ok(CommandResult::continue_())
}

#[derive(Debug, Default)]
struct DmesgCapture {
    data: Vec<u8>,
    records: usize,
    overwritten: bool,
}

fn capture_dmesg() -> io::Result<DmesgCapture> {
    let mut kmsg = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(KMSG)?;
    kmsg.seek(SeekFrom::Start(0))?;
    capture_dmesg_from(&mut kmsg, CAPTURE_LIMITS)
}

fn capture_dmesg_from(kmsg: &mut impl Read, limits: CaptureLimits) -> io::Result<DmesgCapture> {
    let mut record = vec![0; KMSG_RECORD_MAX];
    let mut capture = DmesgCapture::default();

    loop {
        match kmsg.read(&mut record) {
            Ok(0) => break,
            Ok(read) => {
                if capture.records >= limits.records {
                    return Err(capture_limit_error(format!(
                        "dmesg capture exceeds record limit ({})",
                        limits.records
                    )));
                }
                let mut formatted = Vec::new();
                append_kmsg_record(&mut formatted, &record[..read])?;
                append_limited(&mut capture.data, &formatted, limits.bytes)?;
                capture.records += 1;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) if err.raw_os_error() == Some(libc::EPIPE) => {
                capture.overwritten = true;
                append_line_limited(
                    &mut capture.data,
                    "pocketboot: kmsg records were overwritten",
                    limits.bytes,
                )?;
            }
            Err(err) => return Err(err),
        }
    }

    if capture.data.is_empty() {
        append_line_limited(
            &mut capture.data,
            "pocketboot: no dmesg records captured",
            limits.bytes,
        )?;
    }

    Ok(capture)
}

fn append_limited(output: &mut Vec<u8>, data: &[u8], limit: usize) -> io::Result<()> {
    let new_len = output
        .len()
        .checked_add(data.len())
        .ok_or_else(|| capture_limit_error("dmesg capture size overflow"))?;
    if new_len > limit {
        return Err(capture_limit_error(format!(
            "dmesg capture exceeds byte limit ({limit})"
        )));
    }
    output.extend_from_slice(data);
    Ok(())
}

fn append_line_limited(output: &mut Vec<u8>, line: &str, limit: usize) -> io::Result<()> {
    let mut formatted = Vec::with_capacity(line.len().saturating_add(1));
    append_line(&mut formatted, line);
    append_limited(output, &formatted, limit)
}

fn capture_limit_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn append_kmsg_record(output: &mut Vec<u8>, raw: &[u8]) -> io::Result<()> {
    kmsg::for_each_record_line(raw, |line| {
        append_line(output, line);
        Ok(())
    })
}

fn append_line(output: &mut Vec<u8>, line: &str) {
    output.extend_from_slice(line.as_bytes());
    output.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct RecordReader {
        records: VecDeque<Vec<u8>>,
    }

    impl RecordReader {
        fn new(records: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self {
                records: records.into_iter().collect(),
            }
        }
    }

    impl Read for RecordReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let Some(record) = self.records.pop_front() else {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            };
            assert!(record.len() <= buffer.len());
            buffer[..record.len()].copy_from_slice(&record);
            Ok(record.len())
        }
    }

    fn record(text: &str) -> Vec<u8> {
        format!("6,1,1000,-;{text}\n").into_bytes()
    }

    #[test]
    fn capture_fails_closed_at_record_limit() {
        let mut input = RecordReader::new([record("one"), record("two")]);
        let err = capture_dmesg_from(
            &mut input,
            CaptureLimits {
                bytes: 1024,
                records: 1,
            },
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("record limit"));
    }

    #[test]
    fn capture_fails_closed_at_byte_limit() {
        let mut input = RecordReader::new([record("this record will not fit")]);
        let err = capture_dmesg_from(
            &mut input,
            CaptureLimits {
                bytes: 8,
                records: 10,
            },
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("byte limit"));
    }

    #[test]
    fn capture_accepts_data_within_both_limits() {
        let mut input = RecordReader::new([record("one"), record("two")]);
        let capture = capture_dmesg_from(
            &mut input,
            CaptureLimits {
                bytes: 1024,
                records: 2,
            },
        )
        .unwrap();

        assert_eq!(capture.records, 2);
        assert!(capture.data.ends_with(b"two\n"));
    }
}
