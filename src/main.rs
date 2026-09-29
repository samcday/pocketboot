use std::{
    ffi::CString,
    fs, io,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

mod ab_slots;
mod adb;
mod battery;
mod boot_state;
mod bootflow;
mod cmdline;
mod fastboot;
mod gadget;
mod getty;
mod kexec;
mod kmsg;
#[path = "kmsg-forwarder.rs"]
mod kmsg_forwarder;
mod pe;
mod power;
mod pstore;
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
const DRM_PAGE_FLIPS_CMDLINE_PARAM: &str = "pocketboot.drm_page_flips";
const UI_ANIMATION_LAB_CMDLINE_PARAM: &str = "pocketboot.ui_animation_lab";
const MAX_DRM_PAGE_FLIPS: u32 = 64;
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
    if let Err(err) = runtime::block_on(run()) {
        // PID 1 exiting is an unrecoverable kernel panic. Keep the reason in the
        // kernel log so a UART-free capture (ramoops/pstore) still has it.
        kmsg::emergency_log(format_args!("unrecoverable error: {err}"));
        println!("pocketboot error: {}", err);
        thread::sleep(Duration::from_secs(1));
    }
}

async fn run() -> Result<()> {
    if unsafe { libc::getpid() } != 1 {
        return Err("pocketboot must run as PID 1 (/init)".to_string());
    }

    mount_core_vfs()?;

    let cmdline = cmdline::KernelCommandLine::read(PROC_CMDLINE).unwrap_or_else(|err| {
        println!("pocketboot: failed to read kernel command line: {}", err);
        cmdline::KernelCommandLine::default()
    });

    kmsg::init_tracing(&cmdline);
    tracing::info!("starting up");
    pstore::capture();
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

    let serialno = detect_serial(&cmdline);
    tracing::info!(serialno = %serialno, "selected device serialno");
    let system_info = detect_system_info(&serialno);
    let battery = match battery::spawn() {
        Ok(updates) => Some(updates),
        Err(err) => {
            tracing::warn!(error = %err, "failed to spawn battery watcher thread");
            None
        }
    };
    let drm_page_flips = drm_page_flips(&cmdline);
    let ui_animation_lab = cmdline.is_set(UI_ANIMATION_LAB_CMDLINE_PARAM);
    let ui = match ui::spawn(battery, system_info, drm_page_flips, ui_animation_lab) {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::warn!(error = %err, "failed to spawn UI thread");
            None
        }
    };
    let gadget = gadget::Gadget::new(serialno.clone());
    let acm = cmdline.is_set(ACM_CMDLINE_PARAM);
    #[cfg(feature = "qemu")]
    if let Err(err) = qemu::spawn() {
        tracing::warn!(error = ?err, "failed to spawn QEMU USB/IP service");
    }
    if acm {
        kmsg_forwarder::spawn();
    } else {
        tracing::info!(param = ACM_CMDLINE_PARAM, "CDC-ACM disabled");
    }

    let (event_tx, event_rx) = async_channel::unbounded();
    if let Some(ui) = &ui {
        spawn_ui_action_forwarder(ui, event_tx.clone());
    }
    spawn_fastboot_service(
        gadget.clone(),
        serialno.clone(),
        cmdline.clone(),
        acm,
        event_tx.clone(),
        Duration::ZERO,
    );
    spawn_boot_discovery(event_tx.clone());

    // Retain a sender even when no UI or gadget is running. Losing recovery
    // interfaces must not close the coordinator channel and terminate PID 1.
    run_boot_coordinator(ui.as_ref(), event_rx, || {
        spawn_fastboot_service(
            gadget.clone(),
            serialno.clone(),
            cmdline.clone(),
            acm,
            event_tx.clone(),
            Duration::from_secs(1),
        );
    })
    .await?;
    Ok(())
}

