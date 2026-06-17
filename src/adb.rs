use std::{
    collections::HashMap,
    ffi::CStr,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        raw::c_char,
        unix::process::CommandExt,
    },
    process::{Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
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

const ADB_SUBCLASS: u8 = 0x42;
const ADB_PROTOCOL: u8 = 0x01;

const A_SYNC: u32 = 0x434e5953;
const A_CNXN: u32 = 0x4e584e43;
const A_OPEN: u32 = 0x4e45504f;
const A_OKAY: u32 = 0x59414b4f;
const A_CLSE: u32 = 0x45534c43;
const A_WRTE: u32 = 0x45545257;
const A_AUTH: u32 = 0x48545541;
const A_STLS: u32 = 0x534c5453;

const A_VERSION: u32 = 0x01000001;
const MAX_PAYLOAD_V1: u32 = 4 * 1024;
const MAX_PAYLOAD: u32 = 1024 * 1024;
const HEADER_LEN: usize = 24;
const SHELL_CHUNK: usize = 4 * 1024;
const READ_TIMEOUT: Duration = Duration::from_millis(500);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
const DISCONNECT_RETRY_DELAY: Duration = Duration::from_millis(250);
const SHELL: &str = "/bin/sh";
const SHELL_SERVICE: &str = "shell:";

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
                Interface::new(Class::vendor_specific(ADB_SUBCLASS, ADB_PROTOCOL), "adb")
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

    pub(crate) fn start(self) -> io::Result<(AdbServer, EventLoop)> {
        let event_loop = EventLoop::spawn(self.custom)?;
        let server = AdbServer::new(self.rx, self.tx);
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
            .name("pocketboot-adb-events".to_string())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    match custom.event_timeout(Duration::from_millis(500)) {
                        Ok(Some(Event::Enable)) => tracing::info!("adb function enabled"),
                        Ok(Some(Event::Disable)) => tracing::info!("adb function disabled"),
                        Ok(Some(_event)) => {}
                        Ok(None) => {}
                        Err(err) if thread_stop.load(Ordering::Relaxed) => {
                            tracing::debug!(error = ?err, "adb event loop stopped");
                        }
                        Err(err) => tracing::warn!(error = ?err, "adb event loop error"),
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
            tracing::error!("adb event thread panicked");
        } else {
            tracing::debug!("adb event thread joined");
        }
    }
}

pub(crate) struct ServerHandle {
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<io::Result<()>>,
}

impl ServerHandle {
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub(crate) fn join(self) -> io::Result<()> {
        match self.thread.join() {
            Ok(result) => result,
            Err(_) => Err(io::Error::other("adb server thread panicked")),
        }
    }
}

pub(crate) struct AdbServer {
    rx: EndpointOut,
    writer: PacketWriter,
    stop: Arc<AtomicBool>,
    sessions: HashMap<u32, ShellSession>,
    next_local_id: u32,
}

impl AdbServer {
    fn new(rx: EndpointOut, tx: EndpointIn) -> Self {
        Self {
            rx,
            writer: PacketWriter::new(tx),
            stop: Arc::new(AtomicBool::new(false)),
            sessions: HashMap::new(),
            next_local_id: 1,
        }
    }

    pub(crate) fn spawn(self) -> io::Result<ServerHandle> {
        let stop = self.stop.clone();
        let thread = thread::Builder::new()
            .name("pocketboot-adb".to_string())
            .spawn(move || self.run())?;
        Ok(ServerHandle { stop, thread })
    }

