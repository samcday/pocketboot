use std::{
    fmt,
    fs::File,
    io::{self, Write},
    os::fd::AsRawFd,
    sync::Once,
};

use crate::cmdline::KernelCommandLine;
use tracing_subscriber::{filter::LevelFilter, fmt::MakeWriter, prelude::*};

const KMSG: &str = "/dev/kmsg";

static TRACING: Once = Once::new();

const CMDLINE_PARAM: &str = "pocketboot.log";

pub(crate) fn init_tracing(cmdline: &KernelCommandLine) {
    TRACING.call_once(|| {
        let level = match cmdline.value(CMDLINE_PARAM).unwrap_or("warn") {
            "trace" => LevelFilter::TRACE,
            "debug" => LevelFilter::DEBUG,
            "info" => LevelFilter::INFO,
            "warn" | "warning" => LevelFilter::WARN,
            "error" => LevelFilter::ERROR,
            "off" => LevelFilter::OFF,
            _ => LevelFilter::INFO,
        };
        let layer = tracing_subscriber::fmt::layer()
            .without_time()
            .with_level(true)
            .with_target(true)
            .with_writer(KmsgMakeWriter);

        let _ = tracing_subscriber::registry()
            .with(level)
            .with(layer)
            .try_init();

        // A panic would otherwise only reach stderr, which a UART-free capture
        // does not retain. Route it through the kernel log as well.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            emergency_log(info);
            previous(info);
        }));
    });
}

// Stay below old kernels' record limits, including priority, prefix and newline.
const EMERGENCY_RECORD_BYTES: usize = 512;
const EMERGENCY_PREFIX: &str = "<3>pocketboot: ";
const TRUNCATED: &str = " [truncated]";

/// Best-effort diagnostics even when tracing is disabled or unavailable.
pub(crate) fn emergency_log(message: impl fmt::Display) {
    if let Ok(mut file) = File::options().write(true).open(KMSG) {
        write_emergency_record(&mut file, message);
    }
}

fn write_emergency_record(writer: &mut impl Write, message: impl fmt::Display) {
    let mut record = EmergencyRecord {
        bytes: [0; EMERGENCY_RECORD_BYTES],
        len: EMERGENCY_PREFIX.len(),
    };
    record.bytes[..record.len].copy_from_slice(EMERGENCY_PREFIX.as_bytes());
    // fmt::write returns an error when the bounded sink fills. Do not unwrap:
    // this path is also used from the panic hook.
    if fmt::write(&mut record, format_args!("{message}")).is_err() {
        let limit = EMERGENCY_RECORD_BYTES - 1 - TRUNCATED.len();
        record.len = record.len.min(limit);
        // All stored fragments are valid UTF-8; back up over continuation bytes.
        while record.bytes[record.len] & 0xc0 == 0x80 {
            record.len -= 1;
        }
        record.bytes[record.len..record.len + TRUNCATED.len()]
            .copy_from_slice(TRUNCATED.as_bytes());
        record.len += TRUNCATED.len();
    }
    record.bytes[record.len] = b'\n';
    record.len += 1;
    // One write is one kernel record. Never retry a short write as a new record,
    // and never report failures through tracing (or panic).
    let _ = writer.write(&record.bytes[..record.len]);
}

struct EmergencyRecord {
    bytes: [u8; EMERGENCY_RECORD_BYTES],
    len: usize,
}

impl fmt::Write for EmergencyRecord {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut count = text.len().min(EMERGENCY_RECORD_BYTES - 1 - self.len);
        while !text.is_char_boundary(count) {
            count -= 1;
        }
        self.bytes[self.len..self.len + count].copy_from_slice(&text.as_bytes()[..count]);
        self.len += count;
        if count < text.len() {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

struct KmsgMakeWriter;

impl<'writer> MakeWriter<'writer> for KmsgMakeWriter {
    type Writer = KmsgWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        KmsgWriter {
            file: File::options().write(true).open(KMSG).ok(),
            buffer: Vec::new(),
        }
    }
}

struct KmsgWriter {
    file: Option<File>,
    buffer: Vec<u8>,
}

impl Write for KmsgWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self
            .buffer
            .iter()
            .any(|byte| !matches!(byte, b'\n' | b'\r' | b'\0'))
        {
            self.buffer.clear();
            return Ok(());
        }

        if let Some(file) = &self.file {
            let written = unsafe {
                libc::write(
                    file.as_raw_fd(),
                    self.buffer.as_ptr().cast(),
                    self.buffer.len(),
                )
            };
            if written < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        self.buffer.clear();
        Ok(())
    }
}

impl Drop for KmsgWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

pub(crate) fn for_each_record_line<E>(
    raw: &[u8],
    mut emit: impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    let raw = trim_record_end(raw);
    if raw.is_empty() {
        return Ok(());
    }

    let Some(record) = KmsgRecord::parse(raw) else {
        let message = decode_kmsg_text(raw);
        if !message.is_empty() {
            emit(&message)?;
        }
        return Ok(());
    };

    let prefix = format_kmsg_time(record.timestamp_us);
    let message = decode_kmsg_text(record.text);
    for line in message.lines() {
        let line = line.trim_end_matches(['\r', '\0']);
        if line.is_empty() {
            continue;
        }

        let rendered = format!("{prefix} {line}");
        emit(&rendered)?;
    }

    Ok(())
}

