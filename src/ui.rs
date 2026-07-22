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
const FRAME_RATE_WINDOW: Duration = Duration::from_secs(1);
const FRAME_RATE_GAP_RESET: Duration = Duration::from_millis(250);
const ANIMATION_LAB_SETTLE: Duration = Duration::from_millis(600);
const ANIMATION_LAB_BETWEEN_BUTTONS: Duration = Duration::from_millis(800);
const ANIMATION_LAB_HOLD: Duration = Duration::from_millis(1500);

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
    drm_page_flips: u32,
    ui_animation_lab: bool,
) -> io::Result<Handle> {
    let (command_tx, command_rx) = async_channel::unbounded();
    let (action_tx, action_rx) = async_channel::unbounded();
    let handle = thread::Builder::new()
        .name("pocketboot-ui".to_string())
        .spawn(move || {
            if let Err(err) = run(
                battery,
                system_info,
                drm_page_flips,
                ui_animation_lab,
                command_rx,
                action_tx,
            ) {
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
    drm_page_flips: u32,
    ui_animation_lab: bool,
    commands: async_channel::Receiver<Command>,
    actions: async_channel::Sender<Action>,
) -> Result<(), String> {
    let ui_start = Instant::now();
    let drm_open_start = Instant::now();
    let mut kms_display = KmsDisplay::wait_open(UI_START_TIMEOUT)?;
    tracing::debug!(
        duration_us = duration_us(drm_open_start.elapsed()),
        "POCKETBOOT_UI_DRM_OPEN_TIMING"
    );
    kms_display.lab_page_flip_markers_remaining = drm_page_flips;
    let setup_start = Instant::now();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);

    slint::platform::set_platform(Box::new(PocketPlatform::new(window.clone())))
        .map_err(|err| format!("install Slint platform: {err}"))?;
    window.set_size(PhysicalSize::new(kms_display.width, kms_display.height));

    let main_window = MainWindow::new().map_err(|err| format!("create Slint window: {err}"))?;
    main_window.set_device_name(system_info.device_name.into());
    main_window.set_device_detail(system_info.device_detail.into());
    main_window.set_serialno(system_info.serialno.into());
    main_window.on_power_action(move |action| {
        if ui_animation_lab {
            tracing::error!(
                action = ?action,
                "POCKETBOOT_UI_ANIMATION_LAB_UNEXPECTED_POWER_ACTION"
            );
            return;
        }
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
    tracing::debug!(
        duration_us = duration_us(setup_start.elapsed()),
        since_ui_start_us = duration_us(ui_start.elapsed()),
        "POCKETBOOT_UI_SETUP_TIMING"
    );

    let mut touch = TouchInput::new();
    let mut buttons = ButtonInput::new();
    let mut battery = battery.map(BatteryUpdates::new);
    let mut commands = UiCommands::new(commands);
    let mut pointer_down = false;
    let mut power_press_context = PowerPressContext::BootMenu;
    let mut page_flip_lab = DrmPageFlipLab::new(drm_page_flips);
    let mut animation_lab = UiAnimationLab::new(ui_animation_lab);
    let mut first_frame_logged = false;
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

        if page_flip_lab.request_next(kms_display.posted, kms_display.completed_page_flips) {
            if page_flip_lab.remaining + 1 == page_flip_lab.requested {
                tracing::info!(
                    requested = page_flip_lab.requested,
                    "POCKETBOOT_DRM_PAGE_FLIP_TEST_START"
                );
            }
            window.request_redraw();
        }

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

        for event in buttons.poll() {
            match event {
                ButtonEvent::Previous => main_window.invoke_hardware_nav_previous(),
                ButtonEvent::Next => main_window.invoke_hardware_nav_next(),
                ButtonEvent::PowerPressed => {
                    power_press_context = if main_window.get_power_menu_open() {
                        PowerPressContext::PowerMenu
                    } else {
                        PowerPressContext::BootMenu
                    };
                    main_window.invoke_hardware_power_pressed();
                }
                ButtonEvent::PowerReleased => main_window.invoke_hardware_power_released(),
                ButtonEvent::PowerShortPress => {
                    if matches!(power_press_context, PowerPressContext::BootMenu) {
                        main_window.invoke_hardware_power_short_press();
                    }
                }
                ButtonEvent::OpenMenu => {
                    if matches!(power_press_context, PowerPressContext::BootMenu)
                        && !main_window.get_power_menu_open()
                    {
                        main_window.invoke_show_power_menu();
                    }
                }
            }
        }

        animation_lab.poll(&main_window, &mut kms_display)?;

        let redrawn = kms_display.draw_if_needed(&window)?;
        if redrawn && window.has_active_animations() {
            window.request_redraw();
        }
        if redrawn && !first_frame_logged {
            first_frame_logged = true;
            tracing::debug!(
                frame = kms_display.submitted_frames,
                since_ui_start_us = duration_us(ui_start.elapsed()),
                "POCKETBOOT_UI_FIRST_FRAME_SUBMITTED"
            );
        }
        if page_flip_lab.should_drain(kms_display.page_flip_pending) {
            kms_display.wait_for_page_flip()?;
        }
        if let Some((requested, completed)) = page_flip_lab.finish(
            kms_display.page_flip_pending,
            kms_display.completed_page_flips,
        ) {
            tracing::info!(requested, completed, "POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT");
        }
        if !redrawn && !window.has_active_animations() {
            let sleep = slint::platform::duration_until_next_timer_update()
                .map(|duration| duration.min(IDLE_SLEEP))
                .unwrap_or(IDLE_SLEEP);
            if !sleep.is_zero() {
                thread::sleep(sleep);
            }
        }
    }
}

struct UiAnimationLab {
    phase: UiAnimationLabPhase,
    shutdown: Option<UiAnimationLabMeasurement>,
    reboot: Option<UiAnimationLabMeasurement>,
}

#[derive(Clone, Copy)]
struct UiAnimationLabMeasurement {
    report: FrameRateReport,
    progress_milli: u64,
}

impl UiAnimationLabMeasurement {
    fn passes(self) -> bool {
        (30_000..=60_000).contains(&self.report.fps_milli)
            && (30_000..=60_000).contains(&self.report.vblank_fps_milli)
            && self.report.interval_p95_us <= 33_500
            && self.report.interval_max_us <= 50_000
            && (650..=900).contains(&self.progress_milli)
    }
}

#[derive(Clone, Copy)]
enum UiAnimationLabPhase {
    Disabled,
    WaitForDisplay,
    OpenMenuAt(Instant),
    SelectShutdownAt(Instant),
    StartShutdownAt(Instant),
    StopShutdownAt(Instant),
    SelectRebootAt(Instant),
    StartRebootAt(Instant),
    StopRebootAt(Instant),
    FinishAt(Instant),
    Done,
}

impl UiAnimationLab {
    fn new(enabled: bool) -> Self {
        let phase = if enabled {
            tracing::info!("POCKETBOOT_UI_ANIMATION_LAB_ENABLED");
            UiAnimationLabPhase::WaitForDisplay
        } else {
            UiAnimationLabPhase::Disabled
        };
        Self {
            phase,
            shutdown: None,
            reboot: None,
        }
    }

    fn poll(&mut self, window: &MainWindow, display: &mut KmsDisplay) -> Result<(), String> {
        let now = Instant::now();
        match self.phase {
            UiAnimationLabPhase::Disabled | UiAnimationLabPhase::Done => {}
            UiAnimationLabPhase::WaitForDisplay if display.posted => {
                self.phase = UiAnimationLabPhase::OpenMenuAt(now + ANIMATION_LAB_SETTLE);
            }
            UiAnimationLabPhase::WaitForDisplay => {}
            UiAnimationLabPhase::OpenMenuAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                window.invoke_show_power_menu();
                tracing::info!(phase = "open_menu", "POCKETBOOT_UI_ANIMATION_LAB_PHASE");
                self.phase =
                    UiAnimationLabPhase::SelectShutdownAt(Instant::now() + ANIMATION_LAB_SETTLE);
            }
            UiAnimationLabPhase::OpenMenuAt(_) => {}
            UiAnimationLabPhase::SelectShutdownAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                window.invoke_hardware_nav_next();
                self.phase =
                    UiAnimationLabPhase::StartShutdownAt(Instant::now() + ANIMATION_LAB_SETTLE);
            }
            UiAnimationLabPhase::SelectShutdownAt(_) => {}
            UiAnimationLabPhase::StartShutdownAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                display.start_frame_rate_measurement("shutdown");
                window.invoke_hardware_power_pressed();
                tracing::info!(
                    action = "shutdown",
                    "POCKETBOOT_UI_ANIMATION_LAB_HOLD_START"
                );
                self.phase =
                    UiAnimationLabPhase::StopShutdownAt(Instant::now() + ANIMATION_LAB_HOLD);
            }
            UiAnimationLabPhase::StartShutdownAt(_) => {}
            UiAnimationLabPhase::StopShutdownAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                let progress_milli = progress_milli(window.get_hold_progress());
                self.shutdown = display.finish_frame_rate_measurement().map(|report| {
                    UiAnimationLabMeasurement {
                        report,
                        progress_milli,
                    }
                });
                window.invoke_hardware_power_released();
                tracing::info!(
                    action = "shutdown",
                    measured = self.shutdown.is_some(),
                    progress_milli,
                    "POCKETBOOT_UI_ANIMATION_LAB_HOLD_STOP"
                );
                self.phase = UiAnimationLabPhase::SelectRebootAt(
                    Instant::now() + ANIMATION_LAB_BETWEEN_BUTTONS,
                );
            }
            UiAnimationLabPhase::StopShutdownAt(_) => {}
            UiAnimationLabPhase::SelectRebootAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                window.invoke_hardware_nav_next();
                self.phase =
                    UiAnimationLabPhase::StartRebootAt(Instant::now() + ANIMATION_LAB_SETTLE);
            }
            UiAnimationLabPhase::SelectRebootAt(_) => {}
            UiAnimationLabPhase::StartRebootAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                display.start_frame_rate_measurement("reboot");
                window.invoke_hardware_power_pressed();
                tracing::info!(action = "reboot", "POCKETBOOT_UI_ANIMATION_LAB_HOLD_START");
                self.phase = UiAnimationLabPhase::StopRebootAt(Instant::now() + ANIMATION_LAB_HOLD);
            }
            UiAnimationLabPhase::StartRebootAt(_) => {}
            UiAnimationLabPhase::StopRebootAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                let progress_milli = progress_milli(window.get_hold_progress());
                self.reboot = display.finish_frame_rate_measurement().map(|report| {
                    UiAnimationLabMeasurement {
                        report,
                        progress_milli,
                    }
                });
                window.invoke_hardware_power_released();
                tracing::info!(
                    action = "reboot",
                    measured = self.reboot.is_some(),
                    progress_milli,
                    "POCKETBOOT_UI_ANIMATION_LAB_HOLD_STOP"
                );
                self.phase = UiAnimationLabPhase::FinishAt(Instant::now() + ANIMATION_LAB_SETTLE);
            }
            UiAnimationLabPhase::StopRebootAt(_) => {}
            UiAnimationLabPhase::FinishAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                let shutdown = self.shutdown;
                let reboot = self.reboot;
                let passed = shutdown.is_some_and(UiAnimationLabMeasurement::passes)
                    && reboot.is_some_and(UiAnimationLabMeasurement::passes);
                tracing::info!(
                    passed,
                    shutdown_measured = shutdown.is_some(),
                    shutdown_fps_milli = shutdown.map_or(0, |m| m.report.fps_milli),
                    shutdown_vblank_fps_milli = shutdown.map_or(0, |m| m.report.vblank_fps_milli),
                    shutdown_flips = shutdown.map_or(0, |m| m.report.completed_flips),
                    shutdown_p95_us = shutdown.map_or(0, |m| m.report.interval_p95_us),
                    shutdown_max_us = shutdown.map_or(0, |m| m.report.interval_max_us),
                    shutdown_progress_milli = shutdown.map_or(0, |m| m.progress_milli),
                    reboot_measured = reboot.is_some(),
                    reboot_fps_milli = reboot.map_or(0, |m| m.report.fps_milli),
                    reboot_vblank_fps_milli = reboot.map_or(0, |m| m.report.vblank_fps_milli),
                    reboot_flips = reboot.map_or(0, |m| m.report.completed_flips),
                    reboot_p95_us = reboot.map_or(0, |m| m.report.interval_p95_us),
                    reboot_max_us = reboot.map_or(0, |m| m.report.interval_max_us),
                    reboot_progress_milli = reboot.map_or(0, |m| m.progress_milli),
                    "POCKETBOOT_UI_ANIMATION_LAB_RESULT"
                );
                self.phase = UiAnimationLabPhase::Done;
            }
            UiAnimationLabPhase::FinishAt(_) => {}
        }
        Ok(())
    }
}

