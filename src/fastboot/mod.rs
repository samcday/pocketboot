use std::{
    collections::HashMap,
    io,
    os::fd::{AsRawFd, RawFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use gadgetry_most_foul::{
    Class,
    function::{
        Handle,
        custom::{Custom, Endpoint, EndpointDirection, EndpointIn, EndpointOut, Event, Interface},
    },
};

pub(crate) mod commands;

const COMMAND_MAX: usize = 64;
const RESPONSE_MAX: usize = 64;
const RESPONSE_STATUS_LEN: usize = 4;
const RESPONSE_PAYLOAD_MAX: usize = RESPONSE_MAX - RESPONSE_STATUS_LEN;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
const FASTBOOT_SUBCLASS: u8 = 0x42;
const FASTBOOT_PROTOCOL: u8 = 0x03;

pub(crate) type PostResponseAction = Box<dyn FnOnce() -> io::Result<()> + Send + 'static>;
pub(crate) type CommandHandler =
    fn(&mut CommandContext<'_>, &str) -> io::Result<Option<PostResponseAction>>;
pub(crate) type CommandMap = HashMap<&'static str, CommandHandler>;

pub(crate) struct UsbFunction {
    handle: Handle,
    custom: Custom,
    rx: EndpointOut,
    tx: EndpointIn,
}

impl UsbFunction {
    pub(crate) fn new() -> Self {
        let (rx, rx_dir) = EndpointDirection::host_to_device();
        let (tx, tx_dir) = EndpointDirection::device_to_host();
        let (custom, handle) = Custom::builder()
            .with_interface(
                Interface::new(
                    Class::vendor_specific(FASTBOOT_SUBCLASS, FASTBOOT_PROTOCOL),
                    "fastboot",
                )
                .with_endpoint(Endpoint::bulk(rx_dir))
                .with_endpoint(Endpoint::bulk(tx_dir)),
            )
            .build();

        Self {
            handle,
            custom,
            rx,
            tx,
        }
    }

    pub(crate) fn handle(&self) -> Handle {
        self.handle.clone()
    }

    pub(crate) fn start(self, commands: CommandMap) -> io::Result<(FastbootServer, EventLoop)> {
        let event_loop = EventLoop::spawn(self.custom)?;
        let server = FastbootServer::new(self.rx, self.tx, commands);
        Ok((server, event_loop))
    }
}

pub(crate) struct EventLoop {
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl EventLoop {
    fn spawn(mut custom: Custom) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("pocketboot-fastboot-events".to_string())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    match custom.event_timeout(Duration::from_millis(500)) {
                        Ok(Some(Event::Enable)) => tracing::info!("fastboot function enabled"),
                        Ok(Some(Event::Disable)) => tracing::info!("fastboot function disabled"),
                        Ok(Some(_event)) => {}
                        Ok(None) => {}
                        Err(err) if thread_stop.load(Ordering::Relaxed) => {
                            tracing::debug!(error = ?err, "fastboot event loop stopped");
                        }
                        Err(err) => tracing::warn!(error = ?err, "fastboot event loop error"),
                    }
                }
            })?;

        Ok(Self { stop, thread })
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub(crate) fn join(self) {
        if self.thread.join().is_err() {
            tracing::error!("fastboot event thread panicked");
        } else {
            tracing::debug!("fastboot event thread joined");
        }
    }
}

pub(crate) struct FastbootServer {
    rx: EndpointOut,
    tx: EndpointIn,
    commands: CommandMap,
    staged: Option<StagedData>,
}

impl FastbootServer {
    pub(crate) fn new(rx: EndpointOut, tx: EndpointIn, commands: CommandMap) -> Self {
        Self {
            rx,
            tx,
            commands,
            staged: None,
        }
    }

    pub(crate) fn run(mut self) -> io::Result<Option<PostResponseAction>> {
        loop {
            let command = self.read_command()?;
            let Ok(command) = std::str::from_utf8(&command) else {
                FastbootResponder::new(&mut self.tx).fail(b"unrecognized command")?;
                continue;
            };

            if command == "continue" {
                tracing::info!(command, "fastboot command received");
                match FastbootResponder::new(&mut self.tx).okay_best_effort(b"") {
                    Ok(()) => tracing::debug!(command, "fastboot OKAY sent"),
                    Err(err) => {
                        tracing::warn!(command, error = ?err, "fastboot OKAY send failed")
                    }
                }
                return Ok(None);
            }

            if matches!(command, "upload" | "get_staged") {
                self.upload_staged()?;
                continue;
            }

            let handler = self.commands.get(command).copied();
            let mut context = CommandContext::new(&mut self.tx, &mut self.staged);
            match handler {
                Some(handler) => match handler(&mut context, command) {
                    Ok(Some(action)) => return Ok(Some(action)),
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(command, error = ?err, "fastboot command failed");
                        context.fail(format!("{err}"))?;
                    }
                },
                None => context.fail(b"unsupported command")?,
            }
        }
    }

    fn read_command(&mut self) -> io::Result<Vec<u8>> {
        let mut buffer = [0; COMMAND_MAX];
        let read = read_packet(&mut self.rx, &mut buffer)?;
        let command = &buffer[..read];
        let end = command
            .iter()
            .position(|byte| *byte == b'\0')
            .unwrap_or(command.len());
        Ok(command[..end].to_vec())
    }

    fn upload_staged(&mut self) -> io::Result<()> {
        let Some(staged) = self.staged.as_ref() else {
            FastbootResponder::new(&mut self.tx).fail(b"no staged data")?;
            return Ok(());
        };
        let Ok(size) = u32::try_from(staged.data.len()) else {
            FastbootResponder::new(&mut self.tx).fail(b"staged data too large")?;
            return Ok(());
        };
        if size == 0 {
            FastbootResponder::new(&mut self.tx).fail(b"no staged data")?;
            return Ok(());
        }

        tracing::info!(name = %staged.name, bytes = staged.data.len(), "uploading staged data");
        let mut responder = FastbootResponder::new(&mut self.tx);
        responder.data(size)?;
        responder.send_payload(&staged.data)?;
        responder.okay(b"")
    }
}

