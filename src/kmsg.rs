use std::{
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
    });
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
