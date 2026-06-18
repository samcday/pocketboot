use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
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

use crate::kexec;

pub(crate) mod commands;

const COMMAND_MAX: usize = 64;
const DOWNLOAD_PREFIX: &str = "download:";
pub(crate) const MAX_DOWNLOAD_SIZE: u32 = 256 * 1024 * 1024;
const TRANSFER_CHUNK: usize = 1024 * 1024;
const RESPONSE_MAX: usize = 64;
const RESPONSE_STATUS_LEN: usize = 4;
const RESPONSE_PAYLOAD_MAX: usize = RESPONSE_MAX - RESPONSE_STATUS_LEN;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
const DISCONNECT_RETRY_DELAY: Duration = Duration::from_millis(250);
const FASTBOOT_SUBCLASS: u8 = 0x42;
const FASTBOOT_PROTOCOL: u8 = 0x03;

pub(crate) type PostResponseAction = Box<dyn FnOnce() -> io::Result<()> + Send + 'static>;
pub(crate) trait CommandHandler: Send + Sync {
    fn handle(&self, context: &mut CommandContext<'_>, command: &str) -> io::Result<CommandResult>;
}

impl<F> CommandHandler for F
where
    F: for<'a> Fn(&mut CommandContext<'a>, &str) -> io::Result<CommandResult>
        + Send
        + Sync
        + 'static,
{
    fn handle(&self, context: &mut CommandContext<'_>, command: &str) -> io::Result<CommandResult> {
        self(context, command)
    }
}

pub(crate) type CommandMap = Vec<Command>;

pub(crate) struct CommandResult {
    flow: CommandFlow,
    action: Option<PostResponseAction>,
}

enum CommandFlow {
    Continue,
    Exit,
}

impl CommandResult {
    pub(crate) fn continue_() -> Self {
        Self {
            flow: CommandFlow::Continue,
            action: None,
        }
    }

    pub(crate) fn continue_then(action: PostResponseAction) -> Self {
        Self {
            flow: CommandFlow::Continue,
            action: Some(action),
        }
    }

    pub(crate) fn exit(action: Option<PostResponseAction>) -> Self {
        Self {
            flow: CommandFlow::Exit,
            action,
        }
    }
}

pub(crate) struct Command {
    name: &'static str,
    match_kind: CommandMatch,
    handler: Box<dyn CommandHandler>,
}

#[derive(Clone, Copy)]
enum CommandMatch {
    Exact,
    Prefix,
}

impl Command {
    pub(crate) fn exact(name: &'static str, handler: impl CommandHandler + 'static) -> Self {
        Self {
            name,
            match_kind: CommandMatch::Exact,
            handler: Box::new(handler),
        }
    }

    pub(crate) fn prefix(prefix: &'static str, handler: impl CommandHandler + 'static) -> Self {
        Self {
            name: prefix,
            match_kind: CommandMatch::Prefix,
            handler: Box::new(handler),
        }
    }
}

enum ServerStep {
    Continue,
    Exit(Option<PostResponseAction>),
}

pub(crate) struct UsbFunction {
    handle: Handle,
    custom: Custom,
    rx: EndpointOut,
    tx: EndpointIn,
    commands: CommandMap,
}