    fn run(mut self) -> io::Result<()> {
        let mut disconnected = false;

        while !self.stop.load(Ordering::Relaxed) {
            match self.read_packet() {
                Ok(packet) => {
                    if disconnected {
                        tracing::info!("adb host reconnected");
                        disconnected = false;
                    }
                    if let Err(err) = self.handle_packet(packet) {
                        if is_usb_disconnect(&err) {
                            tracing::info!(errno = err.raw_os_error(), error = ?err, "adb host disconnected");
                            disconnected = true;
                            self.sessions.clear();
                        } else {
                            tracing::warn!(error = ?err, "adb packet handling failed");
                        }
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::TimedOut => {}
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {}
                Err(err) if is_usb_disconnect(&err) => {
                    if disconnected {
                        tracing::debug!(errno = err.raw_os_error(), error = ?err, "adb host still disconnected");
                    } else {
                        tracing::info!(errno = err.raw_os_error(), error = ?err, "adb host disconnected");
                        disconnected = true;
                        self.sessions.clear();
                    }
                    thread::sleep(DISCONNECT_RETRY_DELAY);
                }
                Err(err) => {
                    tracing::warn!(errno = err.raw_os_error(), error = ?err, "adb server fatal transport error");
                    return Err(err);
                }
            }
        }

        self.sessions.clear();
        tracing::info!("adb server stopped");
        Ok(())
    }

    fn read_packet(&mut self) -> io::Result<Packet> {
        let mut header = [0; HEADER_LEN];
        self.rx.read_exact_timeout(&mut header, READ_TIMEOUT)?;
        let message = Message::decode(header)?;

        if message.data_length > self.writer.max_payload() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "adb payload too large: {} > {}",
                    message.data_length,
                    self.writer.max_payload()
                ),
            ));
        }

        let payload_len = usize::try_from(message.data_length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "adb payload length overflows usize",
            )
        })?;
        let mut payload = vec![0; payload_len];
        if !payload.is_empty() {
            self.rx.read_exact_timeout(&mut payload, TRANSFER_TIMEOUT)?;
        }

        if message.data_check != 0 && message.data_check != checksum(&payload) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "adb payload checksum mismatch",
            ));
        }

        Ok(Packet { message, payload })
    }

    fn handle_packet(&mut self, packet: Packet) -> io::Result<()> {
        tracing::debug!(
            command = command_name(packet.message.command),
            arg0 = packet.message.arg0,
            arg1 = packet.message.arg1,
            payload = packet.payload.len(),
            "adb packet received"
        );

        match packet.message.command {
            A_CNXN => self.handle_connect(packet),
            A_OPEN => self.handle_open(packet),
            A_WRTE => self.handle_write(packet),
            A_OKAY => self.handle_okay(packet),
            A_CLSE => self.handle_close(packet),
            A_SYNC => Ok(()),
            A_AUTH | A_STLS => {
                tracing::debug!(
                    command = command_name(packet.message.command),
                    "ignoring unsupported adb command"
                );
                Ok(())
            }
            command => {
                tracing::debug!(
                    command = command_name(command),
                    "ignoring unknown adb command"
                );
                Ok(())
            }
        }
    }

    fn handle_connect(&mut self, packet: Packet) -> io::Result<()> {
        let max_payload = packet.message.arg1.clamp(MAX_PAYLOAD_V1, MAX_PAYLOAD);
        self.writer.set_max_payload(max_payload);
        self.sessions.clear();
        tracing::info!(
            version = packet.message.arg0,
            max_payload,
            "adb connection opened"
        );
        self.writer.send(
            A_CNXN,
            A_VERSION,
            max_payload,
            connection_banner().as_bytes(),
        )
    }

    fn handle_open(&mut self, packet: Packet) -> io::Result<()> {
        let remote_id = packet.message.arg0;
        let service = service_name(&packet.payload)?;
        let Some(command) = shell_command(service) else {
            tracing::debug!(service, "rejecting unsupported adb service");
            return self.writer.send(A_CLSE, 0, remote_id, &[]);
        };

        let local_id = self.allocate_local_id();
        match ShellSession::spawn(local_id, remote_id, command, self.writer.clone()) {
            Ok(session) => {
                self.writer.send(A_OKAY, local_id, remote_id, &[])?;
                self.sessions.insert(local_id, session);
                if let Some(session) = self.sessions.get(&local_id) {
                    session.allow_output();
                }
                tracing::info!(local_id, remote_id, "adb shell opened");
            }
            Err(err) => {
                tracing::warn!(error = ?err, "failed to start adb shell");
                self.writer.send(A_CLSE, 0, remote_id, &[])?;
            }
        }
        Ok(())
    }

    fn handle_write(&mut self, packet: Packet) -> io::Result<()> {
        let remote_id = packet.message.arg0;
        let local_id = packet.message.arg1;
        let Some(session) = self.sessions.get_mut(&local_id) else {
            return self.writer.send(A_CLSE, 0, remote_id, &[]);
        };

        if session.remote_id != remote_id {
            tracing::warn!(
                local_id,
                remote_id,
                expected_remote_id = session.remote_id,
                "adb stream id mismatch"
            );
            return self.writer.send(A_CLSE, local_id, remote_id, &[]);
        }

        match session.write_input(&packet.payload) {
            Ok(()) => self.writer.send(A_OKAY, local_id, remote_id, &[]),
            Err(err) => {
                tracing::debug!(local_id, remote_id, error = ?err, "adb shell input failed");
                let mut session = self.sessions.remove(&local_id).unwrap();
                session.close_from_device(&self.writer)
            }
        }
    }

    fn handle_okay(&mut self, packet: Packet) -> io::Result<()> {
        let remote_id = packet.message.arg0;
        let local_id = packet.message.arg1;
        if let Some(session) = self.sessions.get(&local_id) {
            if session.remote_id == remote_id {
                session.acknowledge_output();
            }
        }
        Ok(())
    }

    fn handle_close(&mut self, packet: Packet) -> io::Result<()> {
        let remote_id = packet.message.arg0;
        let local_id = packet.message.arg1;
        if let Some(mut session) = self.sessions.remove(&local_id) {
            tracing::info!(local_id, remote_id, "adb shell closed by host");
            session.close_from_host(&self.writer)?;
        }
        Ok(())
    }

    fn allocate_local_id(&mut self) -> u32 {
        let id = self.next_local_id;
        self.next_local_id = self.next_local_id.wrapping_add(1).max(1);
        id
    }
}