fn progress_milli(progress: f32) -> u64 {
    u64::try_from((progress.clamp(0.0, 1.0) * 1000.0).round() as i64).unwrap_or(0)
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
    window.invoke_refresh_boot_selection();
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
    heap_buffer: CachedRenderBuffer,
    posted: bool,
    page_flip_pending: bool,
    page_flip_submitted_at: Option<Instant>,
    submitted_frames: u64,
    completed_page_flips: u64,
    frame_rate_stats: FrameRateStats,
    frame_rate_measurement_action: Option<&'static str>,
    frame_rate_measurement_report: Option<FrameRateReport>,
    lab_page_flip_markers_remaining: u32,
}

#[derive(Debug)]
struct FrameRateStats {
    mode_vrefresh: u32,
    start_time: Option<Duration>,
    start_vblank: u32,
    last_time: Option<Duration>,
    intervals_us: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrameRateReport {
    completed_flips: u32,
    elapsed_us: u64,
    vblank_delta: u32,
    fps_milli: u64,
    vblank_fps_milli: u64,
    interval_p50_us: u64,
    interval_p95_us: u64,
    interval_max_us: u64,
}

impl FrameRateStats {
    fn new(mode_vrefresh: u32) -> Self {
        Self {
            mode_vrefresh,
            start_time: None,
            start_vblank: 0,
            last_time: None,
            intervals_us: Vec::with_capacity(mode_vrefresh as usize),
        }
    }

