use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    thread,
    time::{Duration, Instant},
};

use rustix::termios::{OptionalActions, OutputModes, tcgetattr, tcsetattr};

use crate::kmsg;

const KMSG: &str = "/dev/kmsg";
const TTY_GS0: &str = "/dev/ttyGS0";
const KMSG_RECORD_MAX: usize = 64 * 1024;
const KMSG_BACKLOG_MAX: usize = 512 * 1024;
const TTY_QUEUE_HIGH_WATER: usize = 4 * 1024;
const TTY_SETTLE_MS: u64 = 500;
const TTY_DTR_LOG_AFTER: Duration = Duration::from_secs(1);

pub(crate) fn spawn() {
    match thread::Builder::new()
        .name("pocketboot-kmsg-forwarder".to_string())
        .spawn(run)
    {
        Ok(_thread) => {
            tracing::info!(
                thread = "pocketboot-kmsg-forwarder",
                "kmsg forwarder thread spawned"
            )
        }
        Err(err) => tracing::error!(error = ?err, "failed to spawn kmsg forwarder thread"),
    }
}

fn run() {
    loop {
        if let Err(err) = forward_once() {
            tracing::warn!(error = ?err, "kmsg forwarder failed; retrying");
            thread::sleep(Duration::from_secs(1));
        }
    }
}

fn forward_once() -> io::Result<()> {
    let mut kmsg = open_kmsg()?;
    tracing::debug!(path = KMSG, "kmsg forwarder opened source");
    let mut record = vec![0; KMSG_RECORD_MAX];
    let mut backlog = Backlog::default();
    drain_kmsg_to_backlog(&mut kmsg, &mut backlog, &mut record)?;

    let mut tty = wait_for_tty(&mut kmsg, &mut backlog, &mut record)?;
    flush_backlog(&mut tty, &mut backlog)?;

    loop {
        match read_kmsg(&mut kmsg, &mut record)? {
            KmsgRead::Record(read) => {
                write_kmsg_record(&mut tty, &record[..read])?;
                wait_for_tty_queue(&tty)?;
            }
            KmsgRead::Empty => thread::sleep(Duration::from_millis(50)),
            KmsgRead::Overwritten => {
                write_tty_line(&mut tty, b"pocketboot: kmsg records were overwritten")?;
            }
        }
    }
}

fn open_kmsg() -> io::Result<File> {
    let mut file = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(KMSG)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

fn wait_for_tty(kmsg: &mut File, backlog: &mut Backlog, record: &mut [u8]) -> io::Result<File> {
    let start = Instant::now();
    loop {
        drain_kmsg_to_backlog(kmsg, backlog, record)?;
        match File::options().read(true).write(true).open(TTY_GS0) {
            Ok(file) => {
                configure_tty(&file)?;
                tracing::info!(
                    tty = TTY_GS0,
                    elapsed_ms = start.elapsed().as_millis(),
                    backlog_records = backlog.records.len(),
                    backlog_bytes = backlog.bytes,
                    dropped_records = backlog.dropped_records,
                    dropped_bytes = backlog.dropped_bytes,
                    "kmsg forwarder attached to tty"
                );
                wait_for_host_dtr(&file, kmsg, backlog, record)?;
                return Ok(file);
            }
            Err(err) if should_retry_open(&err) => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err),
        }
    }
}

fn wait_for_host_dtr(
    tty: &File,
    kmsg: &mut File,
    backlog: &mut Backlog,
    record: &mut [u8],
) -> io::Result<()> {
    let start = Instant::now();
    let mut logged_wait = false;
    loop {
        drain_kmsg_to_backlog(kmsg, backlog, record)?;
        match tty_modem_bits(tty) {
            Ok(bits) if bits & libc::TIOCM_DTR != 0 => {
                tracing::info!(
                    elapsed_ms = start.elapsed().as_millis(),
                    modem_bits = bits,
                    "host asserted CDC-ACM DTR"
                );
                return Ok(());
            }
            Ok(_) => {
                if !logged_wait && start.elapsed() >= TTY_DTR_LOG_AFTER {
                    tracing::debug!("waiting for host CDC-ACM DTR before kmsg replay");
                    logged_wait = true;
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOTTY)) => {
                tracing::warn!(error = ?err, "CDC-ACM DTR unavailable; falling back to tty settle delay");
                return settle_tty(kmsg, backlog, record);
            }
            Err(err) => return Err(err),
        }
    }
}

