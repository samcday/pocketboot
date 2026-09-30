use std::{
    convert::Infallible,
    ffi::CString,
    fmt, fs, io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

mod ab_slots;
mod adb;
mod battery;
mod boot_state;
mod bootflow;
mod breakin;
mod cmdline;
mod fastboot;
mod gadget;
mod getty;
mod input;
mod kexec;
mod kmsg;
#[path = "kmsg-forwarder.rs"]
mod kmsg_forwarder;
mod pe;
mod power;
#[cfg(feature = "qemu")]
mod qemu;
mod reaper;
mod runtime;
mod settle;
mod ui;
mod zboot;

type Result<T> = std::result::Result<T, String>;

const SYS_BLOCK: &str = "/sys/block";
const PROC_CMDLINE: &str = "/proc/cmdline";
const ACM_CMDLINE_PARAM: &str = "pocketboot.acm";
const MENU_CMDLINE_PARAM: &str = "pocketboot.menu";
const DRM_PAGE_FLIPS_CMDLINE_PARAM: &str = "pocketboot.drm_page_flips";
const UI_ANIMATION_LAB_CMDLINE_PARAM: &str = "pocketboot.ui_animation_lab";
const MAX_DRM_PAGE_FLIPS: u32 = 64;
const AUTOBOOT_TIMEOUT: Duration = Duration::from_secs(15);
const FASTBOOT_RESPAWN_DELAY: Duration = Duration::from_secs(5);
const FASTBOOT_CONTINUE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_FASTBOOT_RESPAWNS: u32 = 3;
const FDT_MODEL_PATH: &str = "/sys/firmware/devicetree/base/model";
const FDT_COMPATIBLE_PATH: &str = "/sys/firmware/devicetree/base/compatible";
const FDT_SERIALNO_PATHS: [&str; 1] = [
    // lk2nd puts the serial-number here
    "/sys/firmware/devicetree/base/serial-number",
];
const DEFAULT_SERIALNO: &str = "0001";
const DEFAULT_DEVICE_NAME: &str = "Pocketboot Device";
const DEFAULT_DEVICE_DETAIL: &str = "LinuxBoot environment";

fn main() {
    if unsafe { libc::getpid() } != 1 {
        println!("pocketboot error: pocketboot must run as PID 1 (/init)");
        return;
    }

    // Only early setup can fail; returning lets the kernel's panic= policy act.
    let Err(err) = runtime::block_on(run());
    println!("pocketboot error: {}", err);
    thread::sleep(Duration::from_secs(1));
}

async fn run() -> Result<Infallible> {
    let started = Instant::now();
    mount_core_vfs()?;

    let cmdline = cmdline::KernelCommandLine::read(PROC_CMDLINE).unwrap_or_else(|err| {
        println!("pocketboot: failed to read kernel command line: {}", err);
        cmdline::KernelCommandLine::default()
    });

    kmsg::init_tracing(&cmdline);
    tracing::info!("starting up");
    reaper::spawn();
    getty::spawn(&cmdline);

    let boot_state = boot_state::detect();
    let boot_state_source = boot_state.source.as_ref();
    tracing::info!(
        reboot_mode = ?boot_state.reboot_mode,
        hard_reset = ?boot_state.hard_reset,
        power_key = ?boot_state.power_key,
        charger = ?boot_state.charger,
        warm_reset = ?boot_state.warm_reset,
        source_backend = boot_state_source.map(|source| source.backend).unwrap_or("none"),
        source_detail = boot_state_source.map(|source| source.detail.as_str()).unwrap_or(""),
        "detected boot state"
    );
    if matches!(
        boot_state.reboot_mode,
        Some(boot_state::RebootMode::Bootloader | boot_state::RebootMode::Recovery)
    ) {
        // The spare register is sticky and the previous stage already acted on it.
        tracing::info!(
            reboot_mode = ?boot_state.reboot_mode,
            "reboot mode does not select the boot menu"
        );
    }

    let pon_keys = boot_state::detect_pon_keys();
    let pon_key_state = sample_pon_keys(pon_keys.as_ref(), started);

    let serialno = detect_serial(&cmdline);
    tracing::info!(serialno = %serialno, "selected device serialno");
    let system_info = detect_system_info(&serialno);
    let drm_page_flips = drm_page_flips(&cmdline);
    let ui_animation_lab = cmdline.is_set(UI_ANIMATION_LAB_CMDLINE_PARAM);
    let acm = cmdline.is_set(ACM_CMDLINE_PARAM);
    let initial_menu = initial_menu_reason(
        &cmdline,
        drm_page_flips,
        ui_animation_lab,
        pon_key_state.volume_down,
    );

    let (event_tx, event_rx) = async_channel::unbounded();
    let mut coordinator = Coordinator {
        started,
        event_tx: event_tx.clone(),
        menu: None,
        entries: Vec::new(),
        bootable_entry_indices: Vec::new(),
        discovery_complete: false,
        pending_continue: None,
        continue_generation: 0,
        watcher: None,
        pon_keys,
        serialno,
        system_info,
        cmdline,
        acm,
        drm_page_flips,
        ui_animation_lab,
    };
    match initial_menu {
        Some(reason) => coordinator.enter_menu(reason),
        None => coordinator.start_autoboot(),
    }
    spawn_boot_discovery(event_tx, started);

    Ok(coordinator.run(event_rx).await)
}

fn sample_pon_keys(
    pon_keys: Option<&boot_state::PonKeys>,
    started: Instant,
) -> boot_state::PonKeyState {
    let Some(pon_keys) = pon_keys else {
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis(),
            "no PMIC PON keys found"
        );
        return boot_state::PonKeyState::default();
    };

    match pon_keys.read() {
        Ok(state) => {
            tracing::info!(
                registers = %pon_keys.registers.display(),
                rt_sts_reg = format_args!("0x{:04x}", pon_keys.rt_sts_reg),
                volume_down_key = pon_keys.volume_down,
                power_key = pon_keys.power,
                volume_down_held = state.volume_down,
                power_held = state.power,
                elapsed_ms = started.elapsed().as_millis(),
                "detected PMIC PON keys"
            );
            state
        }
        Err(err) => {
            tracing::warn!(
                registers = %pon_keys.registers.display(),
                error = %err,
                elapsed_ms = started.elapsed().as_millis(),
                "PMIC PON key read failed"
            );
            boot_state::PonKeyState::default()
        }
    }
}