    fn record(&mut self, vblank: u32, event_time: Duration) -> Option<FrameRateReport> {
        let Some(last_time) = self.last_time else {
            self.reset(vblank, event_time);
            return None;
        };
        let interval = event_time.saturating_sub(last_time);
        if interval > FRAME_RATE_GAP_RESET {
            self.reset(vblank, event_time);
            return None;
        }

        self.last_time = Some(event_time);
        self.intervals_us.push(duration_us(interval));

        let start_time = self.start_time.unwrap_or(event_time);
        let elapsed = event_time.saturating_sub(start_time);
        if elapsed < FRAME_RATE_WINDOW {
            return None;
        }

        self.intervals_us.sort_unstable();
        let completed_flips = self.intervals_us.len() as u32;
        let elapsed_us = duration_us(elapsed);
        let vblank_delta = vblank.wrapping_sub(self.start_vblank);
        let report = FrameRateReport {
            completed_flips,
            elapsed_us,
            vblank_delta,
            fps_milli: rate_milli(completed_flips, elapsed),
            vblank_fps_milli: if vblank_delta == 0 {
                0
            } else {
                u64::from(completed_flips)
                    .saturating_mul(u64::from(self.mode_vrefresh))
                    .saturating_mul(1000)
                    / u64::from(vblank_delta)
            },
            interval_p50_us: percentile(&self.intervals_us, 50),
            interval_p95_us: percentile(&self.intervals_us, 95),
            interval_max_us: self.intervals_us.last().copied().unwrap_or(0),
        };
        self.reset(vblank, event_time);
        Some(report)
    }

