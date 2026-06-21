use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    mem,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    slice,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use drm::{
    Device as DrmDevice,
    buffer::{Buffer, DrmFourcc},
    control::{
        self, Device as ControlDevice, Event, ModeTypeFlags, PageFlipFlags, connector, crtc,
        framebuffer,
    },
};
use slint::platform::{
    Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent,
    software_renderer::{
        MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, Rgb565Pixel,
        SoftwareRenderer, TargetPixel,
    },
};
use slint::{ComponentHandle, LogicalPosition, ModelRc, PhysicalSize, SharedString, VecModel};

use crate::{battery, power};

slint::include_modules!();

const DRI: &str = "/dev/dri";
const INPUT: &str = "/dev/input";
const UI_START_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_SLEEP: Duration = Duration::from_millis(16);
const POWER_KEY_HOLD: Duration = Duration::from_millis(2500);

#[derive(Clone, Debug)]
pub(crate) struct SystemInfo {
    pub(crate) device_name: String,
    pub(crate) device_detail: String,
    pub(crate) serialno: String,
}

#[derive(Clone, Debug)]
pub(crate) struct BootMenuEntryInfo {
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) detail: String,
    pub(crate) badge: String,
}

#[derive(Debug)]
pub(crate) enum Action {
    BootEntry(usize),
}

pub(crate) struct Handle {
    commands: async_channel::Sender<Command>,
    actions: async_channel::Receiver<Action>,
    _thread: thread::JoinHandle<()>,
}

impl Handle {
    pub(crate) fn update_boot_entries(&self, entries: Vec<BootMenuEntryInfo>, scan_complete: bool) {
        if self
            .commands
            .try_send(Command::SetBootEntries {
                entries,
                scan_complete,
            })
            .is_err()
        {
            tracing::debug!("UI command channel disconnected");
        }
    }

    pub(crate) fn action_receiver(&self) -> async_channel::Receiver<Action> {
        self.actions.clone()
    }
}

enum Command {
    SetBootEntries {
        entries: Vec<BootMenuEntryInfo>,
        scan_complete: bool,
    },
}

pub(crate) fn spawn(
    battery: Option<battery::Updates>,
    system_info: SystemInfo,
) -> io::Result<Handle> {
    let (command_tx, command_rx) = async_channel::unbounded();
    let (action_tx, action_rx) = async_channel::unbounded();
    let handle = thread::Builder::new()
        .name("pocketboot-ui".to_string())
        .spawn(move || {
            if let Err(err) = run(battery, system_info, command_rx, action_tx) {
                tracing::warn!(error = %err, "UI thread exited");
            }
        })?;
    tracing::info!(thread = "pocketboot-ui", "UI thread spawned");
    Ok(Handle {
        commands: command_tx,
        actions: action_rx,
        _thread: handle,
    })
}

fn run(
    battery: Option<battery::Updates>,
    system_info: SystemInfo,
    commands: async_channel::Receiver<Command>,
    actions: async_channel::Sender<Action>,
) -> Result<(), String> {
    let mut kms_display = KmsDisplay::wait_open(UI_START_TIMEOUT)?;
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);

    slint::platform::set_platform(Box::new(PocketPlatform::new(window.clone())))
        .map_err(|err| format!("install Slint platform: {err}"))?;
    window.set_size(PhysicalSize::new(kms_display.width, kms_display.height));

    let main_window = MainWindow::new().map_err(|err| format!("create Slint window: {err}"))?;
    main_window.set_device_name(system_info.device_name.into());
    main_window.set_device_detail(system_info.device_detail.into());
    main_window.set_serialno(system_info.serialno.into());
    main_window.on_power_action(|action| {
        let result = match action {
            PowerAction::Shutdown => power::power_off(),
            PowerAction::Reboot => power::reboot(),
        };
        if let Err(err) = result {
            tracing::error!(action = ?action, error = %err, "power action failed");
        }
    });
    main_window.on_boot_entry_selected(move |index| {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        if actions.try_send(Action::BootEntry(index)).is_err() {
            tracing::debug!("UI action receiver disconnected");
        }
    });
    main_window
        .show()
        .map_err(|err| format!("show Slint window: {err}"))?;

    let mut touch = TouchInput::new();
    let mut power_key = PowerKeyInput::new();
    let mut battery = battery.map(BatteryUpdates::new);
    let mut commands = UiCommands::new(commands);
    let mut pointer_down = false;
    let display_path = kms_display.path.display().to_string();
    let connector = kms_display.connector.to_string();
    tracing::info!(
        path = %display_path,
        connector = %connector,
        width = kms_display.width,
        height = kms_display.height,
        format = ?kms_display.format,
        "starting frankenSlint UI"
    );

    loop {
        slint::platform::update_timers_and_animations();

        if let Some(battery) = &mut battery {
            battery.poll(&main_window);
        }
        commands.poll(&main_window);

        for report in touch.poll(kms_display.width, kms_display.height) {
            let position = LogicalPosition::new(report.x, report.y);
            match report.kind {
                TouchKind::Down => {
                    pointer_down = true;
                    window.dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
                }
                TouchKind::Move => {
                    window.dispatch_event(WindowEvent::PointerMoved { position });
                }
                TouchKind::Up => {
                    if pointer_down {
                        pointer_down = false;
                        window.dispatch_event(WindowEvent::PointerReleased {
                            position,
                            button: PointerEventButton::Left,
                        });
                        window.dispatch_event(WindowEvent::PointerExited);
                    }
                }
            }
        }

        for event in power_key.poll() {
            match event {
                PowerKeyEvent::OpenMenu => main_window.invoke_show_power_menu(),
            }
        }

        if !kms_display.draw_if_needed(&window)? {
            thread::sleep(IDLE_SLEEP);
        }
    }
}

