use std::{
    collections::HashMap,
    ffi::{CStr, CString, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        raw::c_char,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
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
const SYNC_CHUNK: usize = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_millis(500);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
const DISCONNECT_RETRY_DELAY: Duration = Duration::from_millis(250);
const SHELL: &str = "/bin/sh";
const SHELL_SERVICE: &str = "shell:";
const EXEC_SERVICE: &str = "exec:";
const SYNC_SERVICE: &str = "sync:";

const SYNC_STAT: &[u8; 4] = b"STAT";
const SYNC_LIST: &[u8; 4] = b"LIST";
const SYNC_SEND: &[u8; 4] = b"SEND";
const SYNC_RECV: &[u8; 4] = b"RECV";
const SYNC_DENT: &[u8; 4] = b"DENT";
const SYNC_DONE: &[u8; 4] = b"DONE";
const SYNC_DATA: &[u8; 4] = b"DATA";
const SYNC_OKAY: &[u8; 4] = b"OKAY";
const SYNC_FAIL: &[u8; 4] = b"FAIL";
const SYNC_QUIT: &[u8; 4] = b"QUIT";

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
    sessions: HashMap<u32, AdbSession>,
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
        let Some(service) = adb_service(service) else {
            tracing::debug!(service, "rejecting unsupported adb service");
            return self.writer.send(A_CLSE, 0, remote_id, &[]);
        };

        let local_id = self.allocate_local_id();
        match AdbSession::spawn(local_id, remote_id, service, self.writer.clone()) {
            Ok(session) => {
                self.writer.send(A_OKAY, local_id, remote_id, &[])?;
                let kind = session.kind();
                self.sessions.insert(local_id, session);
                if let Some(session) = self.sessions.get(&local_id) {
                    session.allow_output();
                }
                tracing::info!(local_id, remote_id, kind, "adb stream opened");
            }
            Err(err) => {
                tracing::warn!(error = ?err, "failed to start adb stream");
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

        if session.remote_id() != remote_id {
            tracing::warn!(
                local_id,
                remote_id,
                expected_remote_id = session.remote_id(),
                "adb stream id mismatch"
            );
            return self.writer.send(A_CLSE, local_id, remote_id, &[]);
        }

        if session.acknowledge_before_write() {
            self.writer.send(A_OKAY, local_id, remote_id, &[])?;
        }

        match session.write_input(&packet.payload) {
            Ok(()) if session.acknowledge_before_write() => Ok(()),
            Ok(()) => self.writer.send(A_OKAY, local_id, remote_id, &[]),
            Err(err) => {
                tracing::debug!(local_id, remote_id, error = ?err, "adb stream input failed");
                let mut session = self.sessions.remove(&local_id).unwrap();
                session.close_from_device(&self.writer)
            }
        }
    }

    fn handle_okay(&mut self, packet: Packet) -> io::Result<()> {
        let remote_id = packet.message.arg0;
        let local_id = packet.message.arg1;
        if let Some(session) = self.sessions.get(&local_id) {
            if session.remote_id() == remote_id {
                session.acknowledge_output();
            }
        }
        Ok(())
    }

    fn handle_close(&mut self, packet: Packet) -> io::Result<()> {
        let remote_id = packet.message.arg0;
        let local_id = packet.message.arg1;
        if let Some(mut session) = self.sessions.remove(&local_id) {
            tracing::info!(
                local_id,
                remote_id,
                kind = session.kind(),
                "adb stream closed by host"
            );
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

enum AdbSession {
    Shell(ShellSession),
    Exec(RawCommandSession),
    Sync(SyncSession),
}

impl AdbSession {
    fn spawn(
        local_id: u32,
        remote_id: u32,
        service: AdbService,
        writer: PacketWriter,
    ) -> io::Result<Self> {
        match service {
            AdbService::Shell(command) => {
                ShellSession::spawn(local_id, remote_id, command, writer).map(AdbSession::Shell)
            }
            AdbService::Exec(command) => {
                RawCommandSession::spawn(local_id, remote_id, command, writer).map(AdbSession::Exec)
            }
            AdbService::Sync => {
                SyncSession::spawn(local_id, remote_id, writer).map(AdbSession::Sync)
            }
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            AdbSession::Shell(_) => "shell",
            AdbSession::Exec(_) => "exec",
            AdbSession::Sync(_) => "sync",
        }
    }

    fn remote_id(&self) -> u32 {
        match self {
            AdbSession::Shell(session) => session.remote_id,
            AdbSession::Exec(session) => session.remote_id,
            AdbSession::Sync(session) => session.remote_id,
        }
    }

    fn acknowledge_before_write(&self) -> bool {
        matches!(self, AdbSession::Sync(_))
    }

    fn allow_output(&self) {
        match self {
            AdbSession::Shell(session) => session.allow_output(),
            AdbSession::Exec(session) => session.allow_output(),
            AdbSession::Sync(session) => session.allow_output(),
        }
    }

    fn acknowledge_output(&self) {
        match self {
            AdbSession::Shell(session) => session.acknowledge_output(),
            AdbSession::Exec(session) => session.acknowledge_output(),
            AdbSession::Sync(session) => session.acknowledge_output(),
        }
    }

    fn write_input(&mut self, data: &[u8]) -> io::Result<()> {
        match self {
            AdbSession::Shell(session) => session.write_input(data),
            AdbSession::Exec(session) => session.write_input(data),
            AdbSession::Sync(session) => session.write_input(data),
        }
    }

    fn close_from_host(&mut self, writer: &PacketWriter) -> io::Result<()> {
        match self {
            AdbSession::Shell(session) => session.close_from_host(writer),
            AdbSession::Exec(session) => session.close_from_host(writer),
            AdbSession::Sync(session) => session.close_from_host(writer),
        }
    }

    fn close_from_device(&mut self, writer: &PacketWriter) -> io::Result<()> {
        match self {
            AdbSession::Shell(session) => session.close_from_device(writer),
            AdbSession::Exec(session) => session.close_from_device(writer),
            AdbSession::Sync(session) => session.close_from_device(writer),
        }
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

struct RawCommandSession {
    local_id: u32,
    remote_id: u32,
    stdin: Option<ChildStdin>,
    child_pid: u32,
    child_running: Arc<AtomicBool>,
    output: Arc<OutputFlow>,
    output_thread: Option<thread::JoinHandle<()>>,
}

impl RawCommandSession {
    fn spawn(
        local_id: u32,
        remote_id: u32,
        command: String,
        writer: PacketWriter,
    ) -> io::Result<Self> {
        let (stdin, output_reader, child) = spawn_raw_command(&command)?;
        let child_pid = child.id();
        let child_running = Arc::new(AtomicBool::new(true));
        let output = Arc::new(OutputFlow::new());
        let thread_child_running = child_running.clone();
        let thread_output = output.clone();
        let output_thread = thread::Builder::new()
            .name(format!("pocketboot-adb-exec-{local_id}"))
            .spawn(move || {
                run_raw_command_output(
                    output_reader,
                    child,
                    writer,
                    local_id,
                    remote_id,
                    thread_child_running,
                    thread_output,
                )
            })?;

        Ok(Self {
            local_id,
            remote_id,
            stdin: Some(stdin),
            child_pid,
            child_running,
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
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "adb exec stdin is closed",
            ));
        };
        stdin.write_all(data)
    }

    fn close_from_host(&mut self, writer: &PacketWriter) -> io::Result<()> {
        let send_close = self.close();
        if send_close {
            writer.send(A_CLSE, self.local_id, self.remote_id, &[])?;
        }
        Ok(())
    }

    fn close_from_device(&mut self, writer: &PacketWriter) -> io::Result<()> {
        let send_close = self.close();
        if send_close {
            writer.send(A_CLSE, self.local_id, self.remote_id, &[])?;
        }
        Ok(())
    }

    fn close(&mut self) -> bool {
        self.stdin.take();
        if self.child_running.swap(false, Ordering::AcqRel) {
            terminate_process_group(self.child_pid);
        }
        self.output.close()
    }
}

impl Drop for RawCommandSession {
    fn drop(&mut self) {
        self.close();
        if let Some(thread) = self.output_thread.take() {
            if thread.join().is_err() {
                tracing::warn!(pid = self.child_pid, "adb exec output thread panicked");
            }
        }
    }
}

struct SyncSession {
    local_id: u32,
    remote_id: u32,
    buffer: Vec<u8>,
    state: SyncState,
    responses: Option<mpsc::Sender<SyncResponse>>,
    output: Arc<OutputFlow>,
    output_thread: Option<thread::JoinHandle<()>>,
}

enum SyncState {
    Idle,
    Receiving(SyncSend),
}

struct SyncSend {
    path: PathBuf,
    mode: u32,
    file: Option<File>,
    failure: Option<String>,
}

enum SyncResponse {
    Packet(Vec<u8>),
    File(PathBuf),
}

impl SyncSession {
    fn spawn(local_id: u32, remote_id: u32, writer: PacketWriter) -> io::Result<Self> {
        let (responses, response_rx) = mpsc::channel();
        let output = Arc::new(OutputFlow::new());
        let thread_output = output.clone();
        let output_thread = thread::Builder::new()
            .name(format!("pocketboot-adb-sync-{local_id}"))
            .spawn(move || {
                run_sync_output(response_rx, writer, local_id, remote_id, thread_output)
            })?;

        Ok(Self {
            local_id,
            remote_id,
            buffer: Vec::new(),
            state: SyncState::Idle,
            responses: Some(responses),
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
        self.buffer.extend_from_slice(data);
        self.process_buffer()
    }

    fn close_from_host(&mut self, writer: &PacketWriter) -> io::Result<()> {
        let send_close = self.close();
        if send_close {
            writer.send(A_CLSE, self.local_id, self.remote_id, &[])?;
        }
        Ok(())
    }

    fn close_from_device(&mut self, writer: &PacketWriter) -> io::Result<()> {
        let send_close = self.close();
        if send_close {
            writer.send(A_CLSE, self.local_id, self.remote_id, &[])?;
        }
        Ok(())
    }

    fn close(&mut self) -> bool {
        self.responses.take();
        self.output.close()
    }

    fn process_buffer(&mut self) -> io::Result<()> {
        loop {
            let consumed = match self.state {
                SyncState::Idle => self.process_idle_message()?,
                SyncState::Receiving(_) => self.process_send_message()?,
            };
            if !consumed {
                return Ok(());
            }
        }
    }

    fn process_idle_message(&mut self) -> io::Result<bool> {
        if self.buffer.len() < 8 {
            return Ok(false);
        }

        let id = sync_id(&self.buffer[..4]);
        let length = sync_u32(&self.buffer[4..8]) as usize;
        let total = 8usize.checked_add(length).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "sync request length overflow")
        })?;
        if self.buffer.len() < total {
            return Ok(false);
        }

        let payload = self.buffer[8..total].to_vec();
        self.buffer.drain(..total);
        self.handle_sync_request(id, &payload)?;
        Ok(true)
    }

    fn process_send_message(&mut self) -> io::Result<bool> {
        if self.buffer.len() < 8 {
            return Ok(false);
        }

        let id = sync_id(&self.buffer[..4]);
        let value = sync_u32(&self.buffer[4..8]);
        if &id == SYNC_DATA {
            let length = value as usize;
            let total = 8usize.checked_add(length).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "sync data length overflow")
            })?;
            if self.buffer.len() < total {
                return Ok(false);
            }
            let data = self.buffer[8..total].to_vec();
            self.buffer.drain(..total);
            self.handle_send_data(&data);
            return Ok(true);
        }

        if &id == SYNC_DONE {
            self.buffer.drain(..8);
            self.finish_send(value)?;
            return Ok(true);
        }

        self.buffer.clear();
        self.state = SyncState::Idle;
        self.queue_fail(format!(
            "expected DATA or DONE during SEND, got {}",
            sync_id_name(&id)
        ))?;
        Ok(false)
    }

    fn handle_sync_request(&mut self, id: [u8; 4], payload: &[u8]) -> io::Result<()> {
        match &id {
            SYNC_STAT => self.handle_stat(payload),
            SYNC_LIST => self.handle_list(payload),
            SYNC_RECV => self.handle_recv(payload),
            SYNC_SEND => self.start_send(payload),
            SYNC_QUIT => Ok(()),
            _ => self.queue_fail(format!("unsupported sync request {}", sync_id_name(&id))),
        }
    }

    fn handle_stat(&self, payload: &[u8]) -> io::Result<()> {
        let path = sync_path(payload);
        let (mode, size, mtime) = sync_stat(&path);
        self.queue_packet(sync_stat_packet(mode, size, mtime))
    }

    fn handle_list(&self, payload: &[u8]) -> io::Result<()> {
        let path = sync_path(payload);
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(err) => return self.queue_fail(format!("list {}: {err}", path.display())),
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => return self.queue_fail(format!("list {}: {err}", path.display())),
            };
            let name = entry.file_name();
            let name = name.as_os_str().as_bytes();
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) => {
                    return self.queue_fail(format!("stat {}: {err}", entry.path().display()));
                }
            };
            self.queue_packet(sync_dent_packet(
                metadata.mode(),
                sync_size(&metadata),
                sync_mtime(&metadata),
                name,
            )?)?;
        }

        self.queue_packet(sync_list_done_packet())
    }

    fn handle_recv(&self, payload: &[u8]) -> io::Result<()> {
        self.queue_response(SyncResponse::File(sync_path(payload)))
    }

    fn start_send(&mut self, payload: &[u8]) -> io::Result<()> {
        let (path, mode) = match parse_send_target(payload) {
            Ok(target) => target,
            Err(err) => return self.queue_fail(err.to_string()),
        };

        let file_type = mode & libc::S_IFMT;
        if file_type == libc::S_IFDIR {
            let failure = fs::create_dir_all(&path)
                .err()
                .map(|err| format!("mkdir {}: {err}", path.display()));
            self.state = SyncState::Receiving(SyncSend {
                path,
                mode,
                file: None,
                failure,
            });
            return Ok(());
        }

        if file_type == libc::S_IFLNK {
            self.state = SyncState::Receiving(SyncSend {
                path: path.clone(),
                mode,
                file: None,
                failure: Some(format!("symlink push is not supported: {}", path.display())),
            });
            return Ok(());
        }

        let file = match create_sync_file(&path) {
            Ok(file) => Some(file),
            Err(err) => {
                self.state = SyncState::Receiving(SyncSend {
                    path: path.clone(),
                    mode,
                    file: None,
                    failure: Some(format!("create {}: {err}", path.display())),
                });
                return Ok(());
            }
        };

        self.state = SyncState::Receiving(SyncSend {
            path,
            mode,
            file,
            failure: None,
        });
        Ok(())
    }

    fn handle_send_data(&mut self, data: &[u8]) {
        let SyncState::Receiving(send) = &mut self.state else {
            return;
        };
        if send.failure.is_some() {
            return;
        }

        let Some(file) = send.file.as_mut() else {
            if !data.is_empty() {
                send.failure = Some(format!(
                    "cannot write data to directory {}",
                    send.path.display()
                ));
            }
            return;
        };

        if let Err(err) = file.write_all(data) {
            send.failure = Some(format!("write {}: {err}", send.path.display()));
            send.file.take();
        }
    }

    fn finish_send(&mut self, mtime: u32) -> io::Result<()> {
        let state = std::mem::replace(&mut self.state, SyncState::Idle);
        let SyncState::Receiving(mut send) = state else {
            return self.queue_fail("DONE without SEND");
        };

        if let Some(file) = send.file.as_mut() {
            if let Err(err) = file.flush() {
                send.failure = Some(format!("flush {}: {err}", send.path.display()));
            }
        }
        drop(send.file.take());

        if send.failure.is_none() {
            if let Err(err) = set_sync_mode(&send.path, send.mode) {
                send.failure = Some(format!("chmod {}: {err}", send.path.display()));
            }
        }
        if send.failure.is_none() {
            if let Err(err) = set_sync_mtime(&send.path, mtime) {
                send.failure = Some(format!("utime {}: {err}", send.path.display()));
            }
        }

        match send.failure {
            Some(failure) => self.queue_fail(failure),
            None => self.queue_packet(sync_status_packet(SYNC_OKAY, 0)),
        }
    }

    fn queue_fail(&self, message: impl AsRef<str>) -> io::Result<()> {
        self.queue_packet(sync_fail_packet(message.as_ref())?)
    }

    fn queue_packet(&self, payload: Vec<u8>) -> io::Result<()> {
        self.queue_response(SyncResponse::Packet(payload))
    }

    fn queue_response(&self, response: SyncResponse) -> io::Result<()> {
        let Some(responses) = &self.responses else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "adb sync response channel is closed",
            ));
        };
        responses.send(response).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "adb sync response thread stopped",
            )
        })
    }
}