impl UsbFunction {
    pub(crate) fn new(commands: CommandMap) -> Self {
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
            commands,
        }
    }

    pub(crate) fn handle(&self) -> Handle {
        self.handle.clone()
    }

    pub(crate) fn start(self) -> io::Result<(FastbootServer, EventLoop)> {
        let event_loop = EventLoop::spawn(self.custom)?;
        let server = FastbootServer::new(self.rx, self.tx, self.commands);
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
        let mut disconnected = false;

        loop {
            match self.run_once() {
                Ok(ServerStep::Continue) => {
                    if disconnected {
                        tracing::info!("fastboot host reconnected");
                        disconnected = false;
                    }
                }
                Ok(ServerStep::Exit(action)) => {
                    tracing::info!(
                        has_action = action.is_some(),
                        "fastboot server exit requested"
                    );
                    return Ok(action);
                }
                Err(err) if is_usb_disconnect(&err) => {
                    if disconnected {
                        tracing::debug!(errno = err.raw_os_error(), error = ?err, "fastboot host still disconnected");
                    } else {
                        tracing::info!(errno = err.raw_os_error(), error = ?err, "fastboot host disconnected");
                        disconnected = true;
                    }
                    thread::sleep(DISCONNECT_RETRY_DELAY);
                }
                Err(err) => {
                    tracing::warn!(errno = err.raw_os_error(), error = ?err, "fastboot server fatal transport error");
                    return Err(err);
                }
            }
        }
    }

    fn run_once(&mut self) -> io::Result<ServerStep> {
        let command = self.read_command()?;
        let Ok(command) = std::str::from_utf8(&command) else {
            FastbootResponder::new(&mut self.tx).fail(b"unrecognized command")?;
            return Ok(ServerStep::Continue);
        };
        tracing::info!(command, "fastboot command received");

        if command == "continue" {
            match FastbootResponder::new(&mut self.tx).okay_best_effort(b"") {
                Ok(()) => tracing::debug!(command, "fastboot OKAY sent"),
                Err(err) => {
                    tracing::warn!(command, error = ?err, "fastboot OKAY send failed")
                }
            }
            return Ok(ServerStep::Exit(None));
        }

        if matches!(command, "upload" | "get_staged") {
            self.upload_staged()?;
            return Ok(ServerStep::Continue);
        }

        if command.starts_with(DOWNLOAD_PREFIX) {
            match parse_download_command(command) {
                Ok(Some(size)) => {
                    tracing::info!(command, bytes = size, "fastboot download requested");
                    if let Err(err) = self.download(size) {
                        if is_usb_disconnect(&err) {
                            return Err(err);
                        }
                        tracing::warn!(command, error = ?err, "fastboot download failed");
                        FastbootResponder::new(&mut self.tx).fail(format!("{err}"))?;
                    }
                }
                Ok(None) => unreachable!("download prefix was checked"),
                Err(err) => FastbootResponder::new(&mut self.tx).fail(format!("{err}"))?,
            }
            return Ok(ServerStep::Continue);
        }

        let handler = find_command_handler(&self.commands, command);
        let mut context = CommandContext::new(&mut self.tx, &mut self.staged);
        match handler {
            Some(handler) => match handler.handle(&mut context, command) {
                Ok(result) => return finish_command_result(command, result),
                Err(err) => {
                    if is_usb_disconnect(&err) {
                        return Err(err);
                    }
                    tracing::warn!(command, error = ?err, "fastboot command failed");
                    context.fail(format!("{err}"))?;
                }
            },
            None => context.fail(b"unsupported command")?,
        }

        Ok(ServerStep::Continue)
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

    fn download(&mut self, size: u32) -> io::Result<()> {
        if size > MAX_DOWNLOAD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("download size exceeds max-download-size 0x{MAX_DOWNLOAD_SIZE:08x}"),
            ));
        }

        let size_u64 = u64::from(size);
        let mut file = kexec::create_payload_memfd("fastboot-download")?;
        file.set_len(size_u64)?;

        let buffer_len = usize::try_from(size_u64.min(TRANSFER_CHUNK as u64)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "download size does not fit usize",
            )
        })?;
        let mut buffer = transfer_buffer(buffer_len)?;

        FastbootResponder::new(&mut self.tx).data(size)?;

        let mut remaining = size_u64;
        while remaining > 0 {
            let chunk_len = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "download chunk does not fit usize",
                )
            })?;
            let chunk = &mut buffer[..chunk_len];
            self.rx.read_exact_timeout(chunk, TRANSFER_TIMEOUT)?;
            file.write_all(chunk)?;
            remaining -= chunk_len as u64;
        }

        file.seek(SeekFrom::Start(0))?;
        self.staged = Some(StagedData::file("download", file, size_u64));
        tracing::info!(bytes = size_u64, "download staged");
        FastbootResponder::new(&mut self.tx).okay(b"")
    }

    fn upload_staged(&mut self) -> io::Result<()> {
        let Some(staged) = self.staged.as_ref() else {
            FastbootResponder::new(&mut self.tx).fail(b"no staged data")?;
            return Ok(());
        };
        let Ok(size) = u32::try_from(staged.len()) else {
            FastbootResponder::new(&mut self.tx).fail(b"staged data too large")?;
            return Ok(());
        };
        tracing::info!(name = %staged.name, bytes = staged.len(), "uploading staged data");
        let mut responder = FastbootResponder::new(&mut self.tx);
        responder.data(size)?;
        staged.send(&mut responder)?;
        responder.okay(b"")
    }
}