fn initial_menu_reason(
    cmdline: &cmdline::KernelCommandLine,
    drm_page_flips: u32,
    ui_animation_lab: bool,
    volume_down_held: bool,
) -> Option<MenuReason> {
    if cmdline.is_set(MENU_CMDLINE_PARAM) {
        Some(MenuReason::Cmdline)
    } else if drm_page_flips > 0 || ui_animation_lab {
        // The DRM lab modes only run inside the UI and expect fastboot to stay up.
        Some(MenuReason::Lab)
    } else if volume_down_held {
        Some(MenuReason::VolumeDown)
    } else if cfg!(not(target_arch = "aarch64")) {
        // kexec loading is only implemented for aarch64.
        Some(MenuReason::AutobootUnsupported)
    } else {
        None
    }
}

enum CoordinatorEvent {
    UiAction(ui::Action),
    Fastboot(Result<Option<fastboot::PostResponseAction>>),
    DiscoveryUpdate(Vec<bootflow::BootEntry>),
    DiscoveryComplete(Vec<bootflow::BootEntry>),
    BreakIn,
    AutobootTimeout,
    RespawnFastboot,
    ContinueExpired(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MenuReason {
    Cmdline,
    Lab,
    VolumeDown,
    AutobootUnsupported,
    NoBootableEntry,
    BootFailed { entry: String, error: String },
    DiscoveryTimeout,
}

impl fmt::Display for MenuReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cmdline => "cmdline",
            Self::Lab => "lab",
            Self::VolumeDown => "volume-down",
            Self::AutobootUnsupported => "autoboot-unsupported",
            Self::NoBootableEntry => "no-bootable-entry",
            Self::BootFailed { .. } => "boot-failed",
            Self::DiscoveryTimeout => "discovery-timeout",
        })
    }
}