enum CoordinatorEvent {
    UiAction(ui::Action),
    Fastboot(Result<Option<fastboot::PostResponseAction>>),
    DiscoveryUpdate(Vec<bootflow::BootEntry>),
    DiscoveryComplete(Vec<bootflow::BootEntry>),
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

fn spawn_fastboot_service(
    gadget: gadget::Gadget,
    serialno: String,
    cmdline: cmdline::KernelCommandLine,
    acm: bool,
    event_tx: async_channel::Sender<CoordinatorEvent>,
    delay: Duration,
) {
    runtime::detach(async move {
        // Back off on persistent UDC/thread-creation failures without blocking
        // the coordinator (the UI must remain usable during recovery).
        if !delay.is_zero() {
            async_io::Timer::after(delay).await;
        }
        let result = match gadget.spawn(gadget::Mode::Fastboot {
            commands: fastboot_commands(gadget.clone(), serialno, cmdline),
            acm,
        }) {
            Ok(thread) => runtime::unblock(move || join_fastboot_thread(thread)).await,
            Err(err) => Err(format!("spawn fastboot gadget thread: {err}")),
        };
        let _ = event_tx.send(CoordinatorEvent::Fastboot(result)).await;
    });
}

fn spawn_boot_discovery(event_tx: async_channel::Sender<CoordinatorEvent>) {
    runtime::detach(async move {
        let settled =
            runtime::unblock(|| settle::wait_for_local_flash(Duration::from_secs(5))).await;
        log_settle_report(&settled);

        match runtime::unblock(block_devices).await {
            Ok(devices) => log_block_devices(devices),
            Err(err) => tracing::warn!(error = %err, "block device listing failed"),
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
        let _ = event_tx
            .send(CoordinatorEvent::DiscoveryComplete(entries))
            .await;
    });
}

async fn run_boot_coordinator(
    ui: Option<&ui::Handle>,
    events: async_channel::Receiver<CoordinatorEvent>,
    restart_fastboot: impl FnMut(),
) -> Result<()> {
    run_boot_coordinator_with(
        ui,
        events,
        boot_discovered_entry,
        |err| report_boot_failure(ui, err),
        restart_fastboot,
    )
    .await
}

async fn run_boot_coordinator_with(
    ui: Option<&ui::Handle>,
    events: async_channel::Receiver<CoordinatorEvent>,
    mut boot: impl FnMut(&bootflow::BootEntry) -> Result<()>,
    mut failed: impl FnMut(String),
    mut restart_fastboot: impl FnMut(),
) -> Result<()> {
    let mut boot_entries: Vec<bootflow::BootEntry> = Vec::new();
    let mut bootable_entry_indices: Vec<usize> = Vec::new();
    let mut discovery_complete = false;
    let mut fastboot_requested_default = false;
    tracing::info!("waiting for UI boot selection or fastboot exit");

    loop {
        let event = events
            .recv()
            .await
            .map_err(|_| "boot coordinator event channel closed".to_string())?;
        match event {
            CoordinatorEvent::UiAction(ui::Action::BootEntry(menu_index)) => {
                let Some(entry_index) = bootable_entry_indices.get(menu_index).copied() else {
                    tracing::warn!(menu_index, "UI requested unknown boot entry");
                    failed("The selected boot entry is no longer available".to_string());
                    continue;
                };
                let entry = &boot_entries[entry_index];
                tracing::info!(
                    id = %entry.id,
                    source = %entry.source.display(),
                    "booting UI-selected entry"
                );
                failed(returned_boot_error(boot(entry)));
            }
            CoordinatorEvent::Fastboot(result) => {
                // The old gadget has been joined and unbound before this event.
                // Anything that returns must restore both fastboot and ADB.
                let result = match result {
                    Ok(Some(action)) => {
                        tracing::info!("running fastboot post-response action");
                        action()
                            .map_err(|err| format!("fastboot post-response action failed: {err}"))
                    }
                    Ok(None) if discovery_complete => boot_default_entry(&boot_entries, &mut boot),
                    Ok(None) => {
                        tracing::info!(
                            "fastboot exited; waiting for boot discovery before default boot"
                        );
                        fastboot_requested_default = true;
                        continue;
                    }
                    Err(err) => Err(format!("fastboot service failed: {err}")),
                };
                failed(returned_boot_error(result));
                restart_fastboot();
            }
            CoordinatorEvent::DiscoveryUpdate(entries) => {
                apply_boot_entries_update(
                    ui,
                    &mut boot_entries,
                    &mut bootable_entry_indices,
                    entries,
                    false,
                );
            }
            CoordinatorEvent::DiscoveryComplete(entries) => {
                discovery_complete = true;
                apply_boot_entries_update(
                    ui,
                    &mut boot_entries,
                    &mut bootable_entry_indices,
                    entries,
                    true,
                );
                if fastboot_requested_default {
                    fastboot_requested_default = false;
                    failed(returned_boot_error(boot_default_entry(
                        &boot_entries,
                        &mut boot,
                    )));
                    restart_fastboot();
                } else {
                    tracing::info!("boot discovery complete; holding for fastboot or UI selection");
                }
            }
        }
    }
}

fn apply_boot_entries_update(
    ui: Option<&ui::Handle>,
    boot_entries: &mut Vec<bootflow::BootEntry>,
    bootable_entry_indices: &mut Vec<usize>,
    entries: Vec<bootflow::BootEntry>,
    scan_complete: bool,
) {
    let (indices, menu_entries) = boot_menu_entries(&entries);
    *boot_entries = entries;
    *bootable_entry_indices = indices;
    if let Some(ui) = ui {
        ui.update_boot_entries(menu_entries, scan_complete);
    }
}

fn log_settle_report(settled: &settle::Report) {
    if settled.timed_out {
        tracing::warn!(
            elapsed_ms = settled.elapsed.as_millis(),
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

/// A failed handoff must leave pocketboot running: PID 1 exiting panics the
/// kernel and reboots the device before anyone can read the reason.
fn report_boot_failure(ui: Option<&ui::Handle>, err: String) {
    tracing::error!(error = %err, "boot attempt failed; returning to the boot menu");
    if let Some(ui) = ui {
        ui.boot_failed(err);
    }
}

// A successful reboot/kexec never returns. Treat even an unexpected Ok as a
// failed handoff, not permission to return from main and kill PID 1.
fn returned_boot_error(result: Result<()>) -> String {
    result
        .err()
        .unwrap_or_else(|| "Boot operation returned without rebooting".to_string())
}

fn boot_default_entry(
    boot_entries: &[bootflow::BootEntry],
    boot: &mut impl FnMut(&bootflow::BootEntry) -> Result<()>,
) -> Result<()> {
    let entry = boot_entries
        .iter()
        .find(|entry| entry.is_directly_bootable())
        .ok_or_else(|| "No directly bootable entries found".to_string())?;
    tracing::info!(id = %entry.id, source = %entry.source.display(), "booting discovered entry");
    boot(entry)
}

fn boot_discovered_entry(entry: &bootflow::BootEntry) -> Result<()> {
    entry
        .load()
        .map_err(|err| format!("load discovered boot entry {}: {err}", entry.id))?;
    kexec::exec_loaded_image()
        .map_err(|err| format!("execute discovered boot entry {}: {err}", entry.id))?;
    Ok(())
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

    #[derive(Default)]
    struct RecoveryReport {
        booted: Vec<String>,
        failures: Vec<String>,
        restarts: usize,
    }

    fn run_recovery_events(
        events: Vec<CoordinatorEvent>,
        boot_result: Result<()>,
    ) -> RecoveryReport {
        let (tx, rx) = async_channel::unbounded();
        for event in events {
            tx.try_send(event).unwrap();
        }
        // Closing this test input ends the otherwise permanent coordinator.
        drop(tx);
        let mut report = RecoveryReport::default();
        let error = runtime::block_on(run_boot_coordinator_with(
            None,
            rx,
            |entry| {
                report.booted.push(entry.id.clone());
                boot_result.clone()
            },
            |error| report.failures.push(error),
            || report.restarts += 1,
        ))
        .unwrap_err();
        assert_eq!(error, "boot coordinator event channel closed");
        report
    }

    #[test]
    fn failed_ui_boot_can_be_retried_without_restarting_a_live_gadget() {
        let report = run_recovery_events(
            vec![
                CoordinatorEvent::DiscoveryComplete(vec![bootflow::BootEntry::test_entry(
                    "pmos", "",
                )]),
                CoordinatorEvent::UiAction(ui::Action::BootEntry(0)),
                CoordinatorEvent::UiAction(ui::Action::BootEntry(0)),
            ],
            Err("missing parking contract".into()),
        );
        assert_eq!(report.booted, ["pmos", "pmos"]);
        assert_eq!(report.failures, ["missing parking contract"; 2]);
        assert_eq!(report.restarts, 0);
    }

    #[test]
    fn invalid_menu_selection_is_reported_and_does_not_block_the_next_attempt() {
        let report = run_recovery_events(
            vec![
                CoordinatorEvent::DiscoveryUpdate(vec![bootflow::BootEntry::test_entry(
                    "pmos", "",
                )]),
                CoordinatorEvent::UiAction(ui::Action::BootEntry(7)),
                CoordinatorEvent::UiAction(ui::Action::BootEntry(0)),
            ],
            Err("load failed".into()),
        );
        assert_eq!(report.booted, ["pmos"]);
        assert!(report.failures[0].contains("no longer available"));
        assert_eq!(report.failures[1], "load failed");
    }

    #[test]
    fn failed_default_boot_restarts_usb_before_and_after_discovery() {
        for discovery_first in [false, true] {
            let discovery = CoordinatorEvent::DiscoveryComplete(vec![
                bootflow::BootEntry::test_entry("unresolved", "$missing"),
                bootflow::BootEntry::test_entry("pmos", ""),
            ]);
            let request = CoordinatorEvent::Fastboot(Ok(None));
            let events = if discovery_first {
                vec![discovery, request]
            } else {
                vec![request, discovery]
            };
            let report = run_recovery_events(events, Err("load failed".into()));
            assert_eq!(report.booted, ["pmos"]);
            assert_eq!(report.failures, ["load failed"]);
            assert_eq!(report.restarts, 1);
        }
    }

    #[test]
    fn default_boot_with_no_usable_entries_restores_recovery_instead_of_exiting() {
        for entries in [
            vec![],
            vec![bootflow::BootEntry::test_entry("unresolved", "$missing")],
        ] {
            let report = run_recovery_events(
                vec![
                    CoordinatorEvent::Fastboot(Ok(None)),
                    CoordinatorEvent::DiscoveryComplete(entries),
                    CoordinatorEvent::Fastboot(Ok(None)),
                ],
                Ok(()),
            );
            assert!(report.booted.is_empty());
            assert_eq!(report.failures, ["No directly bootable entries found"; 2]);
            assert_eq!(report.restarts, 2);
        }
    }

    #[test]
    fn returned_fastboot_actions_and_service_failures_restart_usb() {
        let report = run_recovery_events(
            vec![
                CoordinatorEvent::Fastboot(Ok(Some(Box::new(|| {
                    Err(io::Error::other("action failed"))
                })))),
                CoordinatorEvent::Fastboot(Err("UDC unavailable".into())),
                CoordinatorEvent::Fastboot(Ok(Some(Box::new(|| Ok(()))))),
            ],
            Ok(()),
        );
        assert!(report.booted.is_empty());
        assert_eq!(report.restarts, 3);
        assert!(report.failures[0].contains("action failed"));
        assert!(report.failures[1].contains("UDC unavailable"));
        assert!(report.failures[2].contains("returned without rebooting"));
    }

    #[test]
    fn unexpectedly_returning_boot_operations_do_not_exit_the_coordinator() {
        let report = run_recovery_events(
            vec![
                CoordinatorEvent::DiscoveryComplete(vec![bootflow::BootEntry::test_entry(
                    "pmos", "",
                )]),
                CoordinatorEvent::UiAction(ui::Action::BootEntry(0)),
                CoordinatorEvent::Fastboot(Ok(None)),
            ],
            Ok(()),
        );
        assert_eq!(report.booted, ["pmos", "pmos"]);
        assert_eq!(
            report.failures,
            ["Boot operation returned without rebooting"; 2]
        );
        assert_eq!(report.restarts, 1);
    }

    #[test]
    fn recovery_sender_keeps_pid1_waiting_when_ui_and_gadget_are_absent() {
        use std::{cell::Cell, rc::Rc};

        let (tx, rx) = async_channel::unbounded();
        tx.try_send(CoordinatorEvent::Fastboot(Err("no gadget".into())))
            .unwrap();
        let recovery_tx = tx.clone();
        let restarted = Rc::new(Cell::new(0));
        let restarted_callback = restarted.clone();
        let mut coordinator = Box::pin(run_boot_coordinator_with(
            None,
            rx,
            |_| panic!("no boot requested"),
            |_| {},
            move || {
                // Same lifetime contract as run(): recovery retains a sender
                // even if spawning a replacement gadget fails.
                let _keep_open = &recovery_tx;
                restarted_callback.set(restarted_callback.get() + 1);
            },
        ));
        drop(tx);
        assert!(runtime::block_on(futures_lite::future::poll_once(coordinator.as_mut())).is_none());
        assert_eq!(restarted.get(), 1);
        assert!(runtime::block_on(futures_lite::future::poll_once(coordinator.as_mut())).is_none());
    }

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
}
