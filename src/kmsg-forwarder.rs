use std::{
    fs::File,
    io::{self, Read, Write},
    thread,
    time::{Duration, Instant},
};

use rustix::termios::{OptionalActions, OutputModes, tcgetattr, tcsetattr};

use crate::log_line;

const KMSG: &str = "/dev/kmsg";
const TTY_GS0: &str = "/dev/ttyGS0";
const KMSG_RECORD_MAX: usize = 64 * 1024;

pub(crate) fn spawn() {
    match thread::Builder::new()
        .name("pocketboot-kmsg-forwarder".to_string())
        .spawn(run)
    {
        Ok(_thread) => log_line("pocketboot: kmsg forwarder thread spawned"),
        Err(err) => log_line(&format!(
            "pocketboot: failed to spawn kmsg forwarder thread: {err}"
        )),
    }
}

fn run() {
    loop {
        if let Err(err) = forward_once() {
            log_line(&format!("pocketboot: kmsg forwarder failed: {err}"));
            thread::sleep(Duration::from_secs(1));
        }
    }
}

fn forward_once() -> io::Result<()> {
    let mut tty = wait_for_tty()?;
    let mut kmsg = File::open(KMSG)?;
    write_tty_line(
        &mut tty,
        b"pocketboot: forwarding kernel log from /dev/kmsg",
    )?;

    let mut record = vec![0; KMSG_RECORD_MAX];
    loop {
        match kmsg.read(&mut record) {
            Ok(0) => thread::sleep(Duration::from_millis(50)),
            Ok(read) => write_kmsg_record(&mut tty, &record[..read])?,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if err.raw_os_error() == Some(libc::EPIPE) => {
                write_tty_line(&mut tty, b"pocketboot: kmsg records were overwritten")?;
            }
            Err(err) => return Err(err),
        }
    }
}

fn wait_for_tty() -> io::Result<File> {
    let start = Instant::now();
    loop {
        match File::options().write(true).open(TTY_GS0) {
            Ok(file) => {
                configure_tty(&file)?;
                log_line(&format!(
                    "pocketboot: kmsg forwarder attached to {TTY_GS0} after {} ms",
                    start.elapsed().as_millis()
                ));
                return Ok(file);
            }
            Err(err) if should_retry_open(&err) => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err),
        }
    }
}

fn configure_tty(tty: &File) -> io::Result<()> {
    let mut termios = tcgetattr(tty)?;
    termios.make_raw();
    termios.output_modes.remove(
        OutputModes::OPOST
            | OutputModes::ONLCR
            | OutputModes::OCRNL
            | OutputModes::ONOCR
            | OutputModes::ONLRET,
    );
    tcsetattr(tty, OptionalActions::Now, &termios)?;
    Ok(())
}

fn should_retry_open(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock
    )
}

fn write_kmsg_record(tty: &mut File, raw: &[u8]) -> io::Result<()> {
    let payload = raw
        .iter()
        .position(|byte| *byte == b';')
        .map(|index| &raw[index + 1..])
        .unwrap_or(raw);
    write_tty_payload(tty, payload)
}

fn write_tty_payload(tty: &mut File, payload: &[u8]) -> io::Result<()> {
    let payload = trim_record_end(payload);
    if payload.is_empty() {
        return Ok(());
    }

    for line in payload.split(|byte| *byte == b'\n') {
        write_tty_line(tty, trim_line_end(line))?;
    }
    tty.flush()
}

fn write_tty_line(tty: &mut File, line: &[u8]) -> io::Result<()> {
    let mut buffer = Vec::with_capacity(line.len() + 2);
    buffer.extend_from_slice(line);
    buffer.extend_from_slice(b"\r\n");
    tty.write_all(&buffer)
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