impl MenuReason {
    fn notice(&self) -> Option<ui::MenuNotice> {
        match self {
            Self::BootFailed { entry, error } => Some(ui::MenuNotice {
                title: "Boot failed".to_string(),
                detail: format!("{entry}: {error}"),
                error: true,
            }),
            Self::DiscoveryTimeout => Some(ui::MenuNotice {
                title: "Still scanning boot media".to_string(),
                detail: format!(
                    "Autoboot stopped waiting after {}s",
                    AUTOBOOT_TIMEOUT.as_secs()
                ),
                error: false,
            }),
            Self::Cmdline
            | Self::Lab
            | Self::VolumeDown
            | Self::AutobootUnsupported
            | Self::NoBootableEntry => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootOrigin {
    Ui,
    Fastboot,
}

struct MenuState {
    ui: Option<ui::Handle>,
    notice: Option<ui::MenuNotice>,
    gadget: gadget::Gadget,
    fastboot_running: bool,
    fastboot_errors: u32,
}

/// Owns the boot policy. `menu` is None while autobooting; once the menu is
/// entered pocketboot stays resident until a boot succeeds.
struct Coordinator {
    started: Instant,
    // Retained so the event channel can never close.
    event_tx: async_channel::Sender<CoordinatorEvent>,
    menu: Option<MenuState>,
    entries: Vec<bootflow::BootEntry>,
    bootable_entry_indices: Vec<usize>,
    discovery_complete: bool,
    // Generation of a `fastboot continue` waiting for discovery to finish.
    pending_continue: Option<u64>,
    continue_generation: u64,
    watcher: Option<breakin::Watcher>,
    pon_keys: Option<boot_state::PonKeys>,
    serialno: String,
    system_info: ui::SystemInfo,
    cmdline: cmdline::KernelCommandLine,
    acm: bool,
    drm_page_flips: u32,
    ui_animation_lab: bool,
}

impl Coordinator {
    fn start_autoboot(&mut self) {
        let break_in_tx = self.event_tx.clone();
        match breakin::Watcher::spawn(self.started, self.pon_keys.clone(), move || {
            if break_in_tx.try_send(CoordinatorEvent::BreakIn).is_err() {
                tracing::debug!("boot coordinator event channel closed");
            }
        }) {
            Ok(watcher) => self.watcher = Some(watcher),
            Err(err) => tracing::warn!(error = %err, "failed to spawn break-in watcher thread"),
        }

        let timeout_tx = self.event_tx.clone();
        runtime::detach(async move {
            async_io::Timer::after(AUTOBOOT_TIMEOUT).await;
            let _ = timeout_tx.send(CoordinatorEvent::AutobootTimeout).await;
        });
    }

    async fn run(mut self, events: async_channel::Receiver<CoordinatorEvent>) -> Infallible {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(_) => {
                    tracing::error!("boot coordinator event channel closed");
                    return futures_lite::future::pending().await;
                }
            };
            if self.menu.is_none() {
                self.autoboot_event(event).await;
            } else {
                self.menu_event(event).await;
            }
        }
    }

    async fn autoboot_event(&mut self, event: CoordinatorEvent) {
        match event {
            CoordinatorEvent::DiscoveryUpdate(entries) => {
                self.store_entries(entries);
            }
            CoordinatorEvent::DiscoveryComplete(entries) => {
                self.store_entries(entries);
                self.discovery_complete = true;
                if self
                    .watcher
                    .as_ref()
                    .is_some_and(breakin::Watcher::triggered)
                {
                    self.enter_menu(MenuReason::VolumeDown);
                } else if let Some(entry_index) = self.default_entry_index() {
                    self.attempt(entry_index).await;
                } else {
                    self.enter_menu(MenuReason::NoBootableEntry);
                }
            }
            CoordinatorEvent::BreakIn => self.enter_menu(MenuReason::VolumeDown),
            CoordinatorEvent::AutobootTimeout => {
                if self.discovery_complete {
                    tracing::debug!("ignoring autoboot timeout after discovery completed");
                } else {
                    self.enter_menu(MenuReason::DiscoveryTimeout);
                }
            }
            CoordinatorEvent::UiAction(_)
            | CoordinatorEvent::Fastboot(_)
            | CoordinatorEvent::RespawnFastboot
            | CoordinatorEvent::ContinueExpired(_) => {
                tracing::debug!("ignoring boot menu event during autoboot");
            }
        }
    }

    // A break-in during the load is honoured by the final check once the load
    // returns. A load that hangs can only be escaped through the serial getty.
    async fn attempt(&mut self, entry_index: usize) {
        let Some(entry) = self.entries.get(entry_index).cloned() else {
            return;
        };
        tracing::info!(
            decision = "autoboot",
            id = %entry.id,
            source = %entry.source.display(),
            elapsed_ms = self.elapsed_ms(),
            "POCKETBOOT_BOOT_DECISION"
        );

        let load_entry = entry.clone();
        if let Err(err) = runtime::unblock(move || load_entry.load()).await {
            // A failed kexec_load installs nothing, so there is nothing to unload.
            self.enter_menu(MenuReason::BootFailed {
                entry: boot_entry_title(&entry),
                error: err.to_string(),
            });
            return;
        }
        tracing::info!(
            id = %entry.id,
            elapsed_ms = self.elapsed_ms(),
            "autoboot entry loaded"
        );

        let (watcher_held, open_inputs) = self
            .watcher
            .take()
            .map(breakin::Watcher::finish)
            .unwrap_or_default();
        let held = watcher_held
            || self
                .pon_keys
                .as_ref()
                .is_some_and(boot_state::PonKeys::volume_down_pressed);
        if held {
            unload_kexec_image();
            self.enter_menu(MenuReason::VolumeDown);
            return;
        }

        let error = kexec_error(kexec::exec_loaded_image());
        drop(open_inputs);
        unload_kexec_image();
        self.enter_menu(MenuReason::BootFailed {
            entry: boot_entry_title(&entry),
            error,
        });
    }

    fn enter_menu(&mut self, reason: MenuReason) {
        if self.menu.is_some() {
            tracing::debug!(reason = %reason, "boot menu is already active");
            return;
        }
        if let Some(watcher) = self.watcher.take() {
            watcher.finish();
        }
        log_menu_decision(&reason, self.elapsed_ms());

        let battery = match battery::spawn() {
            Ok(updates) => Some(updates),
            Err(err) => {
                tracing::warn!(error = %err, "failed to spawn battery watcher thread");
                None
            }
        };
        let notice = reason.notice();
        let ui = match ui::spawn(
            battery,
            self.system_info.clone(),
            self.drm_page_flips,
            self.ui_animation_lab,
            self.pon_keys.clone(),
        ) {
            Ok(ui) => {
                spawn_ui_action_forwarder(&ui, self.event_tx.clone());
                // The command channel is drained before the first draw, so the
                // first frame already shows these.
                ui.update_boot_entries(boot_menu_entries(&self.entries).1, self.discovery_complete);
                ui.set_notice(notice.clone());
                Some(ui)
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to spawn UI thread");
                None
            }
        };

        self.menu = Some(MenuState {
            ui,
            notice,
            gadget: gadget::Gadget::new(self.serialno.clone()),
            fastboot_running: false,
            fastboot_errors: 0,
        });
        self.spawn_fastboot();
        #[cfg(feature = "qemu")]
        if let Err(err) = qemu::spawn() {
            tracing::warn!(error = ?err, "failed to spawn QEMU USB/IP service");
        }
        if self.acm {
            kmsg_forwarder::spawn();
        } else {
            tracing::info!(param = ACM_CMDLINE_PARAM, "CDC-ACM disabled");
        }
        tracing::info!("waiting for UI boot selection or fastboot exit");
    }

    async fn menu_event(&mut self, event: CoordinatorEvent) {
        match event {
            CoordinatorEvent::DiscoveryUpdate(entries) => {
                let menu_entries = self.store_entries(entries);
                if let Some(ui) = self.ui() {
                    ui.update_boot_entries(menu_entries, false);
                }
            }
            CoordinatorEvent::DiscoveryComplete(entries) => {
                let menu_entries = self.store_entries(entries);
                self.discovery_complete = true;
                if let Some(ui) = self.ui() {
                    ui.update_boot_entries(menu_entries, true);
                }
                if self.menu.as_ref().map(|menu| &menu.notice)
                    == Some(&MenuReason::DiscoveryTimeout.notice())
                {
                    self.show_notice(None);
                }
                if self.pending_continue.take().is_some() {
                    match self.default_entry_index() {
                        Some(entry_index) => {
                            self.menu_boot(entry_index, BootOrigin::Fastboot).await;
                        }
                        None => {
                            tracing::warn!(
                                "fastboot continue: no directly bootable entry was discovered"
                            );
                            self.spawn_fastboot();
                        }
                    }
                }
            }
            CoordinatorEvent::BreakIn | CoordinatorEvent::AutobootTimeout => {
                tracing::debug!("ignoring autoboot event in the boot menu");
            }
            CoordinatorEvent::UiAction(ui::Action::BootEntry(menu_index)) => {
                // A UI selection supersedes a `fastboot continue` still waiting
                // for discovery; fastboot comes back if this boot fails.
                let superseded_continue = self.pending_continue.take().is_some();
                match self.bootable_entry_indices.get(menu_index).copied() {
                    Some(entry_index) => self.menu_boot(entry_index, BootOrigin::Ui).await,
                    None => {
                        tracing::warn!(menu_index, "UI requested unknown boot entry");
                        if let Some(ui) = self.ui() {
                            ui.cancel_booting();
                        }
                    }
                }
                if superseded_continue {
                    self.spawn_fastboot();
                }
            }
            CoordinatorEvent::Fastboot(Err(err)) => {
                self.fastboot_exited(false);
                self.schedule_fastboot_respawn(err);
            }
            CoordinatorEvent::Fastboot(Ok(Some(action))) => {
                self.fastboot_exited(true);
                tracing::info!("running fastboot post-response action");
                // Every action leaves pocketboot when it succeeds.
                let error = match action() {
                    Ok(()) => "returned unexpectedly".to_string(),
                    Err(err) => err.to_string(),
                };
                tracing::warn!(error = %error, "fastboot post-response action failed");
                self.show_notice(Some(ui::MenuNotice {
                    title: "Fastboot action failed".to_string(),
                    detail: error,
                    error: true,
                }));
                self.spawn_fastboot();
            }
            CoordinatorEvent::Fastboot(Ok(None)) => {
                self.fastboot_exited(true);
                if !self.discovery_complete {
                    // Fastboot stays down while the continue is pending, so a
                    // later session can't be cut off by the deferred kexec.
                    tracing::info!(
                        "fastboot continue before boot discovery completed; booting the default entry once it does"
                    );
                    self.continue_generation += 1;
                    let generation = self.continue_generation;
                    self.pending_continue = Some(generation);
                    let event_tx = self.event_tx.clone();
                    runtime::detach(async move {
                        async_io::Timer::after(FASTBOOT_CONTINUE_TIMEOUT).await;
                        let _ = event_tx
                            .send(CoordinatorEvent::ContinueExpired(generation))
                            .await;
                    });
                } else if let Some(entry_index) = self.default_entry_index() {
                    self.menu_boot(entry_index, BootOrigin::Fastboot).await;
                } else {
                    tracing::warn!("fastboot continue: no directly bootable entry was discovered");
                    self.spawn_fastboot();
                }
            }
            CoordinatorEvent::RespawnFastboot => {
                if self.pending_continue.is_none() {
                    self.spawn_fastboot();
                }
            }
            CoordinatorEvent::ContinueExpired(generation) => {
                if self.pending_continue == Some(generation) {
                    self.pending_continue = None;
                    tracing::warn!(
                        "fastboot continue: boot discovery is still running; giving up and restarting fastboot"
                    );
                    self.spawn_fastboot();
                }
            }
        }
    }

    async fn menu_boot(&mut self, entry_index: usize, origin: BootOrigin) {
        let Some(entry) = self.entries.get(entry_index).cloned() else {
            return;
        };
        match origin {
            BootOrigin::Ui => tracing::info!(
                id = %entry.id,
                source = %entry.source.display(),
                "booting UI-selected entry"
            ),
            BootOrigin::Fastboot => tracing::info!(
                id = %entry.id,
                source = %entry.source.display(),
                "booting discovered entry"
            ),
        }

        let load_entry = entry.clone();
        let error = match runtime::unblock(move || load_entry.load()).await {
            Ok(()) => {
                let error = kexec_error(kexec::exec_loaded_image());
                unload_kexec_image();
                error
            }
            Err(err) => err.to_string(),
        };
        tracing::warn!(
            id = %entry.id,
            origin = ?origin,
            error = %error,
            "boot failed; staying in the boot menu"
        );

        self.show_notice(
            MenuReason::BootFailed {
                entry: boot_entry_title(&entry),
                error,
            }
            .notice(),
        );
        if origin == BootOrigin::Ui
            && let Some(ui) = self.ui()
        {
            ui.cancel_booting();
        }
        if origin == BootOrigin::Fastboot {
            self.spawn_fastboot();
        }
    }

    fn spawn_fastboot(&mut self) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        if menu.fastboot_running {
            tracing::debug!("fastboot gadget thread is already running");
            return;
        }

        let commands = fastboot_commands(
            menu.gadget.clone(),
            self.serialno.clone(),
            self.cmdline.clone(),
        );
        match menu.gadget.spawn(gadget::Mode::Fastboot {
            commands,
            acm: self.acm,
        }) {
            Ok(thread) => {
                menu.fastboot_running = true;
                spawn_fastboot_joiner(thread, self.event_tx.clone());
            }
            Err(err) => tracing::warn!(error = %err, "failed to spawn fastboot gadget thread"),
        }
    }