impl Drop for SyncSession {
    fn drop(&mut self) {
        self.close();
        if let Some(thread) = self.output_thread.take() {
            if thread.join().is_err() {
                tracing::warn!("adb sync output thread panicked");
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

fn run_raw_command_output(
    mut output_reader: File,
    mut child: Child,
    writer: PacketWriter,
    local_id: u32,
    remote_id: u32,
    child_running: Arc<AtomicBool>,
    output: Arc<OutputFlow>,
) {
    let mut buffer = [0; SHELL_CHUNK];

    loop {
        if !output.wait_for_write_credit() {
            let _ = child.wait();
            child_running.store(false, Ordering::Release);
            return;
        }

        let read = match read_fd(&mut output_reader, &mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) => {
                tracing::debug!(local_id, remote_id, error = ?err, "adb exec output read failed");
                break;
            }
        };

        if let Err(err) = writer.send(A_WRTE, local_id, remote_id, &buffer[..read]) {
            tracing::debug!(local_id, remote_id, error = ?err, "adb exec output send failed");
            break;
        }
    }

    if let Err(err) = child.wait() {
        tracing::debug!(local_id, remote_id, error = ?err, "adb exec wait failed");
    }
    child_running.store(false, Ordering::Release);

    if output.close() {
        if let Err(err) = writer.send(A_CLSE, local_id, remote_id, &[]) {
            tracing::debug!(local_id, remote_id, error = ?err, "adb exec close send failed");
        }
    }
}

fn run_sync_output(
    responses: mpsc::Receiver<SyncResponse>,
    writer: PacketWriter,
    local_id: u32,
    remote_id: u32,
    output: Arc<OutputFlow>,
) {
    while let Ok(response) = responses.recv() {
        let sent = match response {
            SyncResponse::Packet(payload) => {
                send_stream_payload(&writer, local_id, remote_id, &output, &payload)
            }
            SyncResponse::File(path) => {
                send_sync_file(&writer, local_id, remote_id, &output, &path)
            }
        };
        if !sent {
            break;
        }
    }
}

fn send_sync_file(
    writer: &PacketWriter,
    local_id: u32,
    remote_id: u32,
    output: &OutputFlow,
    path: &Path,
) -> bool {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) => {
            let payload = match sync_fail_packet(&format!("open {}: {err}", path.display())) {
                Ok(payload) => payload,
                Err(err) => {
                    tracing::debug!(local_id, remote_id, error = ?err, "adb sync fail packet failed");
                    return false;
                }
            };
            return send_stream_payload(writer, local_id, remote_id, output, &payload);
        }
    };

    let chunk_size = SYNC_CHUNK.min(writer.max_payload().saturating_sub(8).max(1) as usize);
    let mut buffer = vec![0; chunk_size];
    loop {
        let read = match read_fd(&mut file, &mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) => {
                let payload = match sync_fail_packet(&format!("read {}: {err}", path.display())) {
                    Ok(payload) => payload,
                    Err(err) => {
                        tracing::debug!(local_id, remote_id, error = ?err, "adb sync fail packet failed");
                        return false;
                    }
                };
                return send_stream_payload(writer, local_id, remote_id, output, &payload);
            }
        };

        let payload = match sync_data_packet(&buffer[..read]) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::debug!(local_id, remote_id, error = ?err, "adb sync data packet failed");
                return false;
            }
        };
        if !send_stream_payload(writer, local_id, remote_id, output, &payload) {
            return false;
        }
    }

    send_stream_payload(
        writer,
        local_id,
        remote_id,
        output,
        &sync_status_packet(SYNC_DONE, 0),
    )
}