struct StagedData {
    name: String,
    data: Vec<u8>,
}

pub(crate) struct CommandContext<'a> {
    responder: FastbootResponder<'a>,
    staged: &'a mut Option<StagedData>,
}

impl<'a> CommandContext<'a> {
    fn new(tx: &'a mut EndpointIn, staged: &'a mut Option<StagedData>) -> Self {
        Self {
            responder: FastbootResponder::new(tx),
            staged,
        }
    }

    pub(crate) fn stage(&mut self, name: impl Into<String>, data: Vec<u8>) {
        *self.staged = Some(StagedData {
            name: name.into(),
            data,
        });
    }

    pub(crate) fn info(&mut self, message: impl AsRef<[u8]>) -> io::Result<()> {
        self.responder.info(message)
    }

    pub(crate) fn okay(&mut self, message: impl AsRef<[u8]>) -> io::Result<()> {
        self.responder.okay(message)
    }

    pub(crate) fn okay_then(
        &mut self,
        message: impl AsRef<[u8]>,
        action: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> io::Result<Option<PostResponseAction>> {
        self.okay(message)?;
        Ok(Some(Box::new(action)))
    }

    pub(crate) fn fail(&mut self, message: impl AsRef<[u8]>) -> io::Result<()> {
        self.responder.fail(message)
    }
}

pub(crate) struct FastbootResponder<'a> {
    tx: &'a mut EndpointIn,
}

impl<'a> FastbootResponder<'a> {
    fn new(tx: &'a mut EndpointIn) -> Self {
        Self { tx }
    }

    pub(crate) fn info(&mut self, message: impl AsRef<[u8]>) -> io::Result<()> {
        for chunk in message.as_ref().chunks(RESPONSE_PAYLOAD_MAX) {
            self.write_packet(b"INFO", chunk)?;
        }
        Ok(())
    }

    pub(crate) fn okay(&mut self, message: impl AsRef<[u8]>) -> io::Result<()> {
        self.write_packet_truncated(b"OKAY", message.as_ref())
    }

    fn data(&mut self, size: u32) -> io::Result<()> {
        self.write_packet(b"DATA", format!("{size:08x}").as_bytes())
    }

    fn send_payload(&mut self, data: &[u8]) -> io::Result<()> {
        self.tx.write_all_timeout(data, TRANSFER_TIMEOUT)
    }

    fn okay_best_effort(&mut self, message: impl AsRef<[u8]>) -> io::Result<()> {
        let payload = message.as_ref();
        self.write_packet_nonblocking(b"OKAY", &payload[..payload.len().min(RESPONSE_PAYLOAD_MAX)])
    }

    pub(crate) fn fail(&mut self, message: impl AsRef<[u8]>) -> io::Result<()> {
        self.write_packet_truncated(b"FAIL", message.as_ref())
    }

    fn write_packet_truncated(
        &mut self,
        status: &[u8; RESPONSE_STATUS_LEN],
        payload: &[u8],
    ) -> io::Result<()> {
        self.write_packet(status, &payload[..payload.len().min(RESPONSE_PAYLOAD_MAX)])
    }

    fn write_packet(
        &mut self,
        status: &[u8; RESPONSE_STATUS_LEN],
        payload: &[u8],
    ) -> io::Result<()> {
        if payload.len() > RESPONSE_PAYLOAD_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fastboot response payload too large",
            ));
        }

        let mut packet = Vec::with_capacity(RESPONSE_STATUS_LEN + payload.len());
        packet.extend_from_slice(status);
        packet.extend_from_slice(payload);
        self.tx.write_all_timeout(&packet, RESPONSE_TIMEOUT)
    }

    fn write_packet_nonblocking(
        &mut self,
        status: &[u8; RESPONSE_STATUS_LEN],
        payload: &[u8],
    ) -> io::Result<()> {
        if payload.len() > RESPONSE_PAYLOAD_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fastboot response payload too large",
            ));
        }

        let mut packet = Vec::with_capacity(RESPONSE_STATUS_LEN + payload.len());
        packet.extend_from_slice(status);
        packet.extend_from_slice(payload);

        let control = self.tx.control()?;
        let fd = control.as_raw_fd();
        let flags = set_nonblocking(fd)?;
        let result = write_all_fd(fd, &packet);
        let restore_result = restore_flags(fd, flags);
        result.and(restore_result)
    }
}

fn read_packet(endpoint: &mut EndpointOut, buffer: &mut [u8]) -> io::Result<usize> {
    let control = endpoint.control()?;
    let fd = control.as_raw_fd();

    loop {
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read >= 0 {
            return Ok(read as usize);
        }

        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<libc::c_int> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }

    if flags & libc::O_NONBLOCK == 0 {
        let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(flags)
}

fn restore_flags(fd: RawFd, flags: libc::c_int) -> io::Result<()> {
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_all_fd(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let written = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if written > 0 {
            data = &data[written as usize..];
            continue;
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short fastboot response write",
            ));
        }

        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    Ok(())
}