    fn fastboot_exited(&mut self, ok: bool) {
        if let Some(menu) = self.menu.as_mut() {
            menu.fastboot_running = false;
            if ok {
                menu.fastboot_errors = 0;
            }
        }
    }

    fn schedule_fastboot_respawn(&mut self, err: String) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        if menu.fastboot_errors >= MAX_FASTBOOT_RESPAWNS {
            tracing::warn!(error = %err, "fastboot gadget failed; not respawning it again");
            return;
        }

        menu.fastboot_errors += 1;
        tracing::warn!(
            error = %err,
            attempt = menu.fastboot_errors,
            delay_ms = FASTBOOT_RESPAWN_DELAY.as_millis(),
            "fastboot gadget failed; respawning"
        );
        let event_tx = self.event_tx.clone();
        runtime::detach(async move {
            async_io::Timer::after(FASTBOOT_RESPAWN_DELAY).await;
            let _ = event_tx.send(CoordinatorEvent::RespawnFastboot).await;
        });
    }

    fn store_entries(&mut self, entries: Vec<bootflow::BootEntry>) -> Vec<ui::BootMenuEntryInfo> {
        let (indices, menu_entries) = boot_menu_entries(&entries);
        self.entries = entries;
        self.bootable_entry_indices = indices;
        menu_entries
    }

    fn default_entry_index(&self) -> Option<usize> {
        self.bootable_entry_indices.first().copied()
    }

    fn ui(&self) -> Option<&ui::Handle> {
        self.menu.as_ref()?.ui.as_ref()
    }

    fn show_notice(&mut self, notice: Option<ui::MenuNotice>) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        if let Some(ui) = &menu.ui {
            ui.set_notice(notice.clone());
        }
        menu.notice = notice;
    }

    fn elapsed_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }
}