struct UiCommands {
    rx: async_channel::Receiver<Command>,
    disconnected: bool,
}

impl UiCommands {
    fn new(rx: async_channel::Receiver<Command>) -> Self {
        Self {
            rx,
            disconnected: false,
        }
    }

    fn poll(&mut self, window: &MainWindow) {
        if self.disconnected {
            return;
        }

        loop {
            match self.rx.try_recv() {
                Ok(command) => apply_command(window, command),
                Err(async_channel::TryRecvError::Empty) => return,
                Err(async_channel::TryRecvError::Closed) => {
                    self.disconnected = true;
                    tracing::debug!("UI command channel disconnected");
                    return;
                }
            }
        }
    }
}

fn apply_command(window: &MainWindow, command: Command) {
    match command {
        Command::SetBootEntries {
            entries,
            scan_complete,
        } => apply_boot_entries(window, entries, scan_complete),
    }
}

fn apply_boot_entries(window: &MainWindow, entries: Vec<BootMenuEntryInfo>, scan_complete: bool) {
    let count = entries.len();
    let rows = entries
        .into_iter()
        .map(|entry| BootMenuEntry {
            title: SharedString::from(entry.title),
            subtitle: SharedString::from(entry.subtitle),
            detail: SharedString::from(entry.detail),
            badge: SharedString::from(entry.badge),
        })
        .collect::<Vec<_>>();

    window.set_boot_entries(ModelRc::new(VecModel::from(rows)));
    window.set_boot_entry_count(i32::try_from(count).unwrap_or(i32::MAX));
    window.set_boot_scan_complete(scan_complete);
    window.set_booting_index(-1);
}

struct BatteryUpdates {
    rx: mpsc::Receiver<Option<battery::Snapshot>>,
    disconnected: bool,
}

impl BatteryUpdates {
    fn new(rx: mpsc::Receiver<Option<battery::Snapshot>>) -> Self {
        Self {
            rx,
            disconnected: false,
        }
    }

    fn poll(&mut self, window: &MainWindow) {
        if self.disconnected {
            return;
        }

        loop {
            match self.rx.try_recv() {
                Ok(snapshot) => apply_battery(window, snapshot),
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    tracing::debug!("battery watcher disconnected");
                    return;
                }
            }
        }
    }
}

fn apply_battery(window: &MainWindow, snapshot: Option<battery::Snapshot>) {
    let Some(snapshot) = snapshot else {
        window.set_battery_known(false);
        return;
    };

    window.set_battery_known(true);
    window.set_battery_percent(snapshot.percent.into());
    window.set_battery_status(match snapshot.status {
        battery::Status::Unknown => BatteryStatus::Unknown,
        battery::Status::Charging => BatteryStatus::Charging,
        battery::Status::Discharging => BatteryStatus::Discharging,
        battery::Status::NotCharging => BatteryStatus::NotCharging,
        battery::Status::Full => BatteryStatus::Full,
    });
}

struct PocketPlatform {
    window: Rc<MinimalSoftwareWindow>,
    start: Instant,
}

impl PocketPlatform {
    fn new(window: Rc<MinimalSoftwareWindow>) -> Self {
        Self {
            window,
            start: Instant::now(),
        }
    }
}

impl Platform for PocketPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window: Rc<dyn WindowAdapter> = self.window.clone();
        Ok(window)
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }
}

struct KmsDisplay {
    card: DrmCard,
    path: PathBuf,
    connector: connector::Info,
    crtc: crtc::Handle,
    mode: control::Mode,
    width: u32,
    height: u32,
    format: DrmFourcc,
    front_buffer: KmsBuffer,
    back_buffer: KmsBuffer,
    in_flight_buffer: KmsBuffer,
    posted: bool,
    page_flip_pending: bool,
}