#[derive(Clone)]
struct PacketWriter {
    tx: Arc<Mutex<EndpointIn>>,
    max_payload: Arc<AtomicU32>,
}

impl PacketWriter {
    fn new(tx: EndpointIn) -> Self {
        Self {
            tx: Arc::new(Mutex::new(tx)),
            max_payload: Arc::new(AtomicU32::new(MAX_PAYLOAD)),
        }
    }

    fn max_payload(&self) -> u32 {
        self.max_payload.load(Ordering::Relaxed)
    }

    fn set_max_payload(&self, max_payload: u32) {
        self.max_payload.store(max_payload, Ordering::Relaxed);
    }

    fn send(&self, command: u32, arg0: u32, arg1: u32, payload: &[u8]) -> io::Result<()> {
        if payload.len() > self.max_payload() as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "adb payload too large: {} > {}",
                    payload.len(),
                    self.max_payload()
                ),
            ));
        }

        let packet = Packet::new(command, arg0, arg1, payload)?;
        let header = packet.message.encode();
        let mut tx = self
            .tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tx.write_all_timeout(&header, TRANSFER_TIMEOUT)?;
        if !packet.payload.is_empty() {
            tx.write_all_timeout(&packet.payload, TRANSFER_TIMEOUT)?;
        }
        tracing::debug!(
            command = command_name(command),
            arg0,
            arg1,
            payload = payload.len(),
            "adb packet sent"
        );
        Ok(())
    }
}

struct ShellSession {
    local_id: u32,
    remote_id: u32,
    master: File,
    child_pid: u32,
    output: Arc<OutputFlow>,
    output_thread: Option<thread::JoinHandle<()>>,
}

impl ShellSession {
    fn spawn(
        local_id: u32,
        remote_id: u32,
        command: Option<String>,
        writer: PacketWriter,
    ) -> io::Result<Self> {
        let (master, child_pid) = spawn_shell(command.as_deref())?;
        let output_master = master.try_clone()?;
        let output = Arc::new(OutputFlow::new());
        let thread_output = output.clone();
        let output_thread = thread::Builder::new()
            .name(format!("pocketboot-adb-shell-{local_id}"))
            .spawn(move || {
                run_shell_output(output_master, writer, local_id, remote_id, thread_output)
            })?;

        Ok(Self {
            local_id,
            remote_id,
            master,
            child_pid,
            output,
            output_thread: Some(output_thread),
        })
    }

    fn allow_output(&self) {
        self.output.allow_one_write();
    }

    fn acknowledge_output(&self) {
        self.output.allow_one_write();
    }

    fn write_input(&mut self, data: &[u8]) -> io::Result<()> {
        write_all_pty(&mut self.master, data)
    }

    fn close_from_host(&mut self, writer: &PacketWriter) -> io::Result<()> {
        let send_close = self.output.close();
        terminate_process_group(self.child_pid);
        if send_close {
            writer.send(A_CLSE, self.local_id, self.remote_id, &[])?;
        }
        Ok(())
    }