fn log_menu_decision(reason: &MenuReason, elapsed_ms: u128) {
    match reason {
        MenuReason::BootFailed { entry, error } => tracing::warn!(
            decision = "menu",
            reason = %reason,
            entry = ?entry,
            error = ?error,
            elapsed_ms,
            "POCKETBOOT_BOOT_DECISION"
        ),
        MenuReason::NoBootableEntry | MenuReason::DiscoveryTimeout => tracing::warn!(
            decision = "menu",
            reason = %reason,
            elapsed_ms,
            "POCKETBOOT_BOOT_DECISION"
        ),
        MenuReason::Cmdline
        | MenuReason::Lab
        | MenuReason::VolumeDown
        | MenuReason::AutobootUnsupported => tracing::info!(
            decision = "menu",
            reason = %reason,
            elapsed_ms,
            "POCKETBOOT_BOOT_DECISION"
        ),
    }
}

fn kexec_error(result: io::Result<()>) -> String {
    match result {
        Ok(()) => "kexec returned unexpectedly".to_string(),
        Err(err) => format!("kexec: {err}"),
    }
}

// Only called after this process loaded the image itself: unloading drops
// whatever is installed, including an image fastboot loaded on purpose.
fn unload_kexec_image() {
    if let Err(err) = kexec::unload() {
        tracing::warn!(error = %err, "failed to unload kexec image");
    }
}

fn spawn_ui_action_forwarder(ui: &ui::Handle, event_tx: async_channel::Sender<CoordinatorEvent>) {
    let actions = ui.action_receiver();
    runtime::detach(async move {
        while let Ok(action) = actions.recv().await {
            if event_tx
                .send(CoordinatorEvent::UiAction(action))
                .await
                .is_err()
            {
                break;
            }
        }
        tracing::debug!("UI action forwarder stopped");
    });
}

fn spawn_fastboot_joiner(
    fastboot_thread: thread::JoinHandle<gadget::ThreadResult>,
    event_tx: async_channel::Sender<CoordinatorEvent>,
) {
    runtime::detach(async move {
        let result = runtime::unblock(move || join_fastboot_thread(fastboot_thread)).await;
        let _ = event_tx.send(CoordinatorEvent::Fastboot(result)).await;
    });
}

fn spawn_boot_discovery(event_tx: async_channel::Sender<CoordinatorEvent>, started: Instant) {
    runtime::detach(async move {
        let settled =
            runtime::unblock(|| settle::wait_for_local_flash(Duration::from_secs(5))).await;
        log_settle_report(&settled, started);

        // This walk is only for logging; skip it at the default log level.
        if tracing::enabled!(tracing::Level::INFO) {
            match runtime::unblock(block_devices).await {
                Ok(devices) => log_block_devices(devices),
                Err(err) => tracing::warn!(error = %err, "block device listing failed"),
            }
        }

        let progress_tx = event_tx.clone();
        let result = bootflow::discover(move |entries| {
            let progress_tx = progress_tx.clone();
            async move {
                let _ = progress_tx
                    .send(CoordinatorEvent::DiscoveryUpdate(entries))
                    .await;
            }
        })
        .await;

        let entries = match result {
            Ok(entries) => {
                log_boot_entries(&entries);
                entries
            }
            Err(err) => {
                tracing::warn!(error = ?err, "bootflow discovery failed");
                Vec::new()
            }
        };
        tracing::info!(
            count = entries.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "boot discovery complete"
        );
        let _ = event_tx
            .send(CoordinatorEvent::DiscoveryComplete(entries))
            .await;
    });
}

fn log_settle_report(settled: &settle::Report, started: Instant) {
    let since_start_ms = started.elapsed().as_millis();
    if settled.timed_out {
        tracing::warn!(
            elapsed_ms = settled.elapsed.as_millis(),
            since_start_ms,
            disks = settled.disks,
            partitions = settled.partitions,
            events = settled.events,
            snapshot_changes = settled.snapshot_changes,
            snapshot = %settled.summary,
            "local flash settle timed out"
        );
    } else {
        tracing::info!(
            elapsed_ms = settled.elapsed.as_millis(),
            since_start_ms,
            disks = settled.disks,
            partitions = settled.partitions,
            events = settled.events,
            snapshot_changes = settled.snapshot_changes,
            snapshot = %settled.summary,
            "local flash settled"
        );
    }
}