impl KmsDisplay {
    fn wait_open(timeout: Duration) -> Result<Self, String> {
        let start = Instant::now();
        loop {
            match Self::open_any() {
                Ok(display) => return Ok(display),
                Err(err) if start.elapsed() < timeout => {
                    tracing::debug!(error = %err, "DRM display not ready");
                    thread::sleep(Duration::from_millis(50));
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn open_any() -> Result<Self, String> {
        let entries = fs::read_dir(DRI).map_err(|err| format!("open {DRI}: {err}"))?;
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("card"))
            })
            .collect::<Vec<_>>();
        paths.sort();

        if paths.is_empty() {
            return Err(format!("no DRM card devices found under {DRI}"));
        }

        let mut errors = Vec::new();
        for path in paths {
            match Self::open_path(&path) {
                Ok(display) => return Ok(display),
                Err(err) => errors.push(format!("{}: {err}", path.display())),
            }
        }

        Err(format!(
            "could not initialize DRM display: {}",
            errors.join("; ")
        ))
    }

    fn open_path(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
            .map_err(|err| format!("open: {err}"))?;
        let card = DrmCard(file);
        let _ = card.set_client_capability(drm::ClientCapability::UniversalPlanes, true);

        let resources = card
            .resource_handles()
            .map_err(|err| format!("query DRM resources: {err}"))?;
        let connector = choose_connector(&card, &resources)?;
        let mode = choose_mode(&connector)?;
        let crtc = choose_crtc(&card, &resources, &connector)?;
        let format = choose_format(&card, &resources, crtc)?;
        let (width, height) = mode.size();
        let size = (width as u32, height as u32);

        let front_buffer = KmsBuffer::allocate(&card, size, format)?;
        let back_buffer = KmsBuffer::allocate(&card, size, format)?;
        let in_flight_buffer = KmsBuffer::allocate(&card, size, format)?;

        Ok(Self {
            card,
            path: path.to_path_buf(),
            connector,
            crtc,
            mode,
            width: size.0,
            height: size.1,
            format,
            front_buffer,
            back_buffer,
            in_flight_buffer,
            posted: false,
            page_flip_pending: false,
        })
    }

    fn draw_if_needed(&mut self, window: &MinimalSoftwareWindow) -> Result<bool, String> {
        let mut render_result = Ok(());
        let redraw = window.draw_if_needed(|renderer| {
            render_result = self.render(renderer);
        });

        render_result?;

        if redraw {
            self.present()?;
        }

        Ok(redraw)
    }

    fn render(&mut self, renderer: &SoftwareRenderer) -> Result<(), String> {
        let format = self.format;
        let stride = self.back_buffer.buffer.pitch() as usize / bytes_per_pixel(format);
        let repaint_buffer_type = repaint_buffer_type_for_age(self.back_buffer.age);
        let mut mapping = self
            .card
            .map_dumb_buffer(&mut self.back_buffer.buffer)
            .map_err(|err| format!("map DRM dumb buffer: {err}"))?;
        let buffer = mapping.as_mut();

        renderer.set_repaint_buffer_type(repaint_buffer_type);
        match format {
            DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888 => {
                let pixels = cast_buffer_mut::<Xrgb8888Pixel>(buffer, format)?;
                renderer.render(pixels, stride);
            }
            DrmFourcc::Bgra8888 => {
                let pixels = cast_buffer_mut::<Bgra8888Pixel>(buffer, format)?;
                renderer.render(pixels, stride);
            }
            DrmFourcc::Rgb565 => {
                let pixels = cast_buffer_mut::<Rgb565Pixel>(buffer, format)?;
                renderer.render(pixels, stride);
            }
            _ => unreachable!("unsupported DRM format was selected"),
        }

        Ok(())
    }

    fn present(&mut self) -> Result<(), String> {
        self.wait_for_page_flip()?;

        mem::swap(&mut self.back_buffer, &mut self.front_buffer);
        mem::swap(&mut self.front_buffer, &mut self.in_flight_buffer);

        self.in_flight_buffer.age = 1;
        for buffer in [&mut self.back_buffer, &mut self.front_buffer] {
            if buffer.age != 0 {
                buffer.age = buffer.age.saturating_add(1);
            }
        }

        if self.posted {
            self.card
                .page_flip(
                    self.crtc,
                    self.in_flight_buffer.framebuffer,
                    PageFlipFlags::EVENT,
                    None,
                )
                .map_err(|err| format!("page flip DRM buffer: {err}"))?;
            self.page_flip_pending = true;
        } else {
            self.card
                .set_crtc(
                    self.crtc,
                    Some(self.in_flight_buffer.framebuffer),
                    (0, 0),
                    &[self.connector.handle()],
                    Some(self.mode),
                )
                .map_err(|err| format!("set DRM CRTC: {err}"))?;
            self.posted = true;
        }

        Ok(())
    }

    fn wait_for_page_flip(&mut self) -> Result<(), String> {
        if !self.page_flip_pending {
            return Ok(());
        }

        loop {
            let events = self
                .card
                .receive_events()
                .map_err(|err| format!("receive DRM events: {err}"))?;
            if events.into_iter().any(
                |event| matches!(event, Event::PageFlip(page_flip) if page_flip.crtc == self.crtc),
            ) {
                self.page_flip_pending = false;
                return Ok(());
            }
        }
    }
}

struct DrmCard(File);

impl AsFd for DrmCard {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl DrmDevice for DrmCard {}
impl ControlDevice for DrmCard {}

struct KmsBuffer {
    framebuffer: framebuffer::Handle,
    buffer: control::dumbbuffer::DumbBuffer,
    age: u8,
}

impl KmsBuffer {
    fn allocate(card: &DrmCard, size: (u32, u32), format: DrmFourcc) -> Result<Self, String> {
        let (depth, bpp) = pixel_format_params(format)?;
        let buffer = card
            .create_dumb_buffer(size, format, bpp)
            .map_err(|err| format!("create DRM dumb buffer: {err}"))?;
        let framebuffer = card
            .add_framebuffer(&buffer, depth, bpp)
            .map_err(|err| format!("create DRM framebuffer object: {err}"))?;

        Ok(Self {
            framebuffer,
            buffer,
            age: 0,
        })
    }
}

fn choose_connector(
    card: &DrmCard,
    resources: &control::ResourceHandles,
) -> Result<connector::Info, String> {
    let mut fallback = None;

    for handle in resources.connectors() {
        let connector = card
            .get_connector(*handle, true)
            .map_err(|err| format!("query DRM connector {handle:?}: {err}"))?;
        if connector.modes().is_empty() {
            continue;
        }
        if connector.state() == connector::State::Connected {
            return Ok(connector);
        }
        fallback.get_or_insert(connector);
    }

    fallback.ok_or_else(|| "no DRM connector with modes found".to_string())
}

fn choose_mode(connector: &connector::Info) -> Result<control::Mode, String> {
    connector
        .modes()
        .iter()
        .max_by_key(|mode| {
            let (width, height) = mode.size();
            (
                mode.mode_type().contains(ModeTypeFlags::PREFERRED),
                width as u32 * height as u32,
            )
        })
        .copied()
        .ok_or_else(|| format!("DRM connector {connector} reported no modes"))
}

fn choose_crtc(
    card: &DrmCard,
    resources: &control::ResourceHandles,
    connector: &connector::Info,
) -> Result<crtc::Handle, String> {
    if let Some(crtc) = connector
        .current_encoder()
        .and_then(|encoder| card.get_encoder(encoder).ok())
        .and_then(|encoder| encoder.crtc())
    {
        return Ok(crtc);
    }

    connector
        .encoders()
        .iter()
        .filter_map(|encoder| card.get_encoder(*encoder).ok())
        .flat_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
        .find(|crtc| card.get_crtc(*crtc).is_ok())
        .ok_or_else(|| format!("no compatible DRM CRTC found for connector {connector}"))
}

fn choose_format(
    card: &DrmCard,
    resources: &control::ResourceHandles,
    crtc: crtc::Handle,
) -> Result<DrmFourcc, String> {
    let available = supported_formats(card, resources, crtc);
    for format in [
        DrmFourcc::Xrgb8888,
        DrmFourcc::Argb8888,
        DrmFourcc::Bgra8888,
        DrmFourcc::Rgb565,
    ] {
        if available.contains(&format) {
            return Ok(format);
        }
    }

    Err(format!(
        "no supported DRM format found; available formats: {available:?}"
    ))
}

fn supported_formats(
    card: &DrmCard,
    resources: &control::ResourceHandles,
    crtc: crtc::Handle,
) -> Vec<DrmFourcc> {
    let mut formats = Vec::new();

    if let Ok(planes) = card.plane_handles() {
        for plane in planes {
            let Ok(plane) = card.get_plane(plane) else {
                continue;
            };
            let compatible = plane.crtc() == Some(crtc)
                || resources
                    .filter_crtcs(plane.possible_crtcs())
                    .iter()
                    .any(|candidate| *candidate == crtc);
            if !compatible {
                continue;
            }

            for format in plane.formats() {
                if let Ok(format) = DrmFourcc::try_from(*format) {
                    if !formats.contains(&format) {
                        formats.push(format);
                    }
                }
            }
        }
    }

    if formats.is_empty() {
        formats.push(DrmFourcc::Xrgb8888);
    }

    formats
}

fn repaint_buffer_type_for_age(age: u8) -> RepaintBufferType {
    match age {
        1 => RepaintBufferType::ReusedBuffer,
        2 => RepaintBufferType::SwappedBuffers,
        _ => RepaintBufferType::NewBuffer,
    }
}

fn bytes_per_pixel(format: DrmFourcc) -> usize {
    match format {
        DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888 | DrmFourcc::Bgra8888 => 4,
        DrmFourcc::Rgb565 => 2,
        _ => unreachable!("unsupported DRM format was selected"),
    }
}

fn pixel_format_params(format: DrmFourcc) -> Result<(u32, u32), String> {
    match format {
        DrmFourcc::Xrgb8888 => Ok((24, 32)),
        DrmFourcc::Argb8888 | DrmFourcc::Bgra8888 => Ok((32, 32)),
        DrmFourcc::Rgb565 => Ok((16, 16)),
        _ => Err(format!("unsupported DRM format {format:?}")),
    }
}

fn cast_buffer_mut<T>(buffer: &mut [u8], format: DrmFourcc) -> Result<&mut [T], String> {
    let pixel_size = mem::size_of::<T>();
    if buffer.len() % pixel_size != 0 {
        return Err(format!(
            "DRM buffer length is not aligned for {format:?}: {} bytes",
            buffer.len()
        ));
    }
    if buffer.as_ptr() as usize % mem::align_of::<T>() != 0 {
        return Err(format!("DRM buffer mapping is not aligned for {format:?}"));
    }

    Ok(unsafe { slice::from_raw_parts_mut(buffer.as_mut_ptr().cast(), buffer.len() / pixel_size) })
}

#[repr(transparent)]
#[derive(Clone, Copy, Default)]
struct Xrgb8888Pixel(u32);

impl Xrgb8888Pixel {
    fn rgb(self) -> (u8, u8, u8) {
        (
            ((self.0 >> 16) & 0xff) as u8,
            ((self.0 >> 8) & 0xff) as u8,
            (self.0 & 0xff) as u8,
        )
    }
}

impl TargetPixel for Xrgb8888Pixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let (red, green, blue) = self.rgb();
        let inv_alpha = (u8::MAX - color.alpha) as u16;
        *self = Self::from_rgb(
            (red as u16 * inv_alpha / 255) as u8 + color.red,
            (green as u16 * inv_alpha / 255) as u8 + color.green,
            (blue as u16 * inv_alpha / 255) as u8 + color.blue,
        );
    }

    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(0xff00_0000 | ((red as u32) << 16) | ((green as u32) << 8) | blue as u32)
    }

    fn background() -> Self {
        Self(0)
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Default)]
struct Bgra8888Pixel(u32);