    fn close_from_device(&mut self, writer: &PacketWriter) -> io::Result<()> {
        let send_close = self.output.close();
        terminate_process_group(self.child_pid);
        if send_close {
            writer.send(A_CLSE, self.local_id, self.remote_id, &[])?;
        }
        Ok(())
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        self.output.close();
        terminate_process_group(self.child_pid);
        if let Some(thread) = self.output_thread.take() {
            if thread.join().is_err() {
                tracing::warn!(pid = self.child_pid, "adb shell output thread panicked");
            }
        }
    }
}

struct OutputFlow {
    state: Mutex<OutputState>,
    ready: Condvar,
}

struct OutputState {
    can_write: bool,
    closed: bool,
}

impl OutputFlow {
    fn new() -> Self {
        Self {
            state: Mutex::new(OutputState {
                can_write: false,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn allow_one_write(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.closed {
            state.can_write = true;
            self.ready.notify_all();
        }
    }

    fn wait_for_write_credit(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !state.closed && !state.can_write {
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if state.closed {
            return false;
        }
        state.can_write = false;
        true
    }

    fn close(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return false;
        }
        state.closed = true;
        self.ready.notify_all();
        true
    }
}

fn run_shell_output(
    mut master: File,
    writer: PacketWriter,
    local_id: u32,
    remote_id: u32,
    output: Arc<OutputFlow>,
) {
    let mut buffer = [0; SHELL_CHUNK];

    loop {
        if !output.wait_for_write_credit() {
            return;
        }

        let read = match read_pty(&mut master, &mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) => {
                tracing::debug!(local_id, remote_id, error = ?err, "adb shell output read failed");
                break;
            }
        };

        if let Err(err) = writer.send(A_WRTE, local_id, remote_id, &buffer[..read]) {
            tracing::debug!(local_id, remote_id, error = ?err, "adb shell output send failed");
            break;
        }
    }

    if output.close() {
        if let Err(err) = writer.send(A_CLSE, local_id, remote_id, &[]) {
            tracing::debug!(local_id, remote_id, error = ?err, "adb shell close send failed");
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Packet {
    message: Message,
    payload: Vec<u8>,
}

impl Packet {
    fn new(command: u32, arg0: u32, arg1: u32, payload: &[u8]) -> io::Result<Self> {
        let data_length = u32::try_from(payload.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "adb payload length overflows u32",
            )
        })?;
        Ok(Self {
            message: Message {
                command,
                arg0,
                arg1,
                data_length,
                data_check: checksum(payload),
                magic: command ^ u32::MAX,
            },
            payload: payload.to_vec(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Message {
    command: u32,
    arg0: u32,
    arg1: u32,
    data_length: u32,
    data_check: u32,
    magic: u32,
}

impl Message {
    fn decode(header: [u8; HEADER_LEN]) -> io::Result<Self> {
        let message = Self {
            command: u32::from_le_bytes(header[0..4].try_into().unwrap()),
            arg0: u32::from_le_bytes(header[4..8].try_into().unwrap()),
            arg1: u32::from_le_bytes(header[8..12].try_into().unwrap()),
            data_length: u32::from_le_bytes(header[12..16].try_into().unwrap()),
            data_check: u32::from_le_bytes(header[16..20].try_into().unwrap()),
            magic: u32::from_le_bytes(header[20..24].try_into().unwrap()),
        };
        if message.magic != (message.command ^ u32::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "adb packet magic mismatch",
            ));
        }
        Ok(message)
    }

    fn encode(&self) -> [u8; HEADER_LEN] {
        let mut header = [0; HEADER_LEN];
        header[0..4].copy_from_slice(&self.command.to_le_bytes());
        header[4..8].copy_from_slice(&self.arg0.to_le_bytes());
        header[8..12].copy_from_slice(&self.arg1.to_le_bytes());
        header[12..16].copy_from_slice(&self.data_length.to_le_bytes());
        header[16..20].copy_from_slice(&self.data_check.to_le_bytes());
        header[20..24].copy_from_slice(&self.magic.to_le_bytes());
        header
    }
}

fn checksum(payload: &[u8]) -> u32 {
    payload
        .iter()
        .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
}

fn connection_banner() -> String {
    "recovery::ro.product.name=pocketboot;ro.product.model=pocketboot;ro.product.device=pocketboot;features=".to_string()
}

fn service_name(payload: &[u8]) -> io::Result<&str> {
    let payload = payload.strip_suffix(b"\0").unwrap_or(payload);
    std::str::from_utf8(payload)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "adb service is not utf-8"))
}

fn shell_command(service: &str) -> Option<Option<String>> {
    if service == "shell" || service == SHELL_SERVICE {
        return Some(None);
    }
    service
        .strip_prefix(SHELL_SERVICE)
        .map(|command| Some(command.to_string()))
}

fn spawn_shell(command: Option<&str>) -> io::Result<(File, u32)> {
    let (master, slave) = open_pty()?;
    let stdin = slave.try_clone()?;
    let stdout = slave.try_clone()?;
    let stderr = slave.try_clone()?;
    let slave_fd = slave.as_raw_fd();

    let mut shell = Command::new(SHELL);
    if let Some(command) = command.filter(|command| !command.is_empty()) {
        shell.arg("-c").arg(command);
    } else {
        shell.arg("-i");
    }
    shell
        .env("HOME", "/")
        .env("PATH", "/bin:/sbin:/usr/bin:/usr/sbin")
        .env("TERM", "linux")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    unsafe {
        shell.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            Ok(())
        });
    }

    let child = shell
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("spawn {SHELL}: {err}")))?;
    Ok((master, child.id()))
}

fn open_pty() -> io::Result<(File, File)> {
    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { File::from_raw_fd(master_fd) };

    if unsafe { libc::grantpt(master.as_raw_fd()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::unlockpt(master.as_raw_fd()) } < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut name = [0 as c_char; 128];
    let result = unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    if result != 0 {
        return Err(if result > 0 {
            io::Error::from_raw_os_error(result)
        } else {
            io::Error::last_os_error()
        });
    }

    let slave_name = unsafe { CStr::from_ptr(name.as_ptr()) };
    let slave_fd = unsafe {
        libc::open(
            slave_name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    Ok((master, slave))
}

fn read_pty(master: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match master.read(buffer) {
            Ok(read) => return Ok(read),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if err.raw_os_error() == Some(libc::EIO) => return Ok(0),
            Err(err) => return Err(err),
        }
    }
}

fn write_all_pty(master: &mut File, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        match master.write(data) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pty write returned zero",
                ));
            }
            Ok(written) => data = &data[written..],
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn terminate_process_group(pid: u32) {
    let pgid = -(pid as libc::pid_t);
    unsafe {
        libc::kill(pgid, libc::SIGHUP);
    }
    thread::sleep(Duration::from_millis(50));
    unsafe {
        libc::kill(pgid, libc::SIGTERM);
    }
    thread::sleep(Duration::from_millis(100));
    unsafe {
        libc::kill(pgid, libc::SIGKILL);
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

fn command_name(command: u32) -> &'static str {
    match command {
        A_SYNC => "SYNC",
        A_CNXN => "CNXN",
        A_OPEN => "OPEN",
        A_OKAY => "OKAY",
        A_CLSE => "CLSE",
        A_WRTE => "WRTE",
        A_AUTH => "AUTH",
        A_STLS => "STLS",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_message() {
        let packet = Packet::new(A_CNXN, A_VERSION, MAX_PAYLOAD, b"recovery::features=").unwrap();

        let decoded = Message::decode(packet.message.encode()).unwrap();

        assert_eq!(decoded, packet.message);
        assert_eq!(decoded.magic, A_CNXN ^ u32::MAX);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut packet = Packet::new(A_OPEN, 1, 0, b"shell:\0").unwrap();
        let mut header = packet.message.encode();
        packet.message.magic = 0;
        header[20..24].copy_from_slice(&packet.message.magic.to_le_bytes());

        let err = Message::decode(header).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn computes_adb_checksum() {
        assert_eq!(
            checksum(b"ABC"),
            u32::from(b'A') + u32::from(b'B') + u32::from(b'C')
        );
    }

    #[test]
    fn parses_nul_terminated_service_name() {
        assert_eq!(service_name(b"shell:\0").unwrap(), "shell:");
    }

    #[test]
    fn recognizes_interactive_shell_service() {
        assert_eq!(shell_command("shell:"), Some(None));
        assert_eq!(shell_command("shell"), Some(None));
    }

    #[test]
    fn recognizes_command_shell_service() {
        assert_eq!(shell_command("shell:ls /"), Some(Some("ls /".to_string())));
    }

    #[test]
    fn rejects_non_shell_service() {
        assert_eq!(shell_command("sync:"), None);
    }
}