fn log_block_devices(devices: Vec<BlockDevice>) {
    if devices.is_empty() {
        tracing::warn!("no block devices found");
    } else {
        tracing::info!(count = devices.len(), "block devices found");
        for device in devices {
            tracing::info!(device = %device.name, description = %device.describe(), "block device");
            for partition in device.partitions {
                tracing::info!(partition = %partition.name, description = %partition.describe(), "block partition");
            }
        }
    }
}

fn log_boot_entries(entries: &[bootflow::BootEntry]) {
    if entries.is_empty() {
        tracing::warn!("no boot entries discovered");
    } else {
        tracing::info!(count = entries.len(), "boot entries discovered");
        for (index, entry) in entries.iter().enumerate() {
            tracing::info!(
                index,
                id = %entry.id,
                title = entry.title.as_deref().unwrap_or(""),
                version = entry.version.as_deref().unwrap_or(""),
                architecture = entry.architecture.as_deref().unwrap_or(""),
                role = ?entry.role,
                disk = %entry.disk,
                partition = %entry.partition,
                source = %entry.source.display(),
                preferred = entry.preferred,
                directly_bootable = entry.is_directly_bootable(),
                "boot entry"
            );
        }
    }
}

fn boot_menu_entries(
    boot_entries: &[bootflow::BootEntry],
) -> (Vec<usize>, Vec<ui::BootMenuEntryInfo>) {
    let mut indices = Vec::new();
    let mut entries = Vec::new();

    for (index, entry) in boot_entries.iter().enumerate() {
        if !entry.is_directly_bootable() {
            continue;
        }

        indices.push(index);
        entries.push(ui::BootMenuEntryInfo {
            title: boot_entry_title(entry),
            subtitle: boot_entry_subtitle(entry),
            detail: boot_entry_detail(entry),
            badge: boot_entry_badge(entry),
        });
    }

    (indices, entries)
}

fn boot_entry_title(entry: &bootflow::BootEntry) -> String {
    entry
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&entry.id)
        .to_string()
}

fn boot_entry_subtitle(entry: &bootflow::BootEntry) -> String {
    let mut parts = Vec::new();
    if let Some(version) = entry
        .version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        parts.push(version);
    }
    if let Some(architecture) = entry
        .architecture
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        parts.push(architecture);
    }

    if parts.is_empty() {
        "Ready to boot".to_string()
    } else {
        parts.join(" - ")
    }
}

fn boot_entry_detail(entry: &bootflow::BootEntry) -> String {
    let source = entry
        .source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| entry.source.to_str().unwrap_or("boot entry"));
    format!("{}:{} - {source}", entry.disk, entry.partition)
}

fn boot_entry_badge(entry: &bootflow::BootEntry) -> String {
    let role = match entry.role {
        bootflow::BootPartitionRole::Xbootldr => "xbootldr",
        bootflow::BootPartitionRole::Esp => "esp",
        bootflow::BootPartitionRole::Nested => "nested",
    };
    if entry.preferred {
        format!("default {role}")
    } else {
        role.to_string()
    }
}

fn fastboot_commands(
    gadget: gadget::Gadget,
    serialno: String,
    cmdline: cmdline::KernelCommandLine,
) -> fastboot::CommandMap {
    let slots = ab_slots::Slots::new(cmdline);
    let mut commands = fastboot::commands::boot_commands();
    commands.extend(fastboot::commands::getvar_commands(serialno, slots.clone()));
    commands.extend(fastboot::commands::flash_commands());
    commands.extend(fastboot::commands::slot_commands(slots));
    commands.extend(fastboot::commands::diagnostic_commands());
    commands.extend(fastboot::commands::ums_commands(gadget));
    commands.extend(fastboot::commands::reboot_commands());
    commands
}

fn detect_system_info(serialno: &str) -> ui::SystemInfo {
    ui::SystemInfo {
        device_name: fdt_first_string(FDT_MODEL_PATH)
            .unwrap_or_else(|| DEFAULT_DEVICE_NAME.to_string()),
        device_detail: fdt_compatible_detail().unwrap_or_else(|| DEFAULT_DEVICE_DETAIL.to_string()),
        serialno: serialno.to_string(),
    }
}

fn fdt_first_string(path: impl AsRef<Path>) -> Option<String> {
    read_fdt_strings(path)?.into_iter().next()
}

fn fdt_compatible_detail() -> Option<String> {
    let compatibles = read_fdt_strings(FDT_COMPATIBLE_PATH)?;
    match compatibles.as_slice() {
        [] => None,
        [only] => Some(only.clone()),
        [first, second, ..] => Some(format!("{first} / {second}")),
    }
}

fn read_fdt_strings(path: impl AsRef<Path>) -> Option<Vec<String>> {
    let bytes = fs::read(path).ok()?;
    let values = bytes
        .split(|byte| *byte == b'\0')
        .filter_map(parse_fdt_string)
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!values.is_empty()).then_some(values)
}

fn detect_serial(cmdline: &cmdline::KernelCommandLine) -> String {
    fdt_serialno()
        .or_else(|| cmdline_serialno(cmdline))
        .unwrap_or_else(|| DEFAULT_SERIALNO.to_string())
}