    fn clear(&mut self) {
        self.start_time = None;
        self.start_vblank = 0;
        self.last_time = None;
        self.intervals_us.clear();
    }

    fn reset(&mut self, vblank: u32, event_time: Duration) {
        self.start_time = Some(event_time);
        self.start_vblank = vblank;
        self.last_time = Some(event_time);
        self.intervals_us.clear();
    }
}

fn rate_milli(completed_flips: u32, elapsed: Duration) -> u64 {
    let elapsed_ns = elapsed.as_nanos();
    if elapsed_ns == 0 {
        return 0;
    }
    let rate = u128::from(completed_flips)
        .saturating_mul(1_000_000_000)
        .saturating_mul(1000)
        / elapsed_ns;
    rate.try_into().unwrap_or(u64::MAX)
}

fn percentile(sorted_values: &[u64], percentile: usize) -> u64 {
    if sorted_values.is_empty() {
        return 0;
    }
    let rank = sorted_values
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1);
    sorted_values[rank.min(sorted_values.len() - 1)]
}

#[derive(Debug)]
struct DrmPageFlipLab {
    requested: u32,
    remaining: u32,
    baseline: Option<u64>,
    reported: bool,
}

impl DrmPageFlipLab {
    fn new(requested: u32) -> Self {
        Self {
            requested,
            remaining: requested,
            baseline: None,
            reported: false,
        }
    }

    fn request_next(&mut self, display_posted: bool, completed: u64) -> bool {
        if !display_posted || self.remaining == 0 {
            return false;
        }
        self.baseline.get_or_insert(completed);
        self.remaining -= 1;
        true
    }

    fn should_drain(&self, page_flip_pending: bool) -> bool {
        self.baseline.is_some() && self.remaining == 0 && page_flip_pending
    }

    fn finish(&mut self, page_flip_pending: bool, completed: u64) -> Option<(u32, u64)> {
        if self.reported || self.remaining != 0 || page_flip_pending {
            return None;
        }
        let baseline = self.baseline?;
        self.reported = true;
        Some((self.requested, completed.saturating_sub(baseline)))
    }
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
        let frame_rate_stats = FrameRateStats::new(mode.vrefresh());

