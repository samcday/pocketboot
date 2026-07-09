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
    let ui = match ui::spawn(battery, system_info) {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::warn!(error = %err, "failed to spawn UI thread");
            None
        }
    };
    let gadget = gadget::Gadget::new(serialno.clone());
    let acm = cmdline.is_set(ACM_CMDLINE_PARAM);
    let fastboot_thread = gadget
        .spawn(gadget::Mode::Fastboot {
            commands: fastboot_commands(gadget.clone(), serialno, cmdline.clone()),
            acm,
        })
        .map_err(|err| format!("spawn fastboot gadget thread: {err}"))?;
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
    spawn_fastboot_joiner(fastboot_thread, event_tx.clone());
    spawn_boot_discovery(event_tx.clone());
    drop(event_tx);

    run_boot_coordinator(ui.as_ref(), event_rx).await?;
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

fn spawn_fastboot_joiner(
    fastboot_thread: thread::JoinHandle<gadget::ThreadResult>,
    event_tx: async_channel::Sender<CoordinatorEvent>,
) {
    runtime::detach(async move {
        let result = runtime::unblock(move || join_fastboot_thread(fastboot_thread)).await;
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
                    continue;
                };
                let entry = &boot_entries[entry_index];
                tracing::info!(
                    id = %entry.id,
                    source = %entry.source.display(),
                    "booting UI-selected entry"
                );
                return boot_discovered_entry(entry);
            }
            CoordinatorEvent::Fastboot(result) => {
                let action = result?;
                if let Some(action) = action {
                    tracing::info!("running fastboot post-response action");
                    action()
                        .map_err(|err| format!("fastboot post-response action failed: {err}"))?;
                    return Ok(());
                }

                if discovery_complete {
                    boot_default_entry(&boot_entries)?;
                    return Ok(());
                }

                tracing::info!("fastboot exited; waiting for boot discovery before default boot");
                fastboot_requested_default = true;
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
                    boot_default_entry(&boot_entries)?;
                    return Ok(());
                }
                tracing::info!("boot discovery complete; holding for fastboot or UI selection");
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

fn boot_default_entry(boot_entries: &[bootflow::BootEntry]) -> Result<()> {
    if let Some(entry) = boot_entries
        .iter()
        .find(|entry| entry.is_directly_bootable())
    {
        tracing::info!(id = %entry.id, source = %entry.source.display(), "booting discovered entry");
        boot_discovered_entry(entry)?;
    } else if !boot_entries.is_empty() {
        tracing::warn!("boot entries were discovered, but none are directly bootable yet");
    }
    Ok(())
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
    commands.push(fastboot::commands::reboot_command());
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
}