fn finish_command_result(command: &str, result: CommandResult) -> io::Result<ServerStep> {
    match result.flow {
        CommandFlow::Continue => {
            if let Some(action) = result.action {
                tracing::info!(command, "running fastboot post-response action");
                if let Err(err) = action() {
                    tracing::warn!(command, error = ?err, "fastboot post-response action failed");
                } else {
                    tracing::info!(command, "fastboot post-response action completed");
                }
            }
            Ok(ServerStep::Continue)
        }
        CommandFlow::Exit => {
            tracing::info!(
                command,
                has_action = result.action.is_some(),
                "fastboot command requested exit"
            );
            Ok(ServerStep::Exit(result.action))
        }
    }
}

struct StagedData {
    name: String,
    payload: StagedPayload,
}

enum StagedPayload {
    Memory(Vec<u8>),
    File { file: File, size: u64 },
}

impl StagedData {
    fn memory(name: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            payload: StagedPayload::Memory(data),
        }
    }

    fn file(name: impl Into<String>, file: File, size: u64) -> Self {
        Self {
            name: name.into(),
            payload: StagedPayload::File { file, size },
        }
    }

    fn len(&self) -> u64 {
        match &self.payload {
            StagedPayload::Memory(data) => data.len() as u64,
            StagedPayload::File { size, .. } => *size,
        }
    }

    fn as_file(&self) -> io::Result<File> {
        match &self.payload {
            StagedPayload::Memory(data) => {
                let mut file = kexec::create_payload_memfd(&self.name)?;
                file.write_all(data)?;
                file.seek(SeekFrom::Start(0))?;
                Ok(file)
            }
            StagedPayload::File { file, .. } => {
                File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))
            }
        }
    }

    fn send(&self, responder: &mut FastbootResponder<'_>) -> io::Result<()> {
        match &self.payload {
            StagedPayload::Memory(data) => responder.send_payload(data),
            StagedPayload::File { .. } => {
                let mut file = self.as_file()?;
                responder.send_payload_from_reader(&mut file, self.len())
            }
        }
    }
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
        *self.staged = Some(StagedData::memory(name, data));
    }

    pub(crate) fn stage_file(&mut self, name: impl Into<String>, file: File, size: u64) {
        *self.staged = Some(StagedData::file(name, file, size));
    }

    pub(crate) fn staged_file(&self) -> io::Result<File> {
        self.staged
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no staged data"))?
            .as_file()
    }

    pub(crate) fn info(&mut self, message: impl AsRef<[u8]>) -> io::Result<()> {
        self.responder.info(message)
    }

    pub(crate) fn okay(&mut self, message: impl AsRef<[u8]>) -> io::Result<()> {
        self.responder.okay(message)
    }

    pub(crate) fn okay_then_exit(
        &mut self,
        message: impl AsRef<[u8]>,
        action: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> io::Result<CommandResult> {
        self.okay(message)?;
        Ok(CommandResult::exit(Some(Box::new(action))))
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

    fn send_payload_from_reader(
        &mut self,
        mut reader: impl Read,
        mut remaining: u64,
    ) -> io::Result<()> {
        let mut buffer = transfer_buffer(TRANSFER_CHUNK)?;
        while remaining > 0 {
            let chunk_len = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "upload chunk does not fit usize",
                )
            })?;
            reader.read_exact(&mut buffer[..chunk_len])?;
            self.send_payload(&buffer[..chunk_len])?;
            remaining -= chunk_len as u64;
        }
        Ok(())
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