        let front_buffer = KmsBuffer::allocate(&card, size, format)?;
        let back_buffer = KmsBuffer::allocate(&card, size, format)?;
        let in_flight_buffer = KmsBuffer::allocate(&card, size, format)?;
        let render_pitch = back_buffer.buffer.pitch();
        for (name, pitch) in [
            ("front", front_buffer.buffer.pitch()),
            ("in-flight", in_flight_buffer.buffer.pitch()),
        ] {
            if pitch != render_pitch {
                return Err(format!(
                    "DRM {name} buffer pitch differs from back buffer: {pitch} != {render_pitch}"
                ));
            }
        }
        let heap_buffer = CachedRenderBuffer::new(format, render_pitch, size.1)?;

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
            heap_buffer,
            posted: false,
            page_flip_pending: false,
            page_flip_submitted_at: None,
            submitted_frames: 0,
            completed_page_flips: 0,
            frame_rate_stats,
            frame_rate_measurement_action: None,
            frame_rate_measurement_report: None,
            lab_page_flip_markers_remaining: 0,
        })
    }

    fn start_frame_rate_measurement(&mut self, action: &'static str) {
        self.frame_rate_stats.clear();
        self.frame_rate_measurement_action = Some(action);
        self.frame_rate_measurement_report = None;
    }

    fn finish_frame_rate_measurement(&mut self) -> Option<FrameRateReport> {
        self.frame_rate_measurement_action = None;
        self.frame_rate_measurement_report.take()
    }

    fn draw_if_needed(&mut self, window: &MinimalSoftwareWindow) -> Result<bool, String> {
        let frame = self.submitted_frames.saturating_add(1);
        let draw_start = Instant::now();
        let mut render_result = Ok(());
        let redraw = window.draw_if_needed(|renderer| {
            render_result = self.render(renderer, frame);
        });

        render_result?;

        if redraw {
            self.present(frame)?;
            tracing::trace!(
                frame,
                duration_us = duration_us(draw_start.elapsed()),
                "POCKETBOOT_UI_FRAME_TIMING"
            );
        }

        Ok(redraw)
    }

    fn render(&mut self, renderer: &SoftwareRenderer, frame: u64) -> Result<(), String> {
        let render_total_start = Instant::now();
        let format = self.format;
        let stride = self.back_buffer.buffer.pitch() as usize / bytes_per_pixel(format);
        let buffer_age = self.heap_buffer.age;
        renderer.set_repaint_buffer_type(self.heap_buffer.repaint_buffer_type());

        let renderer_duration = self.heap_buffer.render(renderer, format, stride)?;

        let map_start = Instant::now();
        let mut mapping = self
            .card
            .map_dumb_buffer(&mut self.back_buffer.buffer)
            .map_err(|err| format!("map DRM dumb buffer: {err}"))?;
        let map_duration = map_start.elapsed();
        let copy_duration = self.heap_buffer.copy_to_drm(mapping.as_mut(), format)?;

        tracing::trace!(
            frame,
            path = "cached_heap",
            format = ?format,
            stride,
            buffer_age,
            map_us = duration_us(map_duration),
            renderer_us = duration_us(renderer_duration),
            copy_us = duration_us(copy_duration),
            total_us = duration_us(render_total_start.elapsed()),
            "POCKETBOOT_UI_RENDER_TIMING"
        );

        Ok(())
    }

    fn present(&mut self, frame: u64) -> Result<(), String> {
        let present_start = Instant::now();
        let wait_start = Instant::now();
        self.wait_for_page_flip()?;
        let previous_flip_wait = wait_start.elapsed();

        mem::swap(&mut self.back_buffer, &mut self.front_buffer);
        mem::swap(&mut self.front_buffer, &mut self.in_flight_buffer);

        let (operation, submit_duration) = if self.posted {
            let submit_start = Instant::now();
            self.card
                .page_flip(
                    self.crtc,
                    self.in_flight_buffer.framebuffer,
                    PageFlipFlags::EVENT,
                    None,
                )
                .map_err(|err| format!("page flip DRM buffer: {err}"))?;
            let submit_duration = submit_start.elapsed();
            self.page_flip_pending = true;
            self.page_flip_submitted_at = Some(submit_start);
            ("page_flip", submit_duration)
        } else {
            let submit_start = Instant::now();
            self.card
                .set_crtc(
                    self.crtc,
                    Some(self.in_flight_buffer.framebuffer),
                    (0, 0),
                    &[self.connector.handle()],
                    Some(self.mode),
                )
                .map_err(|err| format!("set DRM CRTC: {err}"))?;
            let submit_duration = submit_start.elapsed();
            self.posted = true;
            tracing::info!(
                path = %self.path.display(),
                connector = %self.connector,
                width = self.width,
                height = self.height,
                "POCKETBOOT_DRM_READY"
            );
            ("set_crtc", submit_duration)
        };
        self.submitted_frames = frame;
        tracing::trace!(
            frame,
            operation,
            previous_flip_wait_us = duration_us(previous_flip_wait),
            submit_us = duration_us(submit_duration),
            total_us = duration_us(present_start.elapsed()),
            "POCKETBOOT_UI_PRESENT_TIMING"
        );
        if frame == 1 {
            tracing::debug!(
                frame,
                operation,
                submit_us = duration_us(submit_duration),
                total_us = duration_us(present_start.elapsed()),
                "POCKETBOOT_UI_FIRST_MODESET_TIMING"
            );
        }

        Ok(())
    }

    fn wait_for_page_flip(&mut self) -> Result<(), String> {
        if !self.page_flip_pending {
            return Ok(());
        }

        let frame = self.submitted_frames;
        let wait_start = Instant::now();
        let submitted_at = self
            .page_flip_submitted_at
            .ok_or_else(|| "page flip pending without a submission timestamp".to_string())?;
        let mut batches = 0u32;
        loop {
            let receive_start = Instant::now();
            let events = self
                .card
                .receive_events()
                .map_err(|err| format!("receive DRM events: {err}"))?;
            let receive_duration = receive_start.elapsed();
            batches = batches.saturating_add(1);
            let mut event_count = 0u32;
            let mut matched_page_flip = None;
            for event in events {
                event_count = event_count.saturating_add(1);
                if let Event::PageFlip(page_flip) = event
                    && page_flip.crtc == self.crtc
                {
                    matched_page_flip = Some(page_flip);
                }
            }
            let matched = matched_page_flip.is_some();
            let receive_wait_duration = wait_start.elapsed();
            tracing::trace!(
                frame,
                batch = batches,
                event_count,
                matched,
                receive_us = duration_us(receive_duration),
                receive_wait_us = duration_us(receive_wait_duration),
                "POCKETBOOT_UI_DRM_EVENT_TIMING"
            );
            if let Some(page_flip) = matched_page_flip {
                let submit_to_observe_duration = submitted_at.elapsed();
                self.page_flip_pending = false;
                self.page_flip_submitted_at = None;
                self.completed_page_flips = self.completed_page_flips.saturating_add(1);
                if let Some(report) = self
                    .frame_rate_stats
                    .record(page_flip.frame, page_flip.duration)
                {
                    if self.frame_rate_measurement_action.is_some() {
                        self.frame_rate_measurement_report = Some(report);
                    }
                    tracing::debug!(
                        action = self.frame_rate_measurement_action.unwrap_or("unscoped"),
                        completed_flips = report.completed_flips,
                        elapsed_us = report.elapsed_us,
                        vblank_delta = report.vblank_delta,
                        fps_milli = report.fps_milli,
                        vblank_fps_milli = report.vblank_fps_milli,
                        interval_p50_us = report.interval_p50_us,
                        interval_p95_us = report.interval_p95_us,
                        interval_max_us = report.interval_max_us,
                        "POCKETBOOT_UI_FRAME_RATE"
                    );
                }
                tracing::trace!(
                    frame,
                    sequence = self.completed_page_flips,
                    batches,
                    submit_to_observe_us = duration_us(submit_to_observe_duration),
                    receive_wait_us = duration_us(receive_wait_duration),
                    "POCKETBOOT_UI_PAGE_FLIP_COMPLETE_TIMING"
                );
                if self.completed_page_flips == 1 {
                    tracing::debug!(
                        frame,
                        sequence = self.completed_page_flips,
                        batches,
                        submit_to_observe_us = duration_us(submit_to_observe_duration),
                        receive_wait_us = duration_us(receive_wait_duration),
                        "POCKETBOOT_UI_FIRST_PAGE_FLIP_TIMING"
                    );
                }
                if self.lab_page_flip_markers_remaining > 0 {
                    self.lab_page_flip_markers_remaining -= 1;
                    tracing::info!(
                        sequence = self.completed_page_flips,
                        remaining = self.lab_page_flip_markers_remaining,
                        "POCKETBOOT_DRM_PAGE_FLIP"
                    );
                }
                return Ok(());
            }
        }
    }
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod drm_lab_tests {
    use super::*;

    #[test]
    fn bounded_page_flip_lab_requests_exact_count_and_reports_once() {
        let mut lab = DrmPageFlipLab::new(3);

        assert!(!lab.request_next(false, 10));
        assert!(lab.request_next(true, 10));
        assert!(lab.request_next(true, 11));
        assert!(lab.request_next(true, 12));
        assert!(!lab.request_next(true, 13));
        assert!(lab.should_drain(true));
        assert_eq!(lab.finish(true, 13), None);
        assert_eq!(lab.finish(false, 13), Some((3, 3)));
        assert_eq!(lab.finish(false, 13), None);
    }

    #[test]
    fn disabled_page_flip_lab_never_forces_a_redraw() {
        let mut lab = DrmPageFlipLab::new(0);

        assert!(!lab.request_next(true, 0));
        assert!(!lab.should_drain(true));
        assert_eq!(lab.finish(false, 0), None);
    }

    #[test]
    fn cached_render_buffer_uses_the_drm_pitch() {
        let mut buffer = CachedRenderBuffer::new(DrmFourcc::Xrgb8888, 64, 3).unwrap();

        assert_eq!(buffer.byte_len(), 192);
        assert_eq!(buffer.repaint_buffer_type(), RepaintBufferType::NewBuffer);
        buffer.age = 1;
        assert_eq!(
            buffer.repaint_buffer_type(),
            RepaintBufferType::ReusedBuffer
        );
    }

    #[test]
    fn cached_render_buffer_rejects_a_pitch_between_pixels() {
        assert!(CachedRenderBuffer::new(DrmFourcc::Rgb565, 3, 1).is_err());
    }

    #[test]
    fn cached_render_buffer_rejects_an_undersized_drm_mapping() {
        let buffer = CachedRenderBuffer::new(DrmFourcc::Xrgb8888, 64, 3).unwrap();
        let mut mapping = vec![0; buffer.byte_len() - 4];

        assert!(
            buffer
                .copy_to_drm(&mut mapping, DrmFourcc::Xrgb8888)
                .is_err()
        );
    }

    #[test]
    fn frame_rate_stats_reports_sixty_fps_from_completed_vblanks() {
        let mut stats = FrameRateStats::new(60);
        let mut report = None;
        for frame in 0..=60u32 {
            report = stats.record(
                100 + frame,
                Duration::from_nanos(16_666_667 * u64::from(frame)),
            );
        }

        let report = report.unwrap();
        assert_eq!(report.completed_flips, 60);
        assert_eq!(report.vblank_delta, 60);
        assert!((59_990..=60_000).contains(&report.fps_milli));
        assert_eq!(report.vblank_fps_milli, 60_000);
        assert_eq!(report.interval_p95_us, 16_666);
        assert_eq!(report.interval_max_us, 16_666);
    }

    #[test]
    fn frame_rate_stats_reports_thirty_fps_from_every_other_vblank() {
        let mut stats = FrameRateStats::new(60);
        let mut report = None;
        for frame in 0..=30u32 {
            report = stats.record(
                100 + frame * 2,
                Duration::from_nanos(33_333_334 * u64::from(frame)),
            );
        }

        let report = report.unwrap();
        assert_eq!(report.completed_flips, 30);
        assert_eq!(report.vblank_delta, 60);
        assert!((29_990..=30_000).contains(&report.fps_milli));
        assert_eq!(report.vblank_fps_milli, 30_000);
        assert_eq!(report.interval_p95_us, 33_333);
        assert_eq!(report.interval_max_us, 33_333);
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
        })
    }
}

