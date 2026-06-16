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

pub(super) fn handle(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<CommandResult> {
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

#[derive(Default)]
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
    let mut record = vec![0; KMSG_RECORD_MAX];
    let mut capture = DmesgCapture::default();

    loop {
        match kmsg.read(&mut record) {
            Ok(0) => break,
            Ok(read) => {
                capture.records += 1;
                append_kmsg_record(&mut capture.data, &record[..read])?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) if err.raw_os_error() == Some(libc::EPIPE) => {
                capture.overwritten = true;
                append_line(
                    &mut capture.data,
                    "pocketboot: kmsg records were overwritten",
                );
            }
            Err(err) => return Err(err),
        }
    }

    if capture.data.is_empty() {
        append_line(&mut capture.data, "pocketboot: no dmesg records captured");
    }

    Ok(capture)
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