fn send_stream_payload(
    writer: &PacketWriter,
    local_id: u32,
    remote_id: u32,
    output: &OutputFlow,
    payload: &[u8],
) -> bool {
    if !output.wait_for_write_credit() {
        return false;
    }

    if let Err(err) = writer.send(A_WRTE, local_id, remote_id, payload) {
        tracing::debug!(local_id, remote_id, error = ?err, "adb stream output send failed");
        return false;
    }
    true
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum AdbService {
    Shell(Option<String>),
    Exec(String),
    Sync,
}

fn adb_service(service: &str) -> Option<AdbService> {
    if service == "shell" || service == SHELL_SERVICE {
        return Some(AdbService::Shell(None));
    }
    if service == SYNC_SERVICE {
        return Some(AdbService::Sync);
    }
    if let Some(command) = service.strip_prefix(EXEC_SERVICE) {
        return Some(AdbService::Exec(command.to_string()));
    }
    service
        .strip_prefix(SHELL_SERVICE)
        .map(|command| AdbService::Shell(Some(command.to_string())))
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

fn spawn_raw_command(command: &str) -> io::Result<(ChildStdin, File, Child)> {
    let (output_reader, output_writer) = open_pipe()?;
    let stdout = output_writer.try_clone()?;
    let stderr = output_writer;

    let mut shell = Command::new(SHELL);
    shell
        .arg("-c")
        .arg(command)
        .env("HOME", "/")
        .env("PATH", "/bin:/sbin:/usr/bin:/usr/sbin")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    unsafe {
        shell.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            Ok(())
        });
    }

    let mut child = shell
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("spawn {SHELL}: {err}")))?;
    let stdin = child.stdin.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::BrokenPipe, "spawned adb exec without stdin")
    })?;
    Ok((stdin, output_reader, child))
}