struct CachedRenderBuffer {
    pixels: CachedRenderPixels,
    age: u8,
}

impl CachedRenderBuffer {
    fn new(format: DrmFourcc, pitch: u32, height: u32) -> Result<Self, String> {
        let bytes_per_pixel = bytes_per_pixel(format);
        let pitch = pitch as usize;
        if pitch % bytes_per_pixel != 0 {
            return Err(format!(
                "DRM pitch is not aligned for {format:?}: {pitch} bytes"
            ));
        }

        let pixel_count = (pitch / bytes_per_pixel)
            .checked_mul(height as usize)
            .ok_or_else(|| format!("cached render buffer is too large for {format:?}"))?;
        let pixels = match format {
            DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888 => {
                CachedRenderPixels::Xrgb8888(vec![Xrgb8888Pixel::default(); pixel_count])
            }
            DrmFourcc::Bgra8888 => {
                CachedRenderPixels::Bgra8888(vec![Bgra8888Pixel::default(); pixel_count])
            }
            DrmFourcc::Rgb565 => {
                CachedRenderPixels::Rgb565(vec![Rgb565Pixel::default(); pixel_count])
            }
            _ => return Err(format!("unsupported DRM format {format:?}")),
        };

        Ok(Self { pixels, age: 0 })
    }

