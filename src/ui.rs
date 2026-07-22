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
        MinimalSoftwareWindow, PhysicalRegion, PremultipliedRgbaColor, RepaintBufferType,
        Rgb565Pixel, SoftwareRenderer, TargetPixel,
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
const ANIMATION_LAB_WARMUP_FRAMES: u8 = 3;
const ANIMATION_LAB_DAMAGE_FRAME_DELAY: Duration = Duration::ZERO;
const ANIMATION_LAB_DAMAGE_PATTERN_STATES: u8 = 5;
const ANIMATION_LAB_DAMAGE_FRAMES_PER_STATE: u8 = 3;
const ANIMATION_LAB_DAMAGE_PATTERN_FRAMES: u8 =
    ANIMATION_LAB_DAMAGE_PATTERN_STATES * ANIMATION_LAB_DAMAGE_FRAMES_PER_STATE;
const ANIMATION_LAB_DAMAGE_VALIDATION_FRAMES: u8 =
    ANIMATION_LAB_DAMAGE_PATTERN_FRAMES + ANIMATION_LAB_DAMAGE_FRAMES_PER_STATE;
const MAX_PENDING_DAMAGE_RECTS: usize = 64;

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
    kms_display.lab_page_flip_markers_remaining = if ui_animation_lab { 0 } else { drm_page_flips };
    let setup_start = Instant::now();
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);

    slint::platform::set_platform(Box::new(PocketPlatform::new(window.clone())))
        .map_err(|err| format!("install Slint platform: {err}"))?;

    let main_window = MainWindow::new().map_err(|err| format!("create Slint window: {err}"))?;
    window.set_size(PhysicalSize::new(kms_display.width, kms_display.height));
    let scale_factor = window.scale_factor();
    let logical_width = kms_display.width as f32 / scale_factor;
    let logical_height = kms_display.height as f32 / scale_factor;
    tracing::info!(
        scale_factor_milli = scale_factor_milli(scale_factor),
        physical_width = kms_display.width,
        physical_height = kms_display.height,
        logical_width = logical_width.round() as u32,
        logical_height = logical_height.round() as u32,
        "POCKETBOOT_UI_SCALE"
    );
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
    let mut page_flip_lab = DrmPageFlipLab::new(if ui_animation_lab { 0 } else { drm_page_flips });
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

        if !ui_animation_lab {
            if let Some(battery) = &mut battery {
                battery.poll(&main_window);
            }
            commands.poll(&main_window);

            for report in touch.poll(kms_display.width, kms_display.height) {
                let position = logical_touch_position(report, window.scale_factor());
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
    damage_validation: Option<UiDamageValidationMeasurement>,
    damage_validation_baseline_flips: u64,
}

#[derive(Clone, Copy)]
struct UiAnimationLabMeasurement {
    report: FrameRateReport,
    progress_milli: u64,
    copy: CopyStats,
}

impl UiAnimationLabMeasurement {
    fn passes(self, visible_frame_bytes: u64) -> bool {
        (55_000..=61_000).contains(&self.report.fps_milli)
            && (55_000..=60_000).contains(&self.report.vblank_fps_milli)
            && self.report.completed_flips >= 55
            && self.report.interval_p50_us <= 18_000
            && self.report.interval_p95_us <= 33_500
            && self.report.interval_max_us <= 50_000
            && (650..=900).contains(&self.progress_milli)
            && self.copy.damage_frames >= 55
            && self.copy.full_frames == 0
            && self.copy.copied_bytes > 0
            && self.copy.max_damage_bytes > 0
            && self.copy.max_damage_bytes < visible_frame_bytes
    }
}

#[derive(Clone, Copy)]
struct UiDamageValidationMeasurement {
    stats: CopyValidationStats,
    completed_flips: u64,
}

impl UiDamageValidationMeasurement {
    fn passes(self) -> bool {
        self.stats.checks == u32::from(ANIMATION_LAB_DAMAGE_VALIDATION_FRAMES)
            && self.stats.mismatches == 0
            && self.stats.buffer_checks == [6, 6, 6]
            && self.stats.damage_checks > 0
            && self.stats.full_checks > 0
            && self.stats.seed_checks == 2
            && self.stats.fallback_checks == u32::from(ANIMATION_LAB_DAMAGE_PATTERN_STATES + 1)
            && self.stats.fallback_verified
            && self.completed_flips == u64::from(ANIMATION_LAB_DAMAGE_VALIDATION_FRAMES)
    }
}

#[derive(Clone, Copy)]
enum UiAnimationLabPhase {
    Disabled,
    WaitForDisplay,
    OpenMenuAt(Instant),
    SelectShutdownAt(Instant),
    WarmShutdownFrameAt(Instant, u8),
    StartShutdownAt(Instant),
    StopShutdownAt(Instant),
    SelectRebootAt(Instant),
    WarmRebootFrameAt(Instant, u8),
    StartRebootAt(Instant),
    StopRebootAt(Instant),
    BeginDamageValidationAt(Instant),
    DamageValidationFrameAt(Instant, u8),
    FinishDamageValidationAt(Instant),
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
            damage_validation: None,
            damage_validation_baseline_flips: 0,
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
                self.phase = UiAnimationLabPhase::WarmShutdownFrameAt(
                    Instant::now() + ANIMATION_LAB_SETTLE,
                    ANIMATION_LAB_WARMUP_FRAMES,
                );
            }
            UiAnimationLabPhase::SelectShutdownAt(_) => {}
            UiAnimationLabPhase::WarmShutdownFrameAt(deadline, remaining) if now >= deadline => {
                display.wait_for_page_flip()?;
                window.window().request_redraw();
                self.phase = if remaining > 1 {
                    UiAnimationLabPhase::WarmShutdownFrameAt(Instant::now(), remaining - 1)
                } else {
                    UiAnimationLabPhase::StartShutdownAt(Instant::now())
                };
            }
            UiAnimationLabPhase::WarmShutdownFrameAt(_, _) => {}
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
                let (report, copy) = display.finish_frame_rate_measurement();
                self.shutdown = report.map(|report| UiAnimationLabMeasurement {
                    report,
                    progress_milli,
                    copy,
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
                self.phase = UiAnimationLabPhase::WarmRebootFrameAt(
                    Instant::now() + ANIMATION_LAB_SETTLE,
                    ANIMATION_LAB_WARMUP_FRAMES,
                );
            }
            UiAnimationLabPhase::SelectRebootAt(_) => {}
            UiAnimationLabPhase::WarmRebootFrameAt(deadline, remaining) if now >= deadline => {
                display.wait_for_page_flip()?;
                window.window().request_redraw();
                self.phase = if remaining > 1 {
                    UiAnimationLabPhase::WarmRebootFrameAt(Instant::now(), remaining - 1)
                } else {
                    UiAnimationLabPhase::StartRebootAt(Instant::now())
                };
            }
            UiAnimationLabPhase::WarmRebootFrameAt(_, _) => {}
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
                let (report, copy) = display.finish_frame_rate_measurement();
                self.reboot = report.map(|report| UiAnimationLabMeasurement {
                    report,
                    progress_milli,
                    copy,
                });
                window.invoke_hardware_power_released();
                tracing::info!(
                    action = "reboot",
                    measured = self.reboot.is_some(),
                    progress_milli,
                    "POCKETBOOT_UI_ANIMATION_LAB_HOLD_STOP"
                );
                self.phase = UiAnimationLabPhase::BeginDamageValidationAt(
                    Instant::now() + ANIMATION_LAB_SETTLE,
                );
            }
            UiAnimationLabPhase::StopRebootAt(_) => {}
            UiAnimationLabPhase::BeginDamageValidationAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                self.damage_validation_baseline_flips = display.completed_page_flips;
                display.begin_copy_validation()?;
                debug_assert!(damage_validation_is_reference(0));
                display.force_back_buffer_full_copy();
                window.set_damage_test_pattern(damage_validation_pattern(0));
                tracing::info!(
                    frames = ANIMATION_LAB_DAMAGE_VALIDATION_FRAMES,
                    pattern_states = ANIMATION_LAB_DAMAGE_PATTERN_STATES,
                    frames_per_state = ANIMATION_LAB_DAMAGE_FRAMES_PER_STATE,
                    "POCKETBOOT_UI_DAMAGE_VALIDATION_START"
                );
                self.phase = UiAnimationLabPhase::DamageValidationFrameAt(
                    Instant::now() + ANIMATION_LAB_DAMAGE_FRAME_DELAY,
                    1,
                );
            }
            UiAnimationLabPhase::BeginDamageValidationAt(_) => {}
            UiAnimationLabPhase::DamageValidationFrameAt(deadline, frame) if now >= deadline => {
                display.wait_for_page_flip()?;
                display.log_copy_validation_flip(frame - 1)?;
                let pattern = damage_validation_pattern(frame);
                if damage_validation_is_reference(frame) {
                    display.force_back_buffer_full_copy();
                }
                if window.get_damage_test_pattern() != pattern {
                    window.set_damage_test_pattern(pattern);
                } else {
                    window.window().request_redraw();
                }

                if frame + 1 < ANIMATION_LAB_DAMAGE_VALIDATION_FRAMES {
                    self.phase = UiAnimationLabPhase::DamageValidationFrameAt(
                        Instant::now() + ANIMATION_LAB_DAMAGE_FRAME_DELAY,
                        frame + 1,
                    );
                } else {
                    self.phase = UiAnimationLabPhase::FinishDamageValidationAt(
                        Instant::now() + ANIMATION_LAB_DAMAGE_FRAME_DELAY,
                    );
                }
            }
            UiAnimationLabPhase::DamageValidationFrameAt(_, _) => {}
            UiAnimationLabPhase::FinishDamageValidationAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                display.log_copy_validation_flip(ANIMATION_LAB_DAMAGE_VALIDATION_FRAMES - 1)?;
                let stats = display.finish_copy_validation();
                let completed_flips = display
                    .completed_page_flips
                    .saturating_sub(self.damage_validation_baseline_flips);
                self.damage_validation = Some(UiDamageValidationMeasurement {
                    stats,
                    completed_flips,
                });
                tracing::info!(
                    checks = stats.checks,
                    mismatches = stats.mismatches,
                    buffer_0_checks = stats.buffer_checks[0],
                    buffer_1_checks = stats.buffer_checks[1],
                    buffer_2_checks = stats.buffer_checks[2],
                    damage_checks = stats.damage_checks,
                    clean_checks = stats.clean_checks,
                    full_checks = stats.full_checks,
                    seed_checks = stats.seed_checks,
                    fallback_checks = stats.fallback_checks,
                    fallback_verified = stats.fallback_verified,
                    completed_flips,
                    "POCKETBOOT_UI_DAMAGE_VALIDATION_RESULT"
                );
                self.phase = UiAnimationLabPhase::FinishAt(
                    Instant::now() + ANIMATION_LAB_DAMAGE_FRAME_DELAY,
                );
            }
            UiAnimationLabPhase::FinishDamageValidationAt(_) => {}
            UiAnimationLabPhase::FinishAt(deadline) if now >= deadline => {
                display.wait_for_page_flip()?;
                let shutdown = self.shutdown;
                let reboot = self.reboot;
                let damage_validation = self.damage_validation;
                let scanout_frame_bytes = display.frame_byte_len();
                let visible_frame_bytes = display.visible_frame_byte_len();
                let scale_factor_milli = scale_factor_milli(window.window().scale_factor());
                let passed = shutdown
                    .is_some_and(|measurement| measurement.passes(visible_frame_bytes))
                    && reboot.is_some_and(|measurement| measurement.passes(visible_frame_bytes))
                    && damage_validation.is_some_and(UiDamageValidationMeasurement::passes)
                    && scale_factor_milli == 2_000;
                tracing::info!(
                    passed,
                    scale_factor_milli,
                    scanout_frame_bytes,
                    visible_frame_bytes,
                    shutdown_measured = shutdown.is_some(),
                    shutdown_fps_milli = shutdown.map_or(0, |m| m.report.fps_milli),
                    shutdown_vblank_fps_milli = shutdown.map_or(0, |m| m.report.vblank_fps_milli),
                    shutdown_flips = shutdown.map_or(0, |m| m.report.completed_flips),
                    shutdown_p50_us = shutdown.map_or(0, |m| m.report.interval_p50_us),
                    shutdown_p95_us = shutdown.map_or(0, |m| m.report.interval_p95_us),
                    shutdown_max_us = shutdown.map_or(0, |m| m.report.interval_max_us),
                    shutdown_progress_milli = shutdown.map_or(0, |m| m.progress_milli),
                    shutdown_damage_frames = shutdown.map_or(0, |m| m.copy.damage_frames),
                    shutdown_full_frames = shutdown.map_or(0, |m| m.copy.full_frames),
                    shutdown_copied_bytes = shutdown.map_or(0, |m| m.copy.copied_bytes),
                    shutdown_max_damage_bytes = shutdown.map_or(0, |m| m.copy.max_damage_bytes),
                    reboot_measured = reboot.is_some(),
                    reboot_fps_milli = reboot.map_or(0, |m| m.report.fps_milli),
                    reboot_vblank_fps_milli = reboot.map_or(0, |m| m.report.vblank_fps_milli),
                    reboot_flips = reboot.map_or(0, |m| m.report.completed_flips),
                    reboot_p50_us = reboot.map_or(0, |m| m.report.interval_p50_us),
                    reboot_p95_us = reboot.map_or(0, |m| m.report.interval_p95_us),
                    reboot_max_us = reboot.map_or(0, |m| m.report.interval_max_us),
                    reboot_progress_milli = reboot.map_or(0, |m| m.progress_milli),
                    reboot_damage_frames = reboot.map_or(0, |m| m.copy.damage_frames),
                    reboot_full_frames = reboot.map_or(0, |m| m.copy.full_frames),
                    reboot_copied_bytes = reboot.map_or(0, |m| m.copy.copied_bytes),
                    reboot_max_damage_bytes = reboot.map_or(0, |m| m.copy.max_damage_bytes),
                    damage_verified_frames = damage_validation.map_or(0, |m| m.stats.checks),
                    damage_mismatch_frames = damage_validation.map_or(0, |m| m.stats.mismatches),
                    damage_buffer_0_checks =
                        damage_validation.map_or(0, |m| m.stats.buffer_checks[0]),
                    damage_buffer_1_checks =
                        damage_validation.map_or(0, |m| m.stats.buffer_checks[1]),
                    damage_buffer_2_checks =
                        damage_validation.map_or(0, |m| m.stats.buffer_checks[2]),
                    damage_completed_flips = damage_validation.map_or(0, |m| m.completed_flips),
                    damage_seed_checks = damage_validation.map_or(0, |m| m.stats.seed_checks),
                    damage_fallback_checks =
                        damage_validation.map_or(0, |m| m.stats.fallback_checks),
                    fallback_full_copy_verified =
                        damage_validation.is_some_and(|m| m.stats.fallback_verified),
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

fn damage_validation_pattern(frame: u8) -> i32 {
    if frame < ANIMATION_LAB_DAMAGE_PATTERN_FRAMES {
        i32::from(frame / ANIMATION_LAB_DAMAGE_FRAMES_PER_STATE + 1)
    } else {
        0
    }
}

fn damage_validation_is_reference(frame: u8) -> bool {
    let state = frame / ANIMATION_LAB_DAMAGE_FRAMES_PER_STATE;
    frame % ANIMATION_LAB_DAMAGE_FRAMES_PER_STATE == state % ANIMATION_LAB_DAMAGE_FRAMES_PER_STATE
}

fn scale_factor_milli(scale_factor: f32) -> u64 {
    u64::try_from((scale_factor.max(0.0) * 1000.0).round() as i64).unwrap_or(0)
}

fn logical_touch_position(report: TouchReport, scale_factor: f32) -> LogicalPosition {
    let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    LogicalPosition::new(report.x / scale_factor, report.y / scale_factor)
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
    last_page_flip_vblank: Option<u32>,
    last_page_flip_buffer_id: Option<u8>,
    submitted_frames: u64,
    completed_page_flips: u64,
    frame_rate_stats: FrameRateStats,
    frame_rate_measurement_action: Option<&'static str>,
    frame_rate_measurement_report: Option<FrameRateReport>,
    copy_measurement_active: bool,
    copy_measurement_stats: CopyStats,
    copy_validation_active: bool,
    copy_validation_stats: CopyValidationStats,
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

        let front_buffer = KmsBuffer::allocate(&card, size, format, 0)?;
        let back_buffer = KmsBuffer::allocate(&card, size, format, 1)?;
        let in_flight_buffer = KmsBuffer::allocate(&card, size, format, 2)?;
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
            last_page_flip_vblank: None,
            last_page_flip_buffer_id: None,
            submitted_frames: 0,
            completed_page_flips: 0,
            frame_rate_stats,
            frame_rate_measurement_action: None,
            frame_rate_measurement_report: None,
            copy_measurement_active: false,
            copy_measurement_stats: CopyStats::default(),
            copy_validation_active: false,
            copy_validation_stats: CopyValidationStats::default(),
            lab_page_flip_markers_remaining: 0,
        })
    }

    fn start_frame_rate_measurement(&mut self, action: &'static str) {
        self.frame_rate_stats.clear();
        self.frame_rate_measurement_action = Some(action);
        self.frame_rate_measurement_report = None;
        self.copy_measurement_active = true;
        self.copy_measurement_stats = CopyStats::default();
    }

    fn finish_frame_rate_measurement(&mut self) -> (Option<FrameRateReport>, CopyStats) {
        self.frame_rate_measurement_action = None;
        self.copy_measurement_active = false;
        (
            self.frame_rate_measurement_report.take(),
            self.copy_measurement_stats,
        )
    }

    fn begin_copy_validation(&mut self) -> Result<(), String> {
        let byte_len = self.heap_buffer.as_bytes(self.format)?.len();
        let allocate_shadow = || {
            let mut shadow = Vec::new();
            shadow
                .try_reserve_exact(byte_len)
                .map_err(|err| format!("allocate {byte_len}-byte BO validation shadow: {err}"))?;
            shadow.resize(byte_len, 0);
            Ok::<_, String>(shadow)
        };
        let front_shadow = allocate_shadow()?;
        let back_shadow = allocate_shadow()?;
        let in_flight_shadow = allocate_shadow()?;

        self.front_buffer.validation_shadow = Some(front_shadow);
        self.back_buffer.validation_shadow = Some(back_shadow);
        self.in_flight_buffer.validation_shadow = Some(in_flight_shadow);
        self.front_buffer.pending_damage = PendingDamage::Full(FullCopyReason::ValidationSeed);
        self.back_buffer.pending_damage = PendingDamage::Full(FullCopyReason::ValidationSeed);
        self.in_flight_buffer.pending_damage = PendingDamage::Full(FullCopyReason::ValidationSeed);
        self.copy_validation_stats = CopyValidationStats::default();
        self.copy_validation_active = true;
        Ok(())
    }

    fn force_back_buffer_full_copy(&mut self) {
        self.back_buffer.pending_damage = PendingDamage::Full(FullCopyReason::ValidationFallback);
    }

    fn finish_copy_validation(&mut self) -> CopyValidationStats {
        self.copy_validation_active = false;
        self.front_buffer.validation_shadow = None;
        self.back_buffer.validation_shadow = None;
        self.in_flight_buffer.validation_shadow = None;
        self.copy_validation_stats
    }

    fn log_copy_validation_flip(&self, validation_frame: u8) -> Result<(), String> {
        let vblank_frame = self
            .last_page_flip_vblank
            .ok_or_else(|| "copy validation page flip has no vblank frame".to_string())?;
        let buffer_id = self
            .last_page_flip_buffer_id
            .ok_or_else(|| "copy validation page flip has no buffer ID".to_string())?;
        tracing::info!(
            validation_frame,
            pattern = damage_validation_pattern(validation_frame),
            reference = damage_validation_is_reference(validation_frame),
            buffer_id,
            vblank_frame,
            completed_flips = self.completed_page_flips,
            "POCKETBOOT_UI_DAMAGE_VALIDATION_FLIP"
        );
        Ok(())
    }

    fn frame_byte_len(&self) -> u64 {
        u64::try_from(self.heap_buffer.byte_len()).unwrap_or(u64::MAX)
    }

    fn visible_frame_byte_len(&self) -> u64 {
        u64::from(self.width)
            .saturating_mul(u64::from(self.height))
            .saturating_mul(bytes_per_pixel(self.format) as u64)
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
        let pitch = self.back_buffer.buffer.pitch();
        let stride = pitch as usize / bytes_per_pixel(format);
        let buffer_age = self.heap_buffer.age;
        renderer.set_repaint_buffer_type(self.heap_buffer.repaint_buffer_type());

        let render = self.heap_buffer.render(renderer, format, stride)?;
        let frame_damage = PendingDamage::from_physical_region(
            &render.damage,
            self.width,
            self.height,
            render.new_buffer,
        );
        self.front_buffer.pending_damage.accumulate(&frame_damage);
        self.back_buffer.pending_damage.accumulate(&frame_damage);
        self.in_flight_buffer
            .pending_damage
            .accumulate(&frame_damage);

        let map_start = Instant::now();
        let mut mapping = self
            .card
            .map_dumb_buffer(&mut self.back_buffer.buffer)
            .map_err(|err| format!("map DRM dumb buffer: {err}"))?;
        let map_duration = map_start.elapsed();
        let pending_damage = self.back_buffer.pending_damage.clone();
        let copy = self.heap_buffer.copy_to_drm(
            mapping.as_mut(),
            format,
            pitch,
            self.width,
            self.height,
            &pending_damage,
        )?;
        drop(mapping);
        if self.copy_measurement_active {
            self.copy_measurement_stats.record(copy);
        }
        if self.copy_validation_active {
            let shadow = self
                .back_buffer
                .validation_shadow
                .as_mut()
                .ok_or_else(|| "copy validation back buffer has no shadow".to_string())?;
            let mismatch = self.heap_buffer.update_validation_shadow(
                shadow,
                format,
                pitch,
                self.width,
                self.height,
                &pending_damage,
                copy.mode,
            )?;
            self.copy_validation_stats
                .record(self.back_buffer.id, copy, mismatch.is_none());
            if let Some(mismatch) = mismatch {
                tracing::error!(
                    buffer_id = self.back_buffer.id,
                    byte_offset = mismatch.byte_offset,
                    x = mismatch.x,
                    y = mismatch.y,
                    expected = mismatch.expected,
                    actual = mismatch.actual,
                    "POCKETBOOT_UI_DAMAGE_VALIDATION_MISMATCH"
                );
            }
        }
        self.back_buffer.pending_damage = PendingDamage::Clean;

        tracing::trace!(
            frame,
            path = "cached_heap",
            buffer_id = self.back_buffer.id,
            format = ?format,
            stride,
            buffer_age,
            map_us = duration_us(map_duration),
            renderer_us = duration_us(render.duration),
            copy_mode = ?copy.mode,
            copy_rects = copy.rect_count,
            copied_bytes = copy.copied_bytes,
            copy_us = duration_us(copy.duration),
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
                self.last_page_flip_vblank = Some(page_flip.frame);
                self.last_page_flip_buffer_id = Some(self.in_flight_buffer.id);
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

    fn pixel_bytes_mut<T: DrmPixel>(pixels: &mut [T]) -> &mut [u8] {
        let byte_len = mem::size_of_val(pixels);
        // SAFETY: This is the mutable counterpart of pixels_as_bytes; DrmPixel guarantees that
        // every byte belongs to an initialized pixel representation.
        unsafe { slice::from_raw_parts_mut(pixels.as_mut_ptr().cast(), byte_len) }
    }

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
                .copy_to_drm(
                    &mut mapping,
                    DrmFourcc::Xrgb8888,
                    64,
                    16,
                    3,
                    &PendingDamage::Full(FullCopyReason::VirginBuffer),
                )
                .is_err()
        );
    }

    #[test]
    fn damage_rectangles_are_clipped_to_the_physical_frame() {
        assert_eq!(clipped_axis(-3, 8, 10), Some((0, 5)));
        assert_eq!(clipped_axis(8, 8, 10), Some((8, 2)));
        assert_eq!(clipped_axis(-8, 3, 10), None);
        assert_eq!(clipped_axis(10, 3, 10), None);
        assert_eq!(clipped_axis(4, 0, 10), None);
    }

    #[test]
    fn pending_damage_accumulates_until_the_buffer_is_current() {
        let first = PixelRect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        let second = PixelRect {
            x: 5,
            y: 6,
            width: 7,
            height: 8,
        };
        let mut pending = PendingDamage::Clean;

        pending.accumulate(&PendingDamage::Rects(vec![first]));
        pending.accumulate(&PendingDamage::Clean);
        pending.accumulate(&PendingDamage::Rects(vec![second]));

        assert_eq!(pending, PendingDamage::Rects(vec![first, second]));
        pending.accumulate(&PendingDamage::Full(FullCopyReason::NewRenderBuffer));
        assert_eq!(
            pending,
            PendingDamage::Full(FullCopyReason::NewRenderBuffer)
        );
        pending.accumulate(&PendingDamage::Rects(vec![first]));
        assert_eq!(
            pending,
            PendingDamage::Full(FullCopyReason::NewRenderBuffer)
        );
    }

    #[test]
    fn too_many_accumulated_rectangles_fall_back_to_a_full_copy() {
        let rects = (0..MAX_PENDING_DAMAGE_RECTS)
            .map(|index| PixelRect {
                x: (index * 2) as u32,
                y: 0,
                width: 1,
                height: 1,
            })
            .collect();
        let mut pending = PendingDamage::Rects(rects);

        pending.accumulate(&PendingDamage::Rects(vec![PixelRect {
            x: (MAX_PENDING_DAMAGE_RECTS * 2) as u32,
            y: 0,
            width: 1,
            height: 1,
        }]));

        assert_eq!(pending, PendingDamage::Full(FullCopyReason::TooManyRects));
    }

    #[test]
    fn accumulated_overlapping_damage_is_coalesced_without_extra_copy_area() {
        let mut pending = PendingDamage::Rects(vec![PixelRect {
            x: 10,
            y: 20,
            width: 30,
            height: 10,
        }]);

        pending.accumulate(&PendingDamage::Rects(vec![PixelRect {
            x: 10,
            y: 20,
            width: 50,
            height: 10,
        }]));
        pending.accumulate(&PendingDamage::Rects(vec![PixelRect {
            x: 60,
            y: 20,
            width: 5,
            height: 10,
        }]));

        assert_eq!(
            pending,
            PendingDamage::Rects(vec![PixelRect {
                x: 10,
                y: 20,
                width: 55,
                height: 10,
            }])
        );
    }

    #[test]
    fn partial_copy_updates_only_damage_and_preserves_pitch_padding() {
        const WIDTH: u32 = 3;
        const HEIGHT: u32 = 3;
        const STRIDE: usize = 5;
        let source = (1..=STRIDE * HEIGHT as usize)
            .map(|value| Xrgb8888Pixel(value as u32))
            .collect::<Vec<_>>();
        let untouched = Xrgb8888Pixel(0xdead_beef);
        let mut destination = vec![untouched; source.len()];
        let damage = PendingDamage::Rects(vec![PixelRect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        }]);

        let outcome = copy_pixels_to_drm(
            pixel_bytes_mut(&mut destination),
            &source,
            DrmFourcc::Xrgb8888,
            (STRIDE * mem::size_of::<Xrgb8888Pixel>()) as u32,
            WIDTH,
            HEIGHT,
            &damage,
        )
        .unwrap();

        assert_eq!(outcome.mode, CopyMode::Damage);
        assert_eq!(outcome.rect_count, 1);
        assert_eq!(outcome.copied_bytes, 16);
        for index in 0..source.len() {
            if [6, 7, 11, 12].contains(&index) {
                assert_eq!(destination[index].0, source[index].0, "pixel {index}");
            } else {
                assert_eq!(destination[index].0, untouched.0, "pixel {index}");
            }
        }
    }

    #[test]
    fn validation_shadow_full_seed_copies_visible_pixels_and_pitch_padding() {
        const WIDTH: u32 = 3;
        const HEIGHT: u32 = 2;
        const STRIDE: usize = 5;
        const PITCH: u32 = (STRIDE * mem::size_of::<Xrgb8888Pixel>()) as u32;
        let mut buffer = CachedRenderBuffer::new(DrmFourcc::Xrgb8888, PITCH, HEIGHT).unwrap();
        let CachedRenderPixels::Xrgb8888(pixels) = &mut buffer.pixels else {
            panic!("XRGB8888 buffer used a different cached pixel representation");
        };
        for (index, pixel) in pixels.iter_mut().enumerate() {
            *pixel = Xrgb8888Pixel(0x1000_0000 + index as u32);
        }
        let mut shadow = vec![0xa5; buffer.byte_len()];
        let damage = PendingDamage::Full(FullCopyReason::ValidationSeed);

        let mismatch = buffer
            .update_validation_shadow(
                &mut shadow,
                DrmFourcc::Xrgb8888,
                PITCH,
                WIDTH,
                HEIGHT,
                &damage,
                CopyMode::Full(FullCopyReason::ValidationSeed),
            )
            .unwrap();

        assert_eq!(mismatch, None);
        assert_eq!(shadow, buffer.as_bytes(DrmFourcc::Xrgb8888).unwrap());
        for padding_pixel in [3usize, 4, 8, 9] {
            let start = padding_pixel * mem::size_of::<Xrgb8888Pixel>();
            assert_eq!(
                &shadow[start..start + mem::size_of::<Xrgb8888Pixel>()],
                &buffer.as_bytes(DrmFourcc::Xrgb8888).unwrap()
                    [start..start + mem::size_of::<Xrgb8888Pixel>()],
                "pitch-padding pixel {padding_pixel}"
            );
        }
    }

    #[test]
    fn validation_shadow_damage_updates_only_rectangles_across_pitched_rows() {
        const WIDTH: u32 = 3;
        const HEIGHT: u32 = 3;
        const STRIDE: usize = 5;
        const PITCH: u32 = (STRIDE * mem::size_of::<Xrgb8888Pixel>()) as u32;
        let mut buffer = CachedRenderBuffer::new(DrmFourcc::Xrgb8888, PITCH, HEIGHT).unwrap();
        let mut shadow = vec![0; buffer.byte_len()];
        let seed = PendingDamage::Full(FullCopyReason::ValidationSeed);
        buffer
            .update_validation_shadow(
                &mut shadow,
                DrmFourcc::Xrgb8888,
                PITCH,
                WIDTH,
                HEIGHT,
                &seed,
                CopyMode::Full(FullCopyReason::ValidationSeed),
            )
            .unwrap();
        let before = shadow.clone();

        let CachedRenderPixels::Xrgb8888(pixels) = &mut buffer.pixels else {
            panic!("XRGB8888 buffer used a different cached pixel representation");
        };
        for (index, value) in [(6usize, 1u32), (7, 2), (11, 3), (12, 4)] {
            pixels[index] = Xrgb8888Pixel(0xff00_0000 | value);
        }
        let damage = PendingDamage::Rects(vec![PixelRect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        }]);

        let mismatch = buffer
            .update_validation_shadow(
                &mut shadow,
                DrmFourcc::Xrgb8888,
                PITCH,
                WIDTH,
                HEIGHT,
                &damage,
                CopyMode::Damage,
            )
            .unwrap();

        assert_eq!(mismatch, None);
        assert_eq!(shadow, buffer.as_bytes(DrmFourcc::Xrgb8888).unwrap());
        for padding_pixel in [3usize, 4, 8, 9, 13, 14] {
            let start = padding_pixel * mem::size_of::<Xrgb8888Pixel>();
            assert_eq!(
                &shadow[start..start + mem::size_of::<Xrgb8888Pixel>()],
                &before[start..start + mem::size_of::<Xrgb8888Pixel>()],
                "pitch-padding pixel {padding_pixel}"
            );
        }
    }

    #[test]
    fn validation_shadow_clean_frame_detects_a_stale_visible_pixel() {
        const WIDTH: u32 = 3;
        const HEIGHT: u32 = 2;
        const STRIDE: usize = 5;
        const PITCH: u32 = (STRIDE * mem::size_of::<Xrgb8888Pixel>()) as u32;
        let mut buffer = CachedRenderBuffer::new(DrmFourcc::Xrgb8888, PITCH, HEIGHT).unwrap();
        let mut shadow = vec![0; buffer.byte_len()];
        let seed = PendingDamage::Full(FullCopyReason::ValidationSeed);
        buffer
            .update_validation_shadow(
                &mut shadow,
                DrmFourcc::Xrgb8888,
                PITCH,
                WIDTH,
                HEIGHT,
                &seed,
                CopyMode::Full(FullCopyReason::ValidationSeed),
            )
            .unwrap();

        let CachedRenderPixels::Xrgb8888(pixels) = &mut buffer.pixels else {
            panic!("XRGB8888 buffer used a different cached pixel representation");
        };
        pixels[STRIDE + 2] = Xrgb8888Pixel(0xff12_3456);

        let mismatch = buffer
            .update_validation_shadow(
                &mut shadow,
                DrmFourcc::Xrgb8888,
                PITCH,
                WIDTH,
                HEIGHT,
                &PendingDamage::Clean,
                CopyMode::Clean,
            )
            .unwrap()
            .expect("clean validation must report the stale pixel");

        assert_eq!((mismatch.x, mismatch.y), (2, 1));
        assert_ne!(mismatch.expected, mismatch.actual);
    }

    #[test]
    fn clean_frame_performs_no_copy_and_is_not_counted_as_damage() {
        const WIDTH: u32 = 2;
        const HEIGHT: u32 = 2;
        let source = vec![Xrgb8888Pixel(1); WIDTH as usize * HEIGHT as usize];
        let untouched = Xrgb8888Pixel(2);
        let mut destination = vec![untouched; source.len()];

        let outcome = copy_pixels_to_drm(
            pixel_bytes_mut(&mut destination),
            &source,
            DrmFourcc::Xrgb8888,
            WIDTH * mem::size_of::<Xrgb8888Pixel>() as u32,
            WIDTH,
            HEIGHT,
            &PendingDamage::Clean,
        )
        .unwrap();
        let mut stats = CopyStats::default();
        stats.record(outcome);

        assert_eq!(outcome.mode, CopyMode::Clean);
        assert_eq!(outcome.copied_bytes, 0);
        assert!(destination.iter().all(|pixel| pixel.0 == untouched.0));
        assert_eq!(stats, CopyStats::default());
    }

    #[test]
    fn expensive_damage_and_explicit_validation_use_full_copy_fallbacks() {
        const WIDTH: u32 = 4;
        const HEIGHT: u32 = 3;
        let source = (1..=WIDTH as usize * HEIGHT as usize)
            .map(|value| Xrgb8888Pixel(value as u32))
            .collect::<Vec<_>>();
        let full_rect = PixelRect {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
        };

        for (damage, expected_reason) in [
            (
                PendingDamage::Rects(vec![full_rect]),
                FullCopyReason::DamageCost,
            ),
            (
                PendingDamage::Full(FullCopyReason::ValidationFallback),
                FullCopyReason::ValidationFallback,
            ),
        ] {
            let mut destination = vec![Xrgb8888Pixel::default(); source.len()];
            let outcome = copy_pixels_to_drm(
                pixel_bytes_mut(&mut destination),
                &source,
                DrmFourcc::Xrgb8888,
                WIDTH * mem::size_of::<Xrgb8888Pixel>() as u32,
                WIDTH,
                HEIGHT,
                &damage,
            )
            .unwrap();

            assert_eq!(outcome.mode, CopyMode::Full(expected_reason));
            assert!(
                destination
                    .iter()
                    .zip(&source)
                    .all(|(actual, expected)| actual.0 == expected.0)
            );
        }
    }

    #[test]
    fn triple_buffer_damage_history_produces_exact_frames_across_reuse() {
        const WIDTH: u32 = 8;
        const HEIGHT: u32 = 6;
        const PIXELS: usize = WIDTH as usize * HEIGHT as usize;
        let mut canonical = vec![Xrgb8888Pixel::default(); PIXELS];
        let mut buffers = [
            vec![Xrgb8888Pixel::default(); PIXELS],
            vec![Xrgb8888Pixel::default(); PIXELS],
            vec![Xrgb8888Pixel::default(); PIXELS],
        ];
        let mut pending = [
            PendingDamage::Full(FullCopyReason::VirginBuffer),
            PendingDamage::Full(FullCopyReason::VirginBuffer),
            PendingDamage::Full(FullCopyReason::VirginBuffer),
        ];

        for frame in 0..9usize {
            let rect = PixelRect {
                x: (frame % WIDTH as usize) as u32,
                y: (frame * 2 % HEIGHT as usize) as u32,
                width: 1,
                height: 1,
            };
            let index = rect.y as usize * WIDTH as usize + rect.x as usize;
            canonical[index] = Xrgb8888Pixel((frame + 1) as u32);
            let frame_damage = PendingDamage::Rects(vec![rect]);
            for buffer_damage in &mut pending {
                buffer_damage.accumulate(&frame_damage);
            }

            let current = frame % buffers.len();
            let outcome = copy_pixels_to_drm(
                pixel_bytes_mut(&mut buffers[current]),
                &canonical,
                DrmFourcc::Xrgb8888,
                WIDTH * mem::size_of::<Xrgb8888Pixel>() as u32,
                WIDTH,
                HEIGHT,
                &pending[current],
            )
            .unwrap();
            pending[current] = PendingDamage::Clean;

            if frame < 3 {
                assert_eq!(outcome.mode, CopyMode::Full(FullCopyReason::VirginBuffer));
            } else {
                assert_eq!(outcome.mode, CopyMode::Damage);
                assert_eq!(outcome.rect_count, 3);
            }
            assert!(
                buffers[current]
                    .iter()
                    .zip(&canonical)
                    .all(|(actual, expected)| actual.0 == expected.0)
            );
        }
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

    #[test]
    fn animation_lab_requires_fifty_five_delivered_partial_frames() {
        let passing = UiAnimationLabMeasurement {
            report: FrameRateReport {
                completed_flips: 55,
                elapsed_us: 1_000_000,
                vblank_delta: 60,
                fps_milli: 55_000,
                vblank_fps_milli: 55_000,
                interval_p50_us: 18_000,
                interval_p95_us: 33_500,
                interval_max_us: 50_000,
            },
            progress_milli: 750,
            copy: CopyStats {
                damage_frames: 55,
                full_frames: 0,
                copied_bytes: 55_000,
                max_damage_bytes: 999,
            },
        };

        assert!(passing.passes(1_000));
        let mut too_slow = passing;
        too_slow.report.fps_milli = 54_999;
        assert!(!too_slow.passes(1_000));
        let mut used_full_copy = passing;
        used_full_copy.copy.full_frames = 1;
        assert!(!used_full_copy.passes(1_000));
        let mut copied_full_frame = passing;
        copied_full_frame.copy.max_damage_bytes = 1_000;
        assert!(!copied_full_frame.passes(1_000));
    }

    #[test]
    fn damage_validation_requires_all_three_buffers_and_explicit_fallback() {
        let passing = UiDamageValidationMeasurement {
            stats: CopyValidationStats {
                checks: 18,
                mismatches: 0,
                buffer_checks: [6, 6, 6],
                damage_checks: 8,
                clean_checks: 0,
                full_checks: 10,
                seed_checks: 2,
                fallback_checks: 6,
                fallback_verified: true,
            },
            completed_flips: 18,
        };

        assert!(passing.passes());
        let mut stale = passing;
        stale.stats.mismatches = 1;
        assert!(!stale.passes());
        let mut missed_buffer = passing;
        missed_buffer.stats.buffer_checks = [9, 9, 0];
        assert!(!missed_buffer.passes());
        let mut no_fallback = passing;
        no_fallback.stats.fallback_verified = false;
        assert!(!no_fallback.passes());
    }

    #[test]
    fn damage_validation_holds_each_pattern_for_three_buffers_and_rotates_reference() {
        let patterns = (0..ANIMATION_LAB_DAMAGE_VALIDATION_FRAMES)
            .map(damage_validation_pattern)
            .collect::<Vec<_>>();
        assert_eq!(
            patterns,
            vec![1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 5, 0, 0, 0]
        );

        let reference_frames = (0..ANIMATION_LAB_DAMAGE_VALIDATION_FRAMES)
            .filter(|frame| damage_validation_is_reference(*frame))
            .collect::<Vec<_>>();
        assert_eq!(reference_frames, vec![0, 4, 8, 9, 13, 17]);
        assert_eq!(
            reference_frames
                .iter()
                .map(|frame| frame % ANIMATION_LAB_DAMAGE_FRAMES_PER_STATE)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 0, 1, 2]
        );
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
    id: u8,
    framebuffer: framebuffer::Handle,
    buffer: control::dumbbuffer::DumbBuffer,
    pending_damage: PendingDamage,
    validation_shadow: Option<Vec<u8>>,
}