fn drm_page_flips(cmdline: &cmdline::KernelCommandLine) -> u32 {
    let Some(value) = cmdline.value(DRM_PAGE_FLIPS_CMDLINE_PARAM) else {
        return 0;
    };
    match value.parse::<u32>() {
        Ok(value) if value <= MAX_DRM_PAGE_FLIPS => value,
        _ => {
            tracing::warn!(
                param = DRM_PAGE_FLIPS_CMDLINE_PARAM,
                value,
                maximum = MAX_DRM_PAGE_FLIPS,
                "ignoring invalid bounded DRM page-flip request"
            );
            0
        }
    }
}

fn fdt_serialno() -> Option<String> {
    FDT_SERIALNO_PATHS.iter().find_map(|path| {
        let bytes = fs::read(path).ok()?;
        parse_fdt_serialno(&bytes).map(str::to_string)
    })
}

fn cmdline_serialno(cmdline: &cmdline::KernelCommandLine) -> Option<String> {
    cmdline.value("androidboot.serialno").map(str::to_string)
}

fn parse_fdt_serialno(bytes: &[u8]) -> Option<&str> {
    let serialno = bytes.split(|byte| *byte == b'\0').next()?;
    parse_fdt_string(serialno)
}

fn parse_fdt_string(bytes: &[u8]) -> Option<&str> {
    let serialno = trim_ascii_bytes(bytes);
    (!serialno.is_empty())
        .then(|| std::str::from_utf8(serialno).ok())
        .flatten()
}

fn trim_ascii_bytes(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn join_fastboot_thread(
    handle: thread::JoinHandle<gadget::ThreadResult>,
) -> Result<Option<fastboot::PostResponseAction>> {
    match handle.join() {
        Ok(Ok(action)) => {
            tracing::info!("fastboot thread exited");
            Ok(action)
        }
        Ok(Err(err)) => Err(format!("fastboot thread failed: {err}")),
        Err(_) => Err("fastboot thread panicked".to_string()),
    }
}

fn mount_core_vfs() -> Result<()> {
    for dir in ["/proc", "/sys", "/dev", "/run"] {
        fs::create_dir_all(dir).map_err(|err| format!("create {dir}: {err}"))?;
    }

    mount_fs(Some("proc"), "/proc", Some("proc"), 0, None)?;
    mount_fs(Some("sysfs"), "/sys", Some("sysfs"), 0, None)?;
    mount_fs(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        0,
        Some("mode=0755"),
    )?;
    fs::create_dir_all("/dev/pts").map_err(|err| format!("create /dev/pts: {err}"))?;
    mount_fs(Some("devpts"), "/dev/pts", Some("devpts"), 0, None)?;
    mount_fs(Some("tmpfs"), "/run", Some("tmpfs"), 0, Some("mode=0755"))?;
    Ok(())
}

fn mount_fs(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    let source = source.map(cstring).transpose()?;
    let target_c = cstring(target)?;
    let fstype = fstype.map(cstring).transpose()?;
    let data = data.map(cstring).transpose()?;
    let data_ptr = data
        .as_ref()
        .map(|s| s.as_ptr() as *const libc::c_void)
        .unwrap_or(std::ptr::null());

    let rc = unsafe {
        libc::mount(
            source
                .as_ref()
                .map(|s| s.as_ptr())
                .unwrap_or(std::ptr::null()),
            target_c.as_ptr(),
            fstype
                .as_ref()
                .map(|s| s.as_ptr())
                .unwrap_or(std::ptr::null()),
            flags,
            data_ptr,
        )
    };

    if rc == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EBUSY) {
        return Ok(());
    }

    Err(format!(
        "mount {} on {target} as {}: {err}",
        source
            .as_ref()
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|| "none".into()),
        fstype
            .as_ref()
            .map(|s| s.to_string_lossy())
            .unwrap_or_else(|| "none".into())
    ))
}

fn cstring(value: &str) -> Result<CString> {
    CString::new(value).map_err(|_| format!("string contains NUL byte: {value:?}"))
}

#[derive(Debug)]
struct BlockDevice {
    name: String,
    path: PathBuf,
    partitions: Vec<BlockDevice>,
}

impl BlockDevice {
    fn describe(&self) -> String {
        let dev = read_trimmed(self.path.join("dev")).unwrap_or_else(|| "?:?".to_string());
        let size = read_trimmed(self.path.join("size"))
            .and_then(|value| value.parse::<u64>().ok())
            .map(format_size)
            .unwrap_or_else(|| "size=?".to_string());
        let access = match read_trimmed(self.path.join("ro")).as_deref() {
            Some("1") => "ro",
            Some("0") => "rw",
            _ => "ro=?",
        };
        let removable = match read_trimmed(self.path.join("removable")).as_deref() {
            Some("1") => " removable",
            _ => "",
        };
        let partname = uevent_value(self.path.join("uevent"), "PARTNAME")
            .map(|value| format!(" partname={value}"))
            .unwrap_or_default();

        format!(
            "{} dev={} {} {}{}{}",
            self.name, dev, size, access, removable, partname
        )
    }
}

fn block_devices() -> Result<Vec<BlockDevice>> {
    let mut devices = Vec::new();
    let entries = fs::read_dir(SYS_BLOCK).map_err(|err| format!("read {SYS_BLOCK}: {err}"))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("read {SYS_BLOCK} entry: {err}"))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        devices.push(BlockDevice {
            partitions: partitions_for(&path)?,
            name,
            path,
        });
    }

    devices.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(devices)
}