    fn repaint_buffer_type(&self) -> RepaintBufferType {
        if self.age == 0 {
            RepaintBufferType::NewBuffer
        } else {
            RepaintBufferType::ReusedBuffer
        }
    }

    fn render(
        &mut self,
        renderer: &SoftwareRenderer,
        format: DrmFourcc,
        stride: usize,
    ) -> Result<Duration, String> {
        self.age = 1;
        let render_start = Instant::now();
        match (format, &mut self.pixels) {
            (DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888, CachedRenderPixels::Xrgb8888(pixels)) => {
                renderer.render(pixels, stride);
            }
            (DrmFourcc::Bgra8888, CachedRenderPixels::Bgra8888(pixels)) => {
                renderer.render(pixels, stride);
            }
            (DrmFourcc::Rgb565, CachedRenderPixels::Rgb565(pixels)) => {
                renderer.render(pixels, stride);
            }
            _ => {
                return Err(format!(
                    "cached render buffer format does not match DRM format {format:?}"
                ));
            }
        }

        Ok(render_start.elapsed())
    }

    fn copy_to_drm(&self, drm_bytes: &mut [u8], format: DrmFourcc) -> Result<Duration, String> {
        let copy_start = Instant::now();
        match (format, &self.pixels) {
            (DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888, CachedRenderPixels::Xrgb8888(pixels)) => {
                copy_pixels_to_drm(drm_bytes, pixels, format)?;
            }
            (DrmFourcc::Bgra8888, CachedRenderPixels::Bgra8888(pixels)) => {
                copy_pixels_to_drm(drm_bytes, pixels, format)?;
            }
            (DrmFourcc::Rgb565, CachedRenderPixels::Rgb565(pixels)) => {
                copy_pixels_to_drm(drm_bytes, pixels, format)?;
            }
            _ => {
                return Err(format!(
                    "cached render buffer format does not match DRM format {format:?}"
                ));
            }
        }

        Ok(copy_start.elapsed())
    }

    #[cfg(test)]
    fn byte_len(&self) -> usize {
        match &self.pixels {
            CachedRenderPixels::Xrgb8888(pixels) => mem::size_of_val(pixels.as_slice()),
            CachedRenderPixels::Bgra8888(pixels) => mem::size_of_val(pixels.as_slice()),
            CachedRenderPixels::Rgb565(pixels) => mem::size_of_val(pixels.as_slice()),
        }
    }
}