impl KmsBuffer {
    fn allocate(
        card: &DrmCard,
        size: (u32, u32),
        format: DrmFourcc,
        id: u8,
    ) -> Result<Self, String> {
        let (depth, bpp) = pixel_format_params(format)?;
        let buffer = card
            .create_dumb_buffer(size, format, bpp)
            .map_err(|err| format!("create DRM dumb buffer: {err}"))?;
        let framebuffer = card
            .add_framebuffer(&buffer, depth, bpp)
            .map_err(|err| format!("create DRM framebuffer object: {err}"))?;

        Ok(Self {
            id,
            framebuffer,
            buffer,
            pending_damage: PendingDamage::Full(FullCopyReason::VirginBuffer),
            validation_shadow: None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PixelRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl PixelRect {
    fn area(self) -> u64 {
        u64::from(self.width).saturating_mul(u64::from(self.height))
    }

    fn merge_if_not_more_expensive(self, other: Self) -> Option<Self> {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let x_end = u64::from(self.x)
            .saturating_add(u64::from(self.width))
            .max(u64::from(other.x).saturating_add(u64::from(other.width)));
        let y_end = u64::from(self.y)
            .saturating_add(u64::from(self.height))
            .max(u64::from(other.y).saturating_add(u64::from(other.height)));
        let width = x_end.saturating_sub(u64::from(x));
        let height = y_end.saturating_sub(u64::from(y));
        let merged_area = width.saturating_mul(height);
        if merged_area > self.area().saturating_add(other.area()) {
            return None;
        }

        Some(Self {
            x,
            y,
            width: u32::try_from(width).ok()?,
            height: u32::try_from(height).ok()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FullCopyReason {
    VirginBuffer,
    NewRenderBuffer,
    TooManyRects,
    DamageCost,
    ValidationSeed,
    ValidationFallback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingDamage {
    Clean,
    Rects(Vec<PixelRect>),
    Full(FullCopyReason),
}

impl PendingDamage {
    fn from_physical_region(
        region: &PhysicalRegion,
        width: u32,
        height: u32,
        new_buffer: bool,
    ) -> Self {
        if new_buffer {
            return Self::Full(FullCopyReason::NewRenderBuffer);
        }

        let mut rects = Vec::new();
        for (position, size) in region.iter() {
            let Some((x, width)) = clipped_axis(position.x, size.width, width) else {
                continue;
            };
            let Some((y, height)) = clipped_axis(position.y, size.height, height) else {
                continue;
            };
            let rect = PixelRect {
                x,
                y,
                width,
                height,
            };
            if !push_coalesced_rect(&mut rects, rect) {
                return Self::Full(FullCopyReason::TooManyRects);
            }
        }

        if rects.is_empty() {
            Self::Clean
        } else {
            Self::Rects(rects)
        }
    }

    fn accumulate(&mut self, damage: &Self) {
        match (&mut *self, damage) {
            (Self::Full(_), _) | (_, Self::Clean) => {}
            (current, Self::Full(reason)) => *current = Self::Full(*reason),
            (Self::Clean, Self::Rects(rects)) => {
                let mut accumulated = Vec::with_capacity(rects.len());
                for rect in rects {
                    if !push_coalesced_rect(&mut accumulated, *rect) {
                        *self = Self::Full(FullCopyReason::TooManyRects);
                        return;
                    }
                }
                *self = Self::Rects(accumulated);
            }
            (Self::Rects(current), Self::Rects(rects)) => {
                for rect in rects {
                    if !push_coalesced_rect(current, *rect) {
                        *self = Self::Full(FullCopyReason::TooManyRects);
                        return;
                    }
                }
            }
        }
    }
}

fn push_coalesced_rect(rects: &mut Vec<PixelRect>, mut candidate: PixelRect) -> bool {
    let mut index = 0;
    while index < rects.len() {
        if let Some(merged) = candidate.merge_if_not_more_expensive(rects[index]) {
            candidate = merged;
            rects.swap_remove(index);
            index = 0;
        } else {
            index += 1;
        }
    }
    if rects.len() >= MAX_PENDING_DAMAGE_RECTS {
        return false;
    }
    rects.push(candidate);
    true
}

fn clipped_axis(origin: i32, length: u32, limit: u32) -> Option<(u32, u32)> {
    let limit = i64::from(limit);
    let start = i64::from(origin).clamp(0, limit);
    let end = (i64::from(origin) + i64::from(length)).clamp(0, limit);
    (end > start).then(|| {
        (
            u32::try_from(start).unwrap_or(0),
            u32::try_from(end - start).unwrap_or(0),
        )
    })
}

struct CachedRenderResult {
    duration: Duration,
    damage: PhysicalRegion,
    new_buffer: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyMode {
    Clean,
    Damage,
    Full(FullCopyReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CopyOutcome {
    mode: CopyMode,
    rect_count: usize,
    copied_bytes: u64,
    duration: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CopyStats {
    damage_frames: u32,
    full_frames: u32,
    copied_bytes: u64,
    max_damage_bytes: u64,
}

impl CopyStats {
    fn record(&mut self, outcome: CopyOutcome) {
        self.copied_bytes = self.copied_bytes.saturating_add(outcome.copied_bytes);
        match outcome.mode {
            CopyMode::Clean => {}
            CopyMode::Damage => {
                self.damage_frames = self.damage_frames.saturating_add(1);
                self.max_damage_bytes = self.max_damage_bytes.max(outcome.copied_bytes);
            }
            CopyMode::Full(_) => self.full_frames = self.full_frames.saturating_add(1),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CopyValidationStats {
    checks: u32,
    mismatches: u32,
    buffer_checks: [u32; 3],
    damage_checks: u32,
    clean_checks: u32,
    full_checks: u32,
    seed_checks: u32,
    fallback_checks: u32,
    fallback_verified: bool,
}

impl CopyValidationStats {
    fn record(&mut self, buffer_id: u8, outcome: CopyOutcome, matches: bool) {
        self.checks = self.checks.saturating_add(1);
        if let Some(checks) = self.buffer_checks.get_mut(buffer_id as usize) {
            *checks = checks.saturating_add(1);
        }
        match outcome.mode {
            CopyMode::Clean => self.clean_checks = self.clean_checks.saturating_add(1),
            CopyMode::Damage => self.damage_checks = self.damage_checks.saturating_add(1),
            CopyMode::Full(reason) => {
                self.full_checks = self.full_checks.saturating_add(1);
                if reason == FullCopyReason::ValidationSeed && matches {
                    self.seed_checks = self.seed_checks.saturating_add(1);
                }
                if reason == FullCopyReason::ValidationFallback && matches {
                    self.fallback_checks = self.fallback_checks.saturating_add(1);
                    self.fallback_verified = true;
                }
            }
        }
        if !matches {
            self.mismatches = self.mismatches.saturating_add(1);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DrmMismatch {
    byte_offset: usize,
    x: usize,
    y: usize,
    expected: u8,
    actual: u8,
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
    ) -> Result<CachedRenderResult, String> {
        let new_buffer = self.age == 0;
        self.age = 1;
        let render_start = Instant::now();
        let damage = match (format, &mut self.pixels) {
            (DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888, CachedRenderPixels::Xrgb8888(pixels)) => {
                renderer.render(pixels, stride)
            }
            (DrmFourcc::Bgra8888, CachedRenderPixels::Bgra8888(pixels)) => {
                renderer.render(pixels, stride)
            }
            (DrmFourcc::Rgb565, CachedRenderPixels::Rgb565(pixels)) => {
                renderer.render(pixels, stride)
            }
            _ => {
                return Err(format!(
                    "cached render buffer format does not match DRM format {format:?}"
                ));
            }
        };

        Ok(CachedRenderResult {
            duration: render_start.elapsed(),
            damage,
            new_buffer,
        })
    }

    fn copy_to_drm(
        &self,
        drm_bytes: &mut [u8],
        format: DrmFourcc,
        pitch: u32,
        width: u32,
        height: u32,
        damage: &PendingDamage,
    ) -> Result<CopyOutcome, String> {
        let copy_start = Instant::now();
        let mut outcome = match (format, &self.pixels) {
            (DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888, CachedRenderPixels::Xrgb8888(pixels)) => {
                copy_pixels_to_drm(drm_bytes, pixels, format, pitch, width, height, damage)?
            }
            (DrmFourcc::Bgra8888, CachedRenderPixels::Bgra8888(pixels)) => {
                copy_pixels_to_drm(drm_bytes, pixels, format, pitch, width, height, damage)?
            }
            (DrmFourcc::Rgb565, CachedRenderPixels::Rgb565(pixels)) => {
                copy_pixels_to_drm(drm_bytes, pixels, format, pitch, width, height, damage)?
            }
            _ => {
                return Err(format!(
                    "cached render buffer format does not match DRM format {format:?}"
                ));
            }
        };
        outcome.duration = copy_start.elapsed();

        Ok(outcome)
    }

    fn as_bytes(&self, format: DrmFourcc) -> Result<&[u8], String> {
        match (format, &self.pixels) {
            (DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888, CachedRenderPixels::Xrgb8888(pixels)) => {
                Ok(pixels_as_bytes(pixels))
            }
            (DrmFourcc::Bgra8888, CachedRenderPixels::Bgra8888(pixels)) => {
                Ok(pixels_as_bytes(pixels))
            }
            (DrmFourcc::Rgb565, CachedRenderPixels::Rgb565(pixels)) => Ok(pixels_as_bytes(pixels)),
            _ => Err(format!(
                "cached render buffer format does not match DRM format {format:?}"
            )),
        }
    }

    fn update_validation_shadow(
        &self,
        shadow: &mut [u8],
        format: DrmFourcc,
        pitch: u32,
        width: u32,
        height: u32,
        damage: &PendingDamage,
        mode: CopyMode,
    ) -> Result<Option<DrmMismatch>, String> {
        let expected = self.as_bytes(format)?;
        let pixel_size = bytes_per_pixel(format);
        let pitch =
            usize::try_from(pitch).map_err(|_| "DRM pitch does not fit usize".to_string())?;
        let width =
            usize::try_from(width).map_err(|_| "DRM width does not fit usize".to_string())?;
        let height =
            usize::try_from(height).map_err(|_| "DRM height does not fit usize".to_string())?;
        let required = pitch
            .checked_mul(height)
            .ok_or_else(|| "DRM validation byte count overflow".to_string())?;
        if shadow.len() < required {
            return Err(format!(
                "cached BO shadow is too small for validation: {} < {} bytes",
                shadow.len(),
                required
            ));
        }
        if expected.len() < required {
            return Err(format!(
                "cached render buffer is too small for validation: {} < {required} bytes",
                expected.len()
            ));
        }

        match mode {
            CopyMode::Full(_) => shadow[..required].copy_from_slice(&expected[..required]),
            CopyMode::Clean => {
                if !matches!(damage, PendingDamage::Clean) {
                    return Err("clean copy mode has non-clean pending damage".to_string());
                }
            }
            CopyMode::Damage => {
                let PendingDamage::Rects(rects) = damage else {
                    return Err("damage copy mode has no damage rectangles".to_string());
                };
                for rect in rects {
                    let x = usize::try_from(rect.x)
                        .map_err(|_| "validation damage x does not fit usize".to_string())?;
                    let y = usize::try_from(rect.y)
                        .map_err(|_| "validation damage y does not fit usize".to_string())?;
                    let rect_width = usize::try_from(rect.width)
                        .map_err(|_| "validation damage width does not fit usize".to_string())?;
                    let rect_height = usize::try_from(rect.height)
                        .map_err(|_| "validation damage height does not fit usize".to_string())?;
                    let x_end = x
                        .checked_add(rect_width)
                        .ok_or_else(|| "validation damage x overflow".to_string())?;
                    let y_end = y
                        .checked_add(rect_height)
                        .ok_or_else(|| "validation damage y overflow".to_string())?;
                    if x_end > width || y_end > height {
                        return Err(format!(
                            "validation damage rectangle lies outside framebuffer: ({x},{y})..({x_end},{y_end}) > {width}x{height}"
                        ));
                    }
                    let start_x = x
                        .checked_mul(pixel_size)
                        .ok_or_else(|| "validation damage byte x overflow".to_string())?;
                    let row_bytes = rect_width
                        .checked_mul(pixel_size)
                        .ok_or_else(|| "validation damage row size overflow".to_string())?;
                    for row in y..y_end {
                        let start = row
                            .checked_mul(pitch)
                            .and_then(|offset| offset.checked_add(start_x))
                            .ok_or_else(|| "validation damage row offset overflow".to_string())?;
                        let end = start
                            .checked_add(row_bytes)
                            .ok_or_else(|| "validation damage row end overflow".to_string())?;
                        shadow[start..end].copy_from_slice(&expected[start..end]);
                    }
                }
            }
        }

        if shadow[..required] == expected[..required] {
            return Ok(None);
        }
        let Some(byte_offset) = expected[..required]
            .iter()
            .zip(&shadow[..required])
            .position(|(expected, actual)| expected != actual)
        else {
            return Ok(None);
        };
        Ok(Some(DrmMismatch {
            byte_offset,
            x: (byte_offset % pitch) / pixel_size,
            y: byte_offset / pitch,
            expected: expected[byte_offset],
            actual: shadow[byte_offset],
        }))
    }

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
/// # Safety
///
/// Implementors must have no uninitialized padding and every bit pattern must be valid.
unsafe trait DrmPixel: Copy + PartialEq {}

fn pixels_as_bytes<T: DrmPixel>(pixels: &[T]) -> &[u8] {
    let byte_len = mem::size_of_val(pixels);
    // SAFETY: DrmPixel requires a copyable representation with no uninitialized padding, and the
    // returned byte slice has exactly the same lifetime and extent as the input pixel slice.
    unsafe { slice::from_raw_parts(pixels.as_ptr().cast(), byte_len) }
}

fn copy_pixels_to_drm<T: DrmPixel>(
    drm_bytes: &mut [u8],
    pixels: &[T],
    format: DrmFourcc,
    pitch: u32,
    width: u32,
    height: u32,
    damage: &PendingDamage,
) -> Result<CopyOutcome, String> {
    let drm_pixels = cast_buffer_mut::<T>(drm_bytes, format)?;
    let pixel_size = mem::size_of::<T>();
    let pitch = usize::try_from(pitch).map_err(|_| "DRM pitch does not fit usize".to_string())?;
    if pitch % pixel_size != 0 {
        return Err(format!(
            "DRM pitch is not aligned for {format:?}: {pitch} bytes"
        ));
    }
    let stride = pitch / pixel_size;
    let width = usize::try_from(width).map_err(|_| "DRM width does not fit usize".to_string())?;
    let height =
        usize::try_from(height).map_err(|_| "DRM height does not fit usize".to_string())?;
    if width > stride {
        return Err(format!(
            "DRM width exceeds stride for {format:?}: {width} > {stride} pixels"
        ));
    }
    let frame_pixels = stride
        .checked_mul(height)
        .ok_or_else(|| "DRM frame pixel count overflow".to_string())?;
    if drm_pixels.len() < frame_pixels || pixels.len() < frame_pixels {
        return Err(format!(
            "DRM buffer is too small for cached render: dst={}, src={}, required={} pixels",
            drm_pixels.len(),
            pixels.len(),
            frame_pixels,
        ));
    }

    let frame_bytes = frame_pixels
        .checked_mul(pixel_size)
        .ok_or_else(|| "DRM frame byte count overflow".to_string())?;
    let visible_frame_bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(pixel_size))
        .ok_or_else(|| "visible DRM frame byte count overflow".to_string())?;
    let (mode, rect_count, copied_bytes) = match damage {
        PendingDamage::Full(reason) => {
            drm_pixels[..frame_pixels].copy_from_slice(&pixels[..frame_pixels]);
            (CopyMode::Full(*reason), 1, frame_bytes)
        }
        PendingDamage::Clean => (CopyMode::Clean, 0, 0),
        PendingDamage::Rects(rects) => {
            let requested_bytes = rects.iter().try_fold(0usize, |total, rect| {
                let pixels = usize::try_from(rect.width)
                    .ok()?
                    .checked_mul(usize::try_from(rect.height).ok()?)?;
                total.checked_add(pixels.checked_mul(pixel_size)?)
            });
            if requested_bytes.is_none_or(|bytes| bytes >= visible_frame_bytes) {
                drm_pixels[..frame_pixels].copy_from_slice(&pixels[..frame_pixels]);
                (CopyMode::Full(FullCopyReason::DamageCost), 1, frame_bytes)
            } else {
                for rect in rects {
                    let x = usize::try_from(rect.x)
                        .map_err(|_| "damage x does not fit usize".to_string())?;
                    let y = usize::try_from(rect.y)
                        .map_err(|_| "damage y does not fit usize".to_string())?;
                    let rect_width = usize::try_from(rect.width)
                        .map_err(|_| "damage width does not fit usize".to_string())?;
                    let rect_height = usize::try_from(rect.height)
                        .map_err(|_| "damage height does not fit usize".to_string())?;
                    let x_end = x
                        .checked_add(rect_width)
                        .ok_or_else(|| "damage x overflow".to_string())?;
                    let y_end = y
                        .checked_add(rect_height)
                        .ok_or_else(|| "damage y overflow".to_string())?;
                    if x_end > width || y_end > height {
                        return Err(format!(
                            "damage rectangle lies outside framebuffer: ({x},{y})..({x_end},{y_end}) > {width}x{height}"
                        ));
                    }
                    for row in y..y_end {
                        let start = row
                            .checked_mul(stride)
                            .and_then(|offset| offset.checked_add(x))
                            .ok_or_else(|| "damage row offset overflow".to_string())?;
                        let end = start
                            .checked_add(rect_width)
                            .ok_or_else(|| "damage row end overflow".to_string())?;
                        drm_pixels[start..end].copy_from_slice(&pixels[start..end]);
                    }
                }
                (CopyMode::Damage, rects.len(), requested_bytes.unwrap_or(0))
            }
        }
    };
    Ok(CopyOutcome {
        mode,
        rect_count,
        copied_bytes: u64::try_from(copied_bytes).unwrap_or(u64::MAX),
        duration: Duration::ZERO,
    })
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
#[derive(Clone, Copy, Default, PartialEq)]
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
#[derive(Clone, Copy, Default, PartialEq)]
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
    fn touch_coordinates_remain_unchanged_at_one_x_scale() {
        let position = logical_touch_position(
            TouchReport {
                kind: TouchKind::Move,
                x: 321.5,
                y: 654.25,
            },
            1.0,
        );

        assert_eq!(position.x, 321.5);
        assert_eq!(position.y, 654.25);
    }

    #[test]
    fn touch_coordinates_are_converted_to_logical_pixels_at_two_x_scale() {
        let position = logical_touch_position(
            TouchReport {
                kind: TouchKind::Move,
                x: 1439.0,
                y: 2959.0,
            },
            2.0,
        );

        assert_eq!(position.x, 719.5);
        assert_eq!(position.y, 1479.5);
    }

    #[test]
    fn invalid_touch_scale_falls_back_to_one() {
        let report = TouchReport {
            kind: TouchKind::Move,
            x: 10.0,
            y: 20.0,
        };

        for scale_factor in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let position = logical_touch_position(report, scale_factor);
            assert_eq!(position.x, 10.0);
            assert_eq!(position.y, 20.0);
        }
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