fn partitions_for(device_path: &Path) -> Result<Vec<BlockDevice>> {
    let mut partitions = Vec::new();
    let entries = fs::read_dir(device_path)
        .map_err(|err| format!("read partitions under {}: {err}", device_path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "read partition entry under {}: {err}",
                device_path.display()
            )
        })?;
        let path = entry.path();
        if !path.join("partition").exists() {
            continue;
        }
        partitions.push(BlockDevice {
            name: entry.file_name().to_string_lossy().into_owned(),
            path,
            partitions: Vec::new(),
        });
    }

    partitions.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(partitions)
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn uevent_value(path: impl AsRef<Path>, key: &str) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    contents.lines().find_map(|line| {
        let (line_key, value) = line.split_once('=')?;
        (line_key == key).then(|| value.to_string())
    })
}

fn format_size(sectors: u64) -> String {
    let bytes = sectors as u128 * 512;
    let mib = bytes / 1024 / 1024;
    format!("size={sectors} sectors/{mib} MiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_androidboot_serialno() {
        let cmdline = cmdline::KernelCommandLine::parse("foo androidboot.serialno=6ea45af6 bar");

        assert_eq!(cmdline_serialno(&cmdline).as_deref(), Some("6ea45af6"));
    }

    #[test]
    fn ignores_empty_androidboot_serialno() {
        let cmdline = cmdline::KernelCommandLine::parse("foo androidboot.serialno= bar");

        assert_eq!(cmdline_serialno(&cmdline), None);
    }

    #[test]
    fn parses_fdt_serialno() {
        assert_eq!(parse_fdt_serialno(b"6ea45af6\0"), Some("6ea45af6"));
    }

    #[test]
    fn trims_fdt_serialno() {
        assert_eq!(parse_fdt_serialno(b"  6ea45af6\n\0"), Some("6ea45af6"));
    }

    #[test]
    fn ignores_empty_fdt_serialno() {
        assert_eq!(parse_fdt_serialno(b"\0"), None);
        assert_eq!(parse_fdt_serialno(b" \n\0"), None);
    }

    #[test]
    fn parses_bounded_drm_page_flip_lab_count() {
        for (value, expected) in [
            ("", 0),
            ("pocketboot.drm_page_flips=0", 0),
            ("pocketboot.drm_page_flips=16", 16),
            ("pocketboot.drm_page_flips=64", 64),
            ("pocketboot.drm_page_flips=65", 0),
            ("pocketboot.drm_page_flips=invalid", 0),
        ] {
            let cmdline = cmdline::KernelCommandLine::parse(value);
            assert_eq!(drm_page_flips(&cmdline), expected, "cmdline: {value}");
        }
    }

    #[test]
    fn initial_menu_reason_prefers_explicit_requests() {
        let menu = cmdline::KernelCommandLine::parse("quiet pocketboot.menu");
        let plain = cmdline::KernelCommandLine::parse("quiet");

        assert_eq!(
            initial_menu_reason(&menu, 16, true, true),
            Some(MenuReason::Cmdline)
        );
        assert_eq!(
            initial_menu_reason(&plain, 16, false, true),
            Some(MenuReason::Lab)
        );
        assert_eq!(
            initial_menu_reason(&plain, 0, true, false),
            Some(MenuReason::Lab)
        );
        assert_eq!(
            initial_menu_reason(&plain, 0, false, true),
            Some(MenuReason::VolumeDown)
        );
    }

    #[test]
    fn initial_menu_reason_autoboots_only_where_kexec_works() {
        let plain = cmdline::KernelCommandLine::parse("quiet pocketboot.menu=1");

        assert_eq!(
            initial_menu_reason(&plain, 0, false, false),
            cfg!(not(target_arch = "aarch64")).then_some(MenuReason::AutobootUnsupported)
        );
    }

    #[test]
    fn menu_reasons_use_kebab_case_markers() {
        let failed = MenuReason::BootFailed {
            entry: "Debian".to_string(),
            error: "boom".to_string(),
        };
        let markers = [
            (MenuReason::Cmdline, "cmdline"),
            (MenuReason::Lab, "lab"),
            (MenuReason::VolumeDown, "volume-down"),
            (MenuReason::AutobootUnsupported, "autoboot-unsupported"),
            (MenuReason::NoBootableEntry, "no-bootable-entry"),
            (failed, "boot-failed"),
            (MenuReason::DiscoveryTimeout, "discovery-timeout"),
        ];

        for (reason, marker) in markers {
            assert_eq!(reason.to_string(), marker);
        }
    }

    #[test]
    fn only_fallback_menus_show_a_notice() {
        for reason in [
            MenuReason::Cmdline,
            MenuReason::Lab,
            MenuReason::VolumeDown,
            MenuReason::AutobootUnsupported,
            MenuReason::NoBootableEntry,
        ] {
            assert_eq!(reason.notice(), None, "reason: {reason}");
        }

        assert_eq!(
            MenuReason::BootFailed {
                entry: "Debian".to_string(),
                error: "kernel is not a raw arm64 Image".to_string(),
            }
            .notice(),
            Some(ui::MenuNotice {
                title: "Boot failed".to_string(),
                detail: "Debian: kernel is not a raw arm64 Image".to_string(),
                error: true,
            })
        );
        assert_eq!(
            MenuReason::DiscoveryTimeout.notice(),
            Some(ui::MenuNotice {
                title: "Still scanning boot media".to_string(),
                detail: "Autoboot stopped waiting after 15s".to_string(),
                error: false,
            })
        );
    }

    #[test]
    fn describes_kexec_failures() {
        assert_eq!(kexec_error(Ok(())), "kexec returned unexpectedly");
        assert_eq!(
            kexec_error(Err(io::Error::from_raw_os_error(libc::EBUSY))),
            format!("kexec: {}", io::Error::from_raw_os_error(libc::EBUSY))
        );
    }
}