fn open_pipe() -> io::Result<(File, File)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }

    let read = unsafe { File::from_raw_fd(fds[0]) };
    let write = unsafe { File::from_raw_fd(fds[1]) };
    Ok((read, write))
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

fn sync_id(bytes: &[u8]) -> [u8; 4] {
    bytes.try_into().expect("sync id is always four bytes")
}

fn sync_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("sync u32 is always four bytes"))
}

fn sync_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

fn sync_stat(path: &Path) -> (u32, u32, u32) {
    match fs::metadata(path) {
        Ok(metadata) => (metadata.mode(), sync_size(&metadata), sync_mtime(&metadata)),
        Err(_) => (0, 0, 0),
    }
}

fn sync_size(metadata: &fs::Metadata) -> u32 {
    metadata.size().min(u64::from(u32::MAX)) as u32
}

fn sync_mtime(metadata: &fs::Metadata) -> u32 {
    metadata.mtime().clamp(0, i64::from(u32::MAX)) as u32
}

fn parse_send_target(payload: &[u8]) -> io::Result<(PathBuf, u32)> {
    let Some(separator) = payload.iter().rposition(|byte| *byte == b',') else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SEND target is missing mode",
        ));
    };
    let path = sync_path(&payload[..separator]);
    let mode = std::str::from_utf8(&payload[separator + 1..])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "SEND mode is not utf-8"))?
        .parse::<u32>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("SEND mode: {err}")))?;
    Ok((path, mode))
}