fn settle_tty(kmsg: &mut File, backlog: &mut Backlog, record: &mut [u8]) -> io::Result<()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(TTY_SETTLE_MS) {
        drain_kmsg_to_backlog(kmsg, backlog, record)?;
        thread::sleep(Duration::from_millis(25));
    }
    Ok(())
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

#[derive(Default)]
struct Backlog {
    records: VecDeque<Vec<u8>>,
    bytes: usize,
    dropped_records: usize,
    dropped_bytes: usize,
}

impl Backlog {
    fn push(&mut self, record: &[u8]) {
        if record.len() > KMSG_BACKLOG_MAX {
            self.dropped_records += 1;
            self.dropped_bytes += record.len();
            return;
        }

        while self.bytes + record.len() > KMSG_BACKLOG_MAX {
            let Some(dropped) = self.records.pop_front() else {
                break;
            };
            self.bytes -= dropped.len();
            self.dropped_records += 1;
            self.dropped_bytes += dropped.len();
        }

        self.bytes += record.len();
        self.records.push_back(record.to_vec());
    }

    fn push_message(&mut self, message: &[u8]) {
        self.push(message);
    }

    fn pop(&mut self) -> Option<Vec<u8>> {
        let record = self.records.pop_front()?;
        self.bytes -= record.len();
        Some(record)
    }
}

enum KmsgRead {
    Record(usize),
    Empty,
    Overwritten,
}

fn read_kmsg(kmsg: &mut File, record: &mut [u8]) -> io::Result<KmsgRead> {
    match kmsg.read(record) {
        Ok(0) => Ok(KmsgRead::Empty),
        Ok(read) => Ok(KmsgRead::Record(read)),
        Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(KmsgRead::Empty),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(KmsgRead::Empty),
        Err(err) if err.raw_os_error() == Some(libc::EPIPE) => Ok(KmsgRead::Overwritten),
        Err(err) => Err(err),
    }
}

fn drain_kmsg_to_backlog(
    kmsg: &mut File,
    backlog: &mut Backlog,
    record: &mut [u8],
) -> io::Result<()> {
    loop {
        match read_kmsg(kmsg, record)? {
            KmsgRead::Record(read) => backlog.push(&record[..read]),
            KmsgRead::Empty => return Ok(()),
            KmsgRead::Overwritten => {
                backlog.push_message(b"pocketboot: kmsg records were overwritten before tty ready");
            }
        }
    }
}

fn flush_backlog(tty: &mut File, backlog: &mut Backlog) -> io::Result<()> {
    write_tty_line(tty, b"pocketboot: forwarding kernel log from /dev/kmsg")?;

    if backlog.dropped_records > 0 {
        write_tty_line(
            tty,
            format!(
                "pocketboot: kmsg backlog dropped {} records/{} bytes",
                backlog.dropped_records, backlog.dropped_bytes
            )
            .as_bytes(),
        )?;
    }

    tracing::info!(
        backlog_records = backlog.records.len(),
        backlog_bytes = backlog.bytes,
        "flushing kmsg backlog"
    );

    while let Some(record) = backlog.pop() {
        write_kmsg_record(tty, &record)?;
        wait_for_tty_queue(tty)?;
    }

    tracing::info!("kmsg forwarder backlog flushed");
    tty.flush()
}

fn write_kmsg_record(tty: &mut File, raw: &[u8]) -> io::Result<()> {
    kmsg::for_each_record_line(raw, |line| write_tty_line(tty, line.as_bytes()))?;
    tty.flush()
}

fn write_tty_line(tty: &mut File, line: &[u8]) -> io::Result<()> {
    let mut buffer = Vec::with_capacity(line.len() + 2);
    buffer.extend_from_slice(line);
    buffer.extend_from_slice(b"\r\n");
    tty.write_all(&buffer)
}

fn wait_for_tty_queue(tty: &File) -> io::Result<()> {
    loop {
        match tty_pending_bytes(tty) {
            Ok(pending) if pending <= TTY_QUEUE_HIGH_WATER => return Ok(()),
            Ok(_) => thread::sleep(Duration::from_millis(10)),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOTTY)) => {
                return Ok(());
            }
            Err(err) => return Err(err),
        }
    }
}

fn tty_pending_bytes(tty: &File) -> io::Result<usize> {
    let mut pending: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCOUTQ, &mut pending) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(pending.max(0) as usize)
}

fn tty_modem_bits(tty: &File) -> io::Result<libc::c_int> {
    let mut bits: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCMGET, &mut bits) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(bits)
}