impl Bgra8888Pixel {
    fn rgba(self) -> PremultipliedRgbaColor {
        PremultipliedRgbaColor {
            red: (self.0 >> 8) as u8,
            green: (self.0 >> 16) as u8,
            blue: (self.0 >> 24) as u8,
            alpha: self.0 as u8,
        }
    }
}

impl TargetPixel for Bgra8888Pixel {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let mut background = self.rgba();
        background.blend(color);
        *self = Self(
            background.alpha as u32
                | ((background.red as u32) << 8)
                | ((background.green as u32) << 16)
                | ((background.blue as u32) << 24),
        );
    }

    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(0xff | ((red as u32) << 8) | ((green as u32) << 16) | ((blue as u32) << 24))
    }

    fn background() -> Self {
        Self(0)
    }
}

struct TouchInput {
    devices: Vec<TouchDevice>,
    ignored_devices: Vec<PathBuf>,
    next_scan: Instant,
}

impl TouchInput {
    fn new() -> Self {
        let mut input = Self {
            devices: Vec::new(),
            ignored_devices: Vec::new(),
            next_scan: Instant::now(),
        };
        input.rescan();
        input
    }

    fn poll(&mut self, width: u32, height: u32) -> Vec<TouchReport> {
        if Instant::now() >= self.next_scan {
            self.rescan();
        }

        let mut reports = Vec::new();
        let mut index = 0;
        while index < self.devices.len() {
            match self.devices[index].poll(width, height, &mut reports) {
                Ok(()) => index += 1,
                Err(err) => {
                    tracing::warn!(path = %self.devices[index].path.display(), error = %err, "dropping touch input device");
                    self.devices.swap_remove(index);
                }
            }
        }
        reports
    }