struct KmsgRecord<'a> {
    timestamp_us: u64,
    text: &'a [u8],
}

impl<'a> KmsgRecord<'a> {
    fn parse(raw: &'a [u8]) -> Option<Self> {
        let separator = raw.iter().position(|byte| *byte == b';')?;
        let header = std::str::from_utf8(&raw[..separator]).ok()?;
        let mut fields = header.split(',');
        let _priority = fields.next()?;
        let _sequence = fields.next()?;
        let timestamp_us = fields.next()?.parse().ok()?;
        let text = first_body_line(&raw[separator + 1..]);

        Some(Self { timestamp_us, text })
    }
}

fn first_body_line(body: &[u8]) -> &[u8] {
    let end = body
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(body.len());
    trim_line_end(&body[..end])
}

fn format_kmsg_time(timestamp_us: u64) -> String {
    let secs = timestamp_us / 1_000_000;
    let usecs = timestamp_us % 1_000_000;
    format!("[{secs:>5}.{usecs:06}]")
}

fn decode_kmsg_text(input: &[u8]) -> String {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'\\' && index + 3 < input.len() && input[index + 1] == b'x' {
            if let (Some(hi), Some(lo)) = (hex(input[index + 2]), hex(input[index + 3])) {
                output.push((hi << 4) | lo);
                index += 4;
                continue;
            }
        }

        output.push(input[index]);
        index += 1;
    }

    String::from_utf8_lossy(&output).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn trim_record_end(mut value: &[u8]) -> &[u8] {
    while matches!(value.last(), Some(b'\n' | b'\r' | b'\0')) {
        value = &value[..value.len() - 1];
    }
    value
}

fn trim_line_end(mut value: &[u8]) -> &[u8] {
    while matches!(value.last(), Some(b'\r' | b'\0')) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(message: impl fmt::Display) -> String {
        let mut output = Vec::new();
        write_emergency_record(&mut output, message);
        assert!(output.len() <= EMERGENCY_RECORD_BYTES);
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn emergency_formats_prefix_message_and_newline() {
        assert_eq!(render(""), "<3>pocketboot: \n");
        assert_eq!(
            render(format_args!("panicked: {} at {}:{}", "oops", "main.rs", 42)),
            "<3>pocketboot: panicked: oops at main.rs:42\n"
        );
    }

    #[test]
    fn emergency_preserves_exact_fit_and_marks_overflow() {
        let capacity = EMERGENCY_RECORD_BYTES - EMERGENCY_PREFIX.len() - 1;
        let exact = "a".repeat(capacity);
        assert_eq!(render(&exact), format!("{EMERGENCY_PREFIX}{exact}\n"));
        let output = render(format_args!("{exact}b"));
        assert_eq!(output.len(), EMERGENCY_RECORD_BYTES);
        assert!(output.ends_with(" [truncated]\n"));
    }

    #[test]
    fn emergency_truncates_utf8_at_both_buffer_and_marker_boundaries() {
        for character in ["é", "界", "🦀"] {
            for offset in 0..4 {
                let message = format!("{}{}", "a".repeat(offset), character.repeat(512));
                let output = render(&message);
                let retained = output
                    .strip_prefix(EMERGENCY_PREFIX)
                    .unwrap()
                    .strip_suffix(" [truncated]\n")
                    .unwrap();
                assert!(message.starts_with(retained));
                assert!(output.len() >= EMERGENCY_RECORD_BYTES - 3);
            }
        }
    }

    #[test]
    fn emergency_stops_streamed_formatting_when_full() {
        struct Streaming;
        impl fmt::Display for Streaming {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // No allocation and no need to format the rest of a huge message.
                for _ in 0..EMERGENCY_RECORD_BYTES {
                    f.write_str("🦀")?;
                }
                panic!("bounded formatter should have stopped");
            }
        }
        assert!(render(Streaming).ends_with(" [truncated]\n"));
    }

    #[test]
    fn emergency_keeps_partial_output_on_formatting_error() {
        struct Broken;
        impl fmt::Display for Broken {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("partial")?;
                Err(fmt::Error)
            }
        }
        assert_eq!(render(Broken), "<3>pocketboot: partial [truncated]\n");
    }

    #[test]
    fn emergency_ignores_io_errors_and_short_writes_without_retrying() {
        struct Writer {
            calls: usize,
            fail: bool,
        }
        impl Write for Writer {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.fail {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    Ok(1)
                }
            }

            fn flush(&mut self) -> io::Result<()> {
                panic!("emergency output should not flush");
            }
        }
        for fail in [true, false] {
            let mut writer = Writer { calls: 0, fail };
            write_emergency_record(&mut writer, "oops");
            assert_eq!(writer.calls, 1);
        }
    }
}