enum CachedRenderPixels {
    Xrgb8888(Vec<Xrgb8888Pixel>),
    Bgra8888(Vec<Bgra8888Pixel>),
    Rgb565(Vec<Rgb565Pixel>),
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

/// A pixel whose in-memory representation can be copied directly to a DRM mapping.
///
/// Implementors must have no uninitialized padding and every bit pattern must be valid.
unsafe trait DrmPixel: Copy {}

fn copy_pixels_to_drm<T: DrmPixel>(
    drm_bytes: &mut [u8],
    pixels: &[T],
    format: DrmFourcc,
) -> Result<(), String> {
    let drm_pixels = cast_buffer_mut::<T>(drm_bytes, format)?;
    if drm_pixels.len() < pixels.len() {
        return Err(format!(
            "DRM buffer mapping is too small for cached render: {} < {} pixels",
            drm_pixels.len(),
            pixels.len()
        ));
    }
    drm_pixels[..pixels.len()].copy_from_slice(pixels);
    Ok(())
}

fn cast_buffer_mut<T: DrmPixel>(buffer: &mut [u8], format: DrmFourcc) -> Result<&mut [T], String> {
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

// SAFETY: These are transparent integer-backed pixel types without padding.
unsafe impl DrmPixel for Xrgb8888Pixel {}
unsafe impl DrmPixel for Bgra8888Pixel {}
unsafe impl DrmPixel for Rgb565Pixel {}

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

struct ButtonInput {
    devices: Vec<ButtonDevice>,
    ignored_devices: Vec<PathBuf>,
    next_scan: Instant,
}

impl ButtonInput {
    fn new() -> Self {
        let mut input = Self {
            devices: Vec::new(),
            ignored_devices: Vec::new(),
            next_scan: Instant::now(),
        };
        input.rescan();
        input
    }

    fn poll(&mut self) -> Vec<ButtonEvent> {
        if Instant::now() >= self.next_scan {
            self.rescan();
        }

        let mut events = Vec::new();
        let mut index = 0;
        while index < self.devices.len() {
            match self.devices[index].poll(&mut events) {
                Ok(()) => index += 1,
                Err(err) => {
                    tracing::warn!(path = %self.devices[index].path.display(), error = %err, "dropping hardware button input device");
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
            match ButtonDevice::open(&path) {
                Ok(device) => {
                    tracing::info!(path = %path.display(), "opened hardware button input device");
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

struct ButtonDevice {
    file: File,
    path: PathBuf,
    state: ButtonState,
}

impl ButtonDevice {
    fn open(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|err| format!("open: {err}"))?;
        let fd = file.as_raw_fd();

        let keys = query_button_keys(fd).map_err(|err| format!("query navigation keys: {err}"))?;
        if !keys.any() {
            return Err("device does not advertise hardware navigation keys".to_string());
        }

        Ok(Self {
            file,
            path: path.to_path_buf(),
            state: ButtonState::default(),
        })
    }

    fn poll(&mut self, events: &mut Vec<ButtonEvent>) -> io::Result<()> {
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
                        self.state.handle(event, events);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }

        self.state.poll(events);
        Ok(())
    }
}

#[derive(Default)]
struct ButtonState {
    power_pressed_since: Option<Instant>,
    power_long_reported: bool,
}

impl ButtonState {
    fn handle(&mut self, event: InputEvent, events: &mut Vec<ButtonEvent>) {
        if event.type_ != EV_KEY {
            return;
        }

        match event.code {
            KEY_VOLUMEUP if matches!(event.value, 1 | 2) => events.push(ButtonEvent::Previous),
            KEY_VOLUMEDOWN if matches!(event.value, 1 | 2) => events.push(ButtonEvent::Next),
            KEY_POWER => self.handle_power(event.value, events),
            _ => {}
        }
    }

    fn handle_power(&mut self, value: i32, events: &mut Vec<ButtonEvent>) {
        match value {
            0 => {
                let was_pressed = self.power_pressed_since.take().is_some();
                let was_long = self.power_long_reported;
                self.power_long_reported = false;
                if was_pressed {
                    events.push(ButtonEvent::PowerReleased);
                    if !was_long {
                        events.push(ButtonEvent::PowerShortPress);
                    }
                }
            }
            1 => {
                self.power_pressed_since = Some(Instant::now());
                self.power_long_reported = false;
                events.push(ButtonEvent::PowerPressed);
            }
            _ => {}
        }
    }

    fn poll(&mut self, events: &mut Vec<ButtonEvent>) {
        let Some(pressed_since) = self.power_pressed_since else {
            return;
        };
        if self.power_long_reported || pressed_since.elapsed() < POWER_KEY_HOLD {
            return;
        }

        self.power_long_reported = true;
        events.push(ButtonEvent::OpenMenu);
    }
}

#[derive(Clone, Copy, Default)]
struct ButtonKeys {
    volume_up: bool,
    volume_down: bool,
    power: bool,
}

impl ButtonKeys {
    fn any(self) -> bool {
        self.volume_up || self.volume_down || self.power
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PowerPressContext {
    BootMenu,
    PowerMenu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ButtonEvent {
    Previous,
    Next,
    PowerPressed,
    PowerReleased,
    PowerShortPress,
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

fn query_button_keys(fd: i32) -> io::Result<ButtonKeys> {
    let mut bits = [0u8; KEY_BITS_BYTES];
    ioctl_read(fd, eviocgbit(EV_KEY, bits.len()), &mut bits)?;
    Ok(ButtonKeys {
        volume_up: test_bit(&bits, KEY_VOLUMEUP),
        volume_down: test_bit(&bits, KEY_VOLUMEDOWN),
        power: test_bit(&bits, KEY_POWER),
    })
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
const KEY_VOLUMEDOWN: u16 = 0x72;
const KEY_VOLUMEUP: u16 = 0x73;
const KEY_POWER: u16 = 0x74;
const KEY_BITS_BYTES: usize = KEY_POWER as usize / 8 + 1;
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
mod button_tests {
    use super::*;

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

    #[test]
    fn volume_buttons_emit_navigation_on_press_and_repeat() {
        let mut state = ButtonState::default();
        let mut events = Vec::new();

        state.handle(event(EV_KEY, KEY_VOLUMEUP, 1), &mut events);
        state.handle(event(EV_KEY, KEY_VOLUMEUP, 2), &mut events);
        state.handle(event(EV_KEY, KEY_VOLUMEDOWN, 1), &mut events);
        state.handle(event(EV_KEY, KEY_VOLUMEDOWN, 2), &mut events);

        assert_eq!(
            events,
            vec![
                ButtonEvent::Previous,
                ButtonEvent::Previous,
                ButtonEvent::Next,
                ButtonEvent::Next,
            ]
        );
    }

    #[test]
    fn short_power_press_reports_press_release_and_short_press() {
        let mut state = ButtonState::default();
        let mut events = Vec::new();

        state.handle(event(EV_KEY, KEY_POWER, 1), &mut events);
        state.handle(event(EV_KEY, KEY_POWER, 0), &mut events);

        assert_eq!(
            events,
            vec![
                ButtonEvent::PowerPressed,
                ButtonEvent::PowerReleased,
                ButtonEvent::PowerShortPress,
            ]
        );
    }

    #[test]
    fn long_power_hold_opens_menu_once() {
        let mut state = ButtonState::default();
        let mut events = Vec::new();

        state.handle(event(EV_KEY, KEY_POWER, 1), &mut events);
        events.clear();
        state.power_pressed_since =
            Some(Instant::now() - POWER_KEY_HOLD - Duration::from_millis(1));

        state.poll(&mut events);
        state.poll(&mut events);

        assert_eq!(events, vec![ButtonEvent::OpenMenu]);
    }

    #[test]
    fn long_power_hold_suppresses_short_press_on_release() {
        let mut state = ButtonState::default();
        let mut events = Vec::new();

        state.handle(event(EV_KEY, KEY_POWER, 1), &mut events);
        events.clear();
        state.power_pressed_since =
            Some(Instant::now() - POWER_KEY_HOLD - Duration::from_millis(1));

        state.poll(&mut events);
        state.handle(event(EV_KEY, KEY_POWER, 0), &mut events);

        assert_eq!(
            events,
            vec![ButtonEvent::OpenMenu, ButtonEvent::PowerReleased]
        );
    }
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