    fn rescan(&mut self) {
        self.next_scan = Instant::now() + Duration::from_secs(1);
        let Ok(entries) = fs::read_dir(INPUT) else {
            return;
        };

        let mut present = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
            {
                continue;
            }
            present.push(path.clone());
            if self.devices.iter().any(|device| device.path == path) {
                continue;
            }
            if self.ignored_devices.iter().any(|ignored| *ignored == path) {
                continue;
            }
            match TouchDevice::open(&path) {
                Ok(device) => {
                    tracing::info!(path = %path.display(), "opened touch input device");
                    self.devices.push(device);
                }
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "ignoring input device");
                    self.ignored_devices.push(path);
                }
            }
        }

        self.ignored_devices
            .retain(|ignored| present.iter().any(|path| path == ignored));
    }
}

struct PowerKeyInput {
    devices: Vec<PowerKeyDevice>,
    ignored_devices: Vec<PathBuf>,
    next_scan: Instant,
}

impl PowerKeyInput {
    fn new() -> Self {
        let mut input = Self {
            devices: Vec::new(),
            ignored_devices: Vec::new(),
            next_scan: Instant::now(),
        };
        input.rescan();
        input
    }

    fn poll(&mut self) -> Vec<PowerKeyEvent> {
        if Instant::now() >= self.next_scan {
            self.rescan();
        }

        let mut events = Vec::new();
        let mut index = 0;
        while index < self.devices.len() {
            match self.devices[index].poll(&mut events) {
                Ok(()) => index += 1,
                Err(err) => {
                    tracing::warn!(path = %self.devices[index].path.display(), error = %err, "dropping power-key input device");
                    self.devices.swap_remove(index);
                }
            }
        }
        events
    }