fn create_sync_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

fn set_sync_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
}

fn set_sync_mtime(path: &Path, mtime: u32) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains interior NUL"))?;
    let times = [
        libc::timespec {
            tv_sec: mtime as libc::time_t,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: mtime as libc::time_t,
            tv_nsec: 0,
        },
    ];
    if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn sync_status_packet(id: &[u8; 4], value: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(8);
    packet.extend_from_slice(id);
    packet.extend_from_slice(&value.to_le_bytes());
    packet
}

fn sync_stat_packet(mode: u32, size: u32, mtime: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(16);
    packet.extend_from_slice(SYNC_STAT);
    packet.extend_from_slice(&mode.to_le_bytes());
    packet.extend_from_slice(&size.to_le_bytes());
    packet.extend_from_slice(&mtime.to_le_bytes());
    packet
}

fn sync_dent_packet(mode: u32, size: u32, mtime: u32, name: &[u8]) -> io::Result<Vec<u8>> {
    let name_len = u32::try_from(name.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "sync directory entry name too long",
        )
    })?;
    let mut packet = Vec::with_capacity(20 + name.len());
    packet.extend_from_slice(SYNC_DENT);
    packet.extend_from_slice(&mode.to_le_bytes());
    packet.extend_from_slice(&size.to_le_bytes());
    packet.extend_from_slice(&mtime.to_le_bytes());
    packet.extend_from_slice(&name_len.to_le_bytes());
    packet.extend_from_slice(name);
    Ok(packet)
}