fn transfer_buffer(len: usize) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|err| io::Error::other(format!("allocate transfer buffer: {err}")))?;
    buffer.resize(len, 0);
    Ok(buffer)
}

fn find_command_handler<'a>(
    commands: &'a [Command],
    command: &str,
) -> Option<&'a dyn CommandHandler> {
    commands
        .iter()
        .find_map(|entry| match entry.match_kind {
            CommandMatch::Exact if entry.name == command => Some(entry.handler.as_ref()),
            _ => None,
        })
        .or_else(|| {
            commands.iter().find_map(|entry| match entry.match_kind {
                CommandMatch::Prefix if command.starts_with(entry.name) => {
                    Some(entry.handler.as_ref())
                }
                _ => None,
            })
        })
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

fn is_usb_disconnect(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ESHUTDOWN | libc::EPIPE | libc::ENODEV | libc::ECONNRESET | libc::EIO)
    ) || matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
    )
}

fn parse_download_command(command: &str) -> io::Result<Option<u32>> {
    let Some(size) = command.strip_prefix(DOWNLOAD_PREFIX) else {
        return Ok(None);
    };
    if size.len() != 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "download size must be 8 hex digits",
        ));
    }

    u32::from_str_radix(size, 16)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid download size"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_handler(
        _context: &mut CommandContext<'_>,
        _command: &str,
    ) -> io::Result<CommandResult> {
        Ok(CommandResult::continue_())
    }

    #[test]
    fn finds_exact_command_handlers() {
        let commands = vec![Command::exact("oem dmesg", dummy_handler)];

        assert!(find_command_handler(&commands, "oem dmesg").is_some());
        assert!(find_command_handler(&commands, "oem dmesg:").is_none());
    }

    #[test]
    fn finds_prefix_command_handlers() {
        let commands = vec![Command::prefix("oem cat:", dummy_handler)];

        assert!(find_command_handler(&commands, "oem cat:/proc/cmdline").is_some());
        assert!(find_command_handler(&commands, "oem cat").is_none());
    }

    #[test]
    fn recognizes_usb_disconnect_errno_values() {
        for errno in [
            libc::ESHUTDOWN,
            libc::EPIPE,
            libc::ENODEV,
            libc::ECONNRESET,
            libc::EIO,
        ] {
            let err = io::Error::from_raw_os_error(errno);
            assert!(is_usb_disconnect(&err), "errno {errno} was not recognized");
        }
    }

    #[test]
    fn recognizes_usb_disconnect_error_kinds() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::NotConnected,
        ] {
            let err = io::Error::new(kind, "transport closed");
            assert!(is_usb_disconnect(&err), "kind {kind:?} was not recognized");
        }
    }

    #[test]
    fn continue_action_failure_does_not_exit_server() {
        let result = CommandResult::continue_then(Box::new(|| Err(io::Error::other("boom"))));

        assert!(matches!(
            finish_command_result("oem test", result).unwrap(),
            ServerStep::Continue
        ));
    }

    #[test]
    fn does_not_treat_protocol_errors_as_disconnects() {
        for kind in [
            io::ErrorKind::InvalidInput,
            io::ErrorKind::InvalidData,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::TimedOut,
        ] {
            let err = io::Error::new(kind, "protocol error");
            assert!(
                !is_usb_disconnect(&err),
                "kind {kind:?} was treated as disconnect"
            );
        }
    }
}