    fn rescan(&mut self) {
        self.next_scan = Instant::now() + Duration::from_secs(1);
        let Ok(entries) = fs::read_dir(INPUT) else {
            return;
        };

        let mut present = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
            {
                continue;
            }
            present.push(path.clone());
            if self.devices.iter().any(|device| device.path == path) {
                continue;
            }
            if self.ignored_devices.iter().any(|ignored| *ignored == path) {
                continue;
            }
            match PowerKeyDevice::open(&path) {
                Ok(device) => {
                    tracing::info!(path = %path.display(), "opened power-key input device");
                    self.devices.push(device);
                }
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "ignoring input device");
                    self.ignored_devices.push(path);
                }
            }
        }

        self.ignored_devices
            .retain(|ignored| present.iter().any(|path| path == ignored));
    }
}

struct PowerKeyDevice {
    file: File,
    path: PathBuf,
    state: PowerKeyState,
}

impl PowerKeyDevice {
    fn open(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|err| format!("open: {err}"))?;
        let fd = file.as_raw_fd();

        if !query_key(fd, KEY_POWER).map_err(|err| format!("query KEY_POWER: {err}"))? {
            return Err("device does not advertise KEY_POWER".to_string());
        }

        Ok(Self {
            file,
            path: path.to_path_buf(),
            state: PowerKeyState::default(),
        })
    }

    fn poll(&mut self, events: &mut Vec<PowerKeyEvent>) -> io::Result<()> {
        let mut buffer = [0u8; mem::size_of::<InputEvent>() * 32];
        loop {
            match self.file.read(&mut buffer) {
                Ok(0) => break,
                Ok(bytes) => {
                    let whole_events = bytes / mem::size_of::<InputEvent>();
                    for chunk in buffer[..whole_events * mem::size_of::<InputEvent>()]
                        .chunks_exact(mem::size_of::<InputEvent>())
                    {
                        let event =
                            unsafe { ptr::read_unaligned(chunk.as_ptr().cast::<InputEvent>()) };
                        self.state.handle(event);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }

        if let Some(event) = self.state.poll() {
            events.push(event);
        }
        Ok(())
    }
}

#[derive(Default)]
struct PowerKeyState {
    pressed_since: Option<Instant>,
    reported: bool,
}

impl PowerKeyState {
    fn handle(&mut self, event: InputEvent) {
        if event.type_ != EV_KEY || event.code != KEY_POWER {
            return;
        }

        match event.value {
            0 => {
                self.pressed_since = None;
                self.reported = false;
            }
            1 => {
                self.pressed_since = Some(Instant::now());
                self.reported = false;
            }
            _ => {}
        }
    }

    fn poll(&mut self) -> Option<PowerKeyEvent> {
        let pressed_since = self.pressed_since?;
        if self.reported || pressed_since.elapsed() < POWER_KEY_HOLD {
            return None;
        }

        self.reported = true;
        Some(PowerKeyEvent::OpenMenu)
    }
}

enum PowerKeyEvent {
    OpenMenu,
}

struct TouchDevice {
    file: File,
    path: PathBuf,
    x_axis: Axis,
    y_axis: Axis,
    state: TouchState,
}

impl TouchDevice {
    fn open(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|err| format!("open: {err}"))?;
        let fd = file.as_raw_fd();

        let x_axis = query_axis(fd, ABS_MT_POSITION_X)
            .or_else(|_| query_axis(fd, ABS_X))
            .map_err(|err| format!("query x axis: {err}"))?;
        let y_axis = query_axis(fd, ABS_MT_POSITION_Y)
            .or_else(|_| query_axis(fd, ABS_Y))
            .map_err(|err| format!("query y axis: {err}"))?;

        Ok(Self {
            file,
            path: path.to_path_buf(),
            x_axis,
            y_axis,
            state: TouchState::default(),
        })
    }

    fn poll(&mut self, width: u32, height: u32, reports: &mut Vec<TouchReport>) -> io::Result<()> {
        let mut buffer = [0u8; mem::size_of::<InputEvent>() * 32];
        loop {
            match self.file.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(bytes) => {
                    let whole_events = bytes / mem::size_of::<InputEvent>();
                    for chunk in buffer[..whole_events * mem::size_of::<InputEvent>()]
                        .chunks_exact(mem::size_of::<InputEvent>())
                    {
                        let event =
                            unsafe { ptr::read_unaligned(chunk.as_ptr().cast::<InputEvent>()) };
                        if let Some(report) =
                            self.state
                                .handle(event, self.x_axis, self.y_axis, width, height)
                        {
                            reports.push(report);
                        }
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Axis {
    code: u16,
    min: i32,
    max: i32,
}

impl Axis {
    fn scale(self, value: i32, size: u32) -> f32 {
        let span = (self.max - self.min).max(1) as f32;
        let value = ((value - self.min) as f32 / span).clamp(0.0, 1.0);
        value * (size.saturating_sub(1) as f32)
    }
}

#[derive(Default)]
struct TouchState {
    current_slot: usize,
    slots: [TouchSlot; MAX_TOUCH_SLOTS],
    legacy_x: Option<i32>,
    legacy_y: Option<i32>,
    legacy_down: bool,
    was_down: bool,
    last_point: Option<(i32, i32)>,
}

impl TouchState {
    fn handle(
        &mut self,
        event: InputEvent,
        x_axis: Axis,
        y_axis: Axis,
        width: u32,
        height: u32,
    ) -> Option<TouchReport> {
        match event.type_ {
            EV_ABS => self.handle_abs(event.code, event.value),
            EV_KEY if event.code == BTN_TOUCH => self.handle_touch_button(event.value != 0),
            EV_SYN if event.code == SYN_REPORT => {
                return self.report(x_axis, y_axis, width, height);
            }
            _ => {}
        }
        None
    }

    fn handle_abs(&mut self, code: u16, value: i32) {
        match code {
            ABS_MT_SLOT => {
                if (0..MAX_TOUCH_SLOTS as i32).contains(&value) {
                    self.current_slot = value as usize;
                }
            }
            ABS_MT_TRACKING_ID => {
                self.slots[self.current_slot].active = value >= 0;
            }
            ABS_MT_POSITION_X => self.slots[self.current_slot].x = Some(value),
            ABS_MT_POSITION_Y => self.slots[self.current_slot].y = Some(value),
            ABS_X => self.legacy_x = Some(value),
            ABS_Y => self.legacy_y = Some(value),
            _ => {}
        }
    }

    fn handle_touch_button(&mut self, down: bool) {
        self.legacy_down = down;
        if !down {
            for slot in &mut self.slots {
                slot.active = false;
            }
        }
    }

    fn report(
        &mut self,
        x_axis: Axis,
        y_axis: Axis,
        width: u32,
        height: u32,
    ) -> Option<TouchReport> {
        let active_point = self.active_point(x_axis, y_axis);
        match (active_point, self.was_down) {
            (Some(point), false) => {
                self.was_down = true;
                self.last_point = Some(point);
                Some(TouchReport::new(
                    TouchKind::Down,
                    point,
                    x_axis,
                    y_axis,
                    width,
                    height,
                ))
            }
            (Some(point), true) => {
                self.last_point = Some(point);
                Some(TouchReport::new(
                    TouchKind::Move,
                    point,
                    x_axis,
                    y_axis,
                    width,
                    height,
                ))
            }
            (None, true) => {
                self.was_down = false;
                let point = self.last_point?;
                Some(TouchReport::new(
                    TouchKind::Up,
                    point,
                    x_axis,
                    y_axis,
                    width,
                    height,
                ))
            }
            (None, false) => None,
        }
    }

    fn active_point(&self, x_axis: Axis, y_axis: Axis) -> Option<(i32, i32)> {
        if x_axis.code == ABS_MT_POSITION_X || y_axis.code == ABS_MT_POSITION_Y {
            for slot in self.slots {
                if slot.active {
                    if let (Some(x), Some(y)) = (slot.x, slot.y) {
                        return Some((x, y));
                    }
                }
            }
        }

        if self.legacy_down {
            if let (Some(x), Some(y)) = (self.legacy_x, self.legacy_y) {
                return Some((x, y));
            }
        }

        None
    }
}

#[derive(Clone, Copy, Default)]
struct TouchSlot {
    active: bool,
    x: Option<i32>,
    y: Option<i32>,
}

#[derive(Clone, Copy)]
struct TouchReport {
    kind: TouchKind,
    x: f32,
    y: f32,
}

impl TouchReport {
    fn new(
        kind: TouchKind,
        point: (i32, i32),
        x_axis: Axis,
        y_axis: Axis,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            kind,
            x: x_axis.scale(point.0, width),
            y: y_axis.scale(point.1, height),
        }
    }
}

#[derive(Clone, Copy)]
enum TouchKind {
    Down,
    Move,
    Up,
}

fn query_axis(fd: i32, code: u16) -> io::Result<Axis> {
    let mut info = InputAbsInfo::default();
    ioctl_read(fd, eviocgabs(code), &mut info)?;
    if info.maximum <= info.minimum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty ABS axis range",
        ));
    }
    Ok(Axis {
        code,
        min: info.minimum,
        max: info.maximum,
    })
}

fn query_key(fd: i32, code: u16) -> io::Result<bool> {
    let mut bits = [0u8; KEY_POWER_BITS_BYTES];
    ioctl_read(fd, eviocgbit(EV_KEY, bits.len()), &mut bits)?;
    Ok(test_bit(&bits, code))
}

fn test_bit(bits: &[u8], bit: u16) -> bool {
    let index = bit as usize / 8;
    let mask = 1 << (bit as usize % 8);
    bits.get(index).is_some_and(|byte| byte & mask != 0)
}

fn ioctl_read<T>(fd: i32, request: u64, value: &mut T) -> io::Result<()> {
    let rc = unsafe { libc::ioctl(fd, request as _, value as *mut T) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn eviocgabs(abs: u16) -> u64 {
    ior(b'E', 0x40 + abs as u8, mem::size_of::<InputAbsInfo>())
}

fn eviocgbit(type_: u16, size: usize) -> u64 {
    ior(b'E', 0x20 + type_ as u8, size)
}

fn ior(type_: u8, number: u8, size: usize) -> u64 {
    ioc(IOC_READ, type_, number, size)
}

fn ioc(direction: u8, type_: u8, number: u8, size: usize) -> u64 {
    ((direction as u64) << IOC_DIRSHIFT)
        | ((type_ as u64) << IOC_TYPESHIFT)
        | ((number as u64) << IOC_NRSHIFT)
        | ((size as u64) << IOC_SIZESHIFT)
}

const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_READ: u8 = 2;

const MAX_TOUCH_SLOTS: usize = 10;
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const KEY_POWER: u16 = 0x74;
const KEY_POWER_BITS_BYTES: usize = KEY_POWER as usize / 8 + 1;
const BTN_TOUCH: u16 = 0x14a;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
#[derive(Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[cfg(test)]
mod touch_tests {
    use super::*;

    const WIDTH: u32 = 101;
    const HEIGHT: u32 = 101;

    fn event(type_: u16, code: u16, value: i32) -> InputEvent {
        InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        }
    }

    fn axis(code: u16) -> Axis {
        Axis {
            code,
            min: 0,
            max: 1000,
        }
    }

    fn sync(state: &mut TouchState, x_axis: Axis, y_axis: Axis) -> Option<TouchReport> {
        state.handle(event(EV_SYN, SYN_REPORT, 0), x_axis, y_axis, WIDTH, HEIGHT)
    }

    #[test]
    fn legacy_touch_reports_release_after_button_up() {
        let mut state = TouchState::default();
        let x_axis = axis(ABS_X);
        let y_axis = axis(ABS_Y);

        state.handle(event(EV_KEY, BTN_TOUCH, 1), x_axis, y_axis, WIDTH, HEIGHT);
        state.handle(event(EV_ABS, ABS_X, 500), x_axis, y_axis, WIDTH, HEIGHT);
        state.handle(event(EV_ABS, ABS_Y, 500), x_axis, y_axis, WIDTH, HEIGHT);
        let report = sync(&mut state, x_axis, y_axis).expect("down report");
        assert!(matches!(report.kind, TouchKind::Down));

        state.handle(event(EV_KEY, BTN_TOUCH, 0), x_axis, y_axis, WIDTH, HEIGHT);
        let report = sync(&mut state, x_axis, y_axis).expect("up report");
        assert!(matches!(report.kind, TouchKind::Up));
    }

    #[test]
    fn mt_tracking_id_reports_release() {
        let mut state = TouchState::default();
        let x_axis = axis(ABS_MT_POSITION_X);
        let y_axis = axis(ABS_MT_POSITION_Y);

        state.handle(event(EV_ABS, ABS_MT_SLOT, 0), x_axis, y_axis, WIDTH, HEIGHT);
        state.handle(
            event(EV_ABS, ABS_MT_TRACKING_ID, 42),
            x_axis,
            y_axis,
            WIDTH,
            HEIGHT,
        );
        state.handle(
            event(EV_ABS, ABS_MT_POSITION_X, 500),
            x_axis,
            y_axis,
            WIDTH,
            HEIGHT,
        );
        state.handle(
            event(EV_ABS, ABS_MT_POSITION_Y, 500),
            x_axis,
            y_axis,
            WIDTH,
            HEIGHT,
        );
        let report = sync(&mut state, x_axis, y_axis).expect("down report");
        assert!(matches!(report.kind, TouchKind::Down));

        state.handle(event(EV_ABS, ABS_MT_SLOT, 0), x_axis, y_axis, WIDTH, HEIGHT);
        state.handle(
            event(EV_ABS, ABS_MT_TRACKING_ID, -1),
            x_axis,
            y_axis,
            WIDTH,
            HEIGHT,
        );
        let report = sync(&mut state, x_axis, y_axis).expect("up report");
        assert!(matches!(report.kind, TouchKind::Up));
    }

    #[test]
    fn button_up_releases_mt_slot_without_tracking_id_release() {
        let mut state = TouchState::default();
        let x_axis = axis(ABS_MT_POSITION_X);
        let y_axis = axis(ABS_MT_POSITION_Y);

        state.handle(event(EV_KEY, BTN_TOUCH, 1), x_axis, y_axis, WIDTH, HEIGHT);
        state.handle(event(EV_ABS, ABS_MT_SLOT, 0), x_axis, y_axis, WIDTH, HEIGHT);
        state.handle(
            event(EV_ABS, ABS_MT_TRACKING_ID, 42),
            x_axis,
            y_axis,
            WIDTH,
            HEIGHT,
        );
        state.handle(
            event(EV_ABS, ABS_MT_POSITION_X, 500),
            x_axis,
            y_axis,
            WIDTH,
            HEIGHT,
        );
        state.handle(
            event(EV_ABS, ABS_MT_POSITION_Y, 500),
            x_axis,
            y_axis,
            WIDTH,
            HEIGHT,
        );
        let report = sync(&mut state, x_axis, y_axis).expect("down report");
        assert!(matches!(report.kind, TouchKind::Down));

        state.handle(event(EV_KEY, BTN_TOUCH, 0), x_axis, y_axis, WIDTH, HEIGHT);
        let report = sync(&mut state, x_axis, y_axis).expect("up report");
        assert!(matches!(report.kind, TouchKind::Up));
    }
}