fn sync_list_done_packet() -> Vec<u8> {
    let mut packet = Vec::with_capacity(20);
    packet.extend_from_slice(SYNC_DONE);
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet
}

fn sync_data_packet(data: &[u8]) -> io::Result<Vec<u8>> {
    let data_len = u32::try_from(data.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "sync data packet too large"))?;
    let mut packet = Vec::with_capacity(8 + data.len());
    packet.extend_from_slice(SYNC_DATA);
    packet.extend_from_slice(&data_len.to_le_bytes());
    packet.extend_from_slice(data);
    Ok(packet)
}

fn sync_fail_packet(message: &str) -> io::Result<Vec<u8>> {
    let message = message.as_bytes();
    let message_len = u32::try_from(message.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "sync failure message too long")
    })?;
    let mut packet = Vec::with_capacity(8 + message.len());
    packet.extend_from_slice(SYNC_FAIL);
    packet.extend_from_slice(&message_len.to_le_bytes());
    packet.extend_from_slice(message);
    Ok(packet)
}

fn sync_id_name(id: &[u8; 4]) -> String {
    String::from_utf8_lossy(id).into_owned()
}

fn read_fd(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match file.read(buffer) {
            Ok(read) => return Ok(read),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
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
        assert_eq!(adb_service("shell:"), Some(AdbService::Shell(None)));
        assert_eq!(adb_service("shell"), Some(AdbService::Shell(None)));
    }

    #[test]
    fn recognizes_command_shell_service() {
        assert_eq!(
            adb_service("shell:ls /"),
            Some(AdbService::Shell(Some("ls /".to_string())))
        );
    }

    #[test]
    fn recognizes_exec_service() {
        assert_eq!(
            adb_service("exec:cat /proc/cmdline"),
            Some(AdbService::Exec("cat /proc/cmdline".to_string()))
        );
    }

    #[test]
    fn recognizes_sync_service() {
        assert_eq!(adb_service("sync:"), Some(AdbService::Sync));
    }

    #[test]
    fn rejects_non_shell_service() {
        assert_eq!(adb_service("host:version"), None);
    }

    #[test]
    fn parses_send_target_with_commas_in_path() {
        let (path, mode) = parse_send_target(b"/tmp/file,with,commas,33206").unwrap();

        assert_eq!(path.as_os_str().as_bytes(), b"/tmp/file,with,commas");
        assert_eq!(mode, 33206);
    }

    #[test]
    fn builds_sync_stat_packet() {
        assert_eq!(
            sync_stat_packet(0o100644, 123, 456),
            b"STAT\xA4\x81\x00\x00{\x00\x00\x00\xC8\x01\x00\x00".to_vec()
        );
    }
}
