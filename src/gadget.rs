use std::{
    fs, io,
    os::unix::{ffi::OsStrExt, fs::symlink},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use gadgetry_most_foul::{
    Class, Config, Gadget as RawGadget, Id, RegGadget, Strings, Udc, default_udc,
    function::{
        Handle,
        serial::{Serial, SerialClass},
    },
};

use crate::{
    adb,
    fastboot::{self, PostResponseAction},
};

const CONFIGFS: &str = "/sys/kernel/config";
const VENDOR_ID: u16 = Id::LINUX_FOUNDATION_VID;
const PRODUCT_ID: u16 = 0x0104;
const CONFIG_NAME: &str = "pocketboot";
const CONFIG_MAX_POWER_MA: u16 = 2;
const CONFIG_DIR: &str = "configs/c.1";
const FUNCTIONS_DIR: &str = "functions";
const MASS_STORAGE_FUNCTION: &str = "mass_storage.pocketboot-ums";
const MASS_STORAGE_INQUIRY: &str = "pocketboot UMS";
const MAX_UMS_LUNS: usize = 8;

pub(crate) type ThreadResult = io::Result<Option<PostResponseAction>>;

#[derive(Clone)]
pub(crate) struct Gadget {
    state: Arc<Mutex<State>>,
    serialno: String,
}

#[derive(Default)]
struct State {
    reg: Option<RegGadget>,
    udc: Option<Udc>,
    ums: UmsState,
}

#[derive(Default)]
struct UmsState {
    function_dir: Option<PathBuf>,
    slots: Vec<Option<PathBuf>>,
}

impl UmsState {
    fn clear(&mut self) {
        self.function_dir = None;
        self.slots.clear();
    }

    fn active_count(&self) -> usize {
        self.slots.iter().flatten().count()
    }
}

pub(crate) enum MassStorageStart {
    Started {
        lun: usize,
    },
    AlreadyStarted {
        lun: usize,
    },
    AfterResponse {
        lun: usize,
        action: PostResponseAction,
    },
}

pub(crate) enum MassStorageStop {
    Stopped {
        lun: usize,
    },
    AfterResponse {
        lun: usize,
        action: PostResponseAction,
    },
}

#[allow(dead_code)]
pub(crate) enum Mode {
    Console,
    Fastboot {
        commands: fastboot::CommandMap,
        acm: bool,
    },
}

impl Mode {
    fn label(&self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::Fastboot { .. } => "fastboot",
        }
    }
}

trait GadgetFunction {
    fn handle(&self) -> Handle;
}

struct AcmFunction {
    _serial: Serial,
    handle: Handle,
}

impl AcmFunction {
    fn new() -> Self {
        let (serial, handle) = Serial::new(SerialClass::Acm);
        Self {
            _serial: serial,
            handle,
        }
    }
}

impl GadgetFunction for AcmFunction {
    fn handle(&self) -> Handle {
        self.handle.clone()
    }
}

impl GadgetFunction for fastboot::UsbFunction {
    fn handle(&self) -> Handle {
        self.handle()
    }
}

impl GadgetFunction for adb::UsbFunction {
    fn handle(&self) -> Handle {
        self.handle()
    }
}

impl Gadget {
    pub(crate) fn new(serialno: impl Into<String>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            serialno: serialno.into(),
        }
    }

    pub(crate) fn spawn(&self, mode: Mode) -> io::Result<thread::JoinHandle<ThreadResult>> {
        let gadget = self.clone();
        let label = mode.label();
        let thread_name = format!("pocketboot-{label}");
        let thread =
            thread::Builder::new()
                .name(thread_name.clone())
                .spawn(move || match gadget.run(mode) {
                    Ok(action) => Ok(action),
                    Err(err) => {
                        tracing::error!(error = ?err, "USB gadget failed");
                        Err(err)
                    }
                })?;
        tracing::info!(
            thread = thread_name,
            mode = label,
            "USB gadget thread spawned"
        );
        Ok(thread)
    }

    pub(crate) fn start_mass_storage(&self, backing: PathBuf) -> io::Result<MassStorageStart> {
        let mut state = self.state.lock().unwrap();
        if let Some(lun) = mass_storage_lun(&state.ums, &backing) {
            return Ok(MassStorageStart::AlreadyStarted { lun });
        }

        if state.ums.function_dir.is_none() {
            let gadget = self.clone();
            return Ok(MassStorageStart::AfterResponse {
                lun: 0,
                action: Box::new(move || gadget.activate_mass_storage(backing)),
            });
        }

        let lun = attach_mass_storage_slot(&mut state, backing)?;
        Ok(MassStorageStart::Started { lun })
    }

    pub(crate) fn stop_mass_storage(&self, backing: PathBuf) -> io::Result<MassStorageStop> {
        let mut state = self.state.lock().unwrap();
        let lun = mass_storage_lun(&state.ums, &backing).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("UMS is not started for {}", backing.display()),
            )
        })?;

        detach_mass_storage_slot(&mut state, lun)?;
        if state.ums.active_count() == 0 {
            let gadget = self.clone();
            Ok(MassStorageStop::AfterResponse {
                lun,
                action: Box::new(move || gadget.deactivate_mass_storage()),
            })
        } else {
            Ok(MassStorageStop::Stopped { lun })
        }
    }

    fn activate_mass_storage(&self, backing: PathBuf) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if mass_storage_lun(&state.ums, &backing).is_some() {
            tracing::info!(backing = %backing.display(), "UMS backing already active after response");
            return Ok(());
        }
        if state.ums.function_dir.is_some() {
            attach_mass_storage_slot(&mut state, backing)?;
            return Ok(());
        }

        let reg = state.reg.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "USB gadget is not registered")
        })?;
        let udc = state.udc.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "USB gadget is not bound")
        })?;
        let udc_name = udc.name().to_string_lossy().into_owned();
        let gadget_path = reg.path().to_path_buf();

        tracing::info!(backing = %backing.display(), udc = %udc_name, "adding UMS function");
        reg.bind(None)?;
        tracing::info!(udc = %udc_name, "USB gadget unbound for UMS add");

        let function_dir = match create_mass_storage_function(&gadget_path) {
            Ok(function_dir) => function_dir,
            Err(add_err) => return rebind_after_error(reg, &udc, add_err, "UMS add"),
        };

        if let Err(attach_err) = attach_mass_storage_lun(&lun_dir(&function_dir, 0), &backing) {
            cleanup_mass_storage_function(&gadget_path);
            return rebind_after_error(reg, &udc, attach_err, "UMS attach");
        }

        if let Err(bind_err) = reg.bind(Some(&udc)) {
            cleanup_mass_storage_function(&gadget_path);
            return rebind_after_error(reg, &udc, bind_err, "UMS bind");
        }

        state.ums.function_dir = Some(function_dir);
        state.ums.slots = vec![None; MAX_UMS_LUNS];
        state.ums.slots[0] = Some(backing.clone());
        tracing::info!(backing = %backing.display(), lun = 0, udc = %udc_name, "USB gadget rebound with UMS function");
        Ok(())
    }

    fn deactivate_mass_storage(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.ums.function_dir.is_none() {
            tracing::info!("UMS function already removed");
            return Ok(());
        }
        if state.ums.active_count() != 0 {
            tracing::info!(
                active = state.ums.active_count(),
                "keeping UMS function with active LUNs"
            );
            return Ok(());
        }

        let reg = state.reg.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "USB gadget is not registered")
        })?;
        let udc = state.udc.clone().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "USB gadget is not bound")
        })?;
        let udc_name = udc.name().to_string_lossy().into_owned();
        let gadget_path = reg.path().to_path_buf();

        tracing::info!(udc = %udc_name, "removing idle UMS function");
        reg.bind(None)?;
        tracing::info!(udc = %udc_name, "USB gadget unbound for UMS removal");

        if let Err(remove_err) = remove_mass_storage_function(&gadget_path) {
            return rebind_after_error(reg, &udc, remove_err, "UMS removal");
        }

        reg.bind(Some(&udc))?;
        state.ums.clear();
        tracing::info!(udc = %udc_name, "USB gadget rebound without UMS function");
        Ok(())
    }

    fn run(&self, mode: Mode) -> ThreadResult {
        self.setup_gadget(mode)
    }

    fn setup_gadget(&self, mode: Mode) -> ThreadResult {
        mount_configfs()?;
        tracing::debug!(path = CONFIGFS, "configfs mounted");

        match mode {
            Mode::Console => self.setup_console_gadget(),
            Mode::Fastboot { commands, acm } => self.setup_fastboot_gadget(commands, acm),
        }
    }

    fn setup_console_gadget(&self) -> ThreadResult {
        let serial = AcmFunction::new();
        let config = config_with_functions(&[&serial]);
        self.register_and_bind(config)?;
        Ok(None)
    }

    fn setup_fastboot_gadget(&self, commands: fastboot::CommandMap, acm: bool) -> ThreadResult {
        let fastboot_function = fastboot::UsbFunction::new(commands);
        let adb_function = adb::UsbFunction::new();
        let config = if acm {
            let serial = AcmFunction::new();
            config_with_functions(&[&serial, &fastboot_function, &adb_function])
        } else {
            config_with_functions(&[&fastboot_function, &adb_function])
        };
        self.register_and_bind(config)?;

        let (server, event_loop) = fastboot_function.start()?;
        let (adb_server, adb_event_loop) = adb_function.start()?;
        let adb_handle = adb_server.spawn()?;
        let server_result = server.run();
        match &server_result {
            Ok(action) => tracing::info!(
                has_action = action.is_some(),
                "fastboot server exited normally"
            ),
            Err(err) => tracing::warn!(error = ?err, "fastboot server exited with error"),
        }

        adb_handle.stop();
        event_loop.stop();
        adb_event_loop.stop();
        let unbind_result = self.unbind_and_remove();
        match &unbind_result {
            Ok(()) => tracing::info!("USB gadget unbound"),
            Err(err) => tracing::warn!(error = ?err, "USB gadget unbind failed"),
        }

        if let Err(err) = adb_handle.join() {
            tracing::warn!(error = ?err, "adb server exited with error");
        }
        event_loop.join();
        adb_event_loop.join();

        let action = server_result?;
        unbind_result?;
        Ok(action)
    }

    fn register_and_bind(&self, config: Config) -> io::Result<()> {
        let gadget = RawGadget::new(
            Class::INTERFACE_SPECIFIC,
            Id::new(VENDOR_ID, PRODUCT_ID),
            Strings::new("pocketboot", "pocketboot", &self.serialno),
        )
        .with_config(config);

        let reg = gadget.register()?;
        tracing::info!(path = %reg.path().display(), "USB gadget registered");

        let udc = wait_for_udc(Duration::from_secs(10))?;
        let udc_name = udc.name().to_string_lossy().into_owned();
        reg.bind(Some(&udc))?;
        tracing::info!(udc = %udc_name, "USB gadget bound");

        let mut state = self.state.lock().unwrap();
        if state.reg.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "USB gadget is already registered",
            ));
        }
        state.udc = Some(udc);
        state.ums.clear();
        state.reg = Some(reg);
        Ok(())
    }

    fn unbind_and_remove(&self) -> io::Result<()> {
        let reg = {
            let mut state = self.state.lock().unwrap();
            state.udc = None;
            state.ums.clear();
            state.reg.take()
        };

        let Some(reg) = reg else {
            return Ok(());
        };
        reg.bind(None)?;
        drop(reg);
        Ok(())
    }
}

fn config_with_functions(functions: &[&dyn GadgetFunction]) -> Config {
    let mut config = Config::new(CONFIG_NAME);
    config.max_power = CONFIG_MAX_POWER_MA;
    for function in functions {
        config.add_function(function.handle());
    }
    config
}

fn mass_storage_lun(ums: &UmsState, backing: &Path) -> Option<usize> {
    ums.slots
        .iter()
        .position(|slot| slot.as_deref() == Some(backing))
}

fn attach_mass_storage_slot(state: &mut State, backing: PathBuf) -> io::Result<usize> {
    let function_dir = state
        .ums
        .function_dir
        .as_ref()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "UMS function is not registered",
            )
        })?
        .clone();
    let lun = state
        .ums
        .slots
        .iter()
        .position(Option::is_none)
        .ok_or_else(|| io::Error::other(format!("no free UMS LUNs; maximum is {MAX_UMS_LUNS}")))?;

    attach_mass_storage_lun(&lun_dir(&function_dir, lun), &backing).map_err(|err| {
        tracing::warn!(backing = %backing.display(), lun, error = ?err, "failed to attach UMS LUN backing");
        err
    })?;

    state.ums.slots[lun] = Some(backing.clone());
    tracing::info!(backing = %backing.display(), lun, "UMS LUN attached");
    Ok(lun)
}

fn detach_mass_storage_slot(state: &mut State, lun: usize) -> io::Result<()> {
    let function_dir = state
        .ums
        .function_dir
        .as_ref()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "UMS function is not registered",
            )
        })?
        .clone();
    let backing = state
        .ums
        .slots
        .get(lun)
        .and_then(Option::as_ref)
        .cloned()
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("UMS LUN {lun} is empty"))
        })?;

    detach_mass_storage_lun(&lun_dir(&function_dir, lun)).map_err(|err| {
        tracing::warn!(backing = %backing.display(), lun, error = ?err, "failed to detach UMS LUN backing");
        err
    })?;

    state.ums.slots[lun] = None;
    tracing::info!(backing = %backing.display(), lun, "UMS LUN detached");
    Ok(())
}

fn create_mass_storage_function(gadget_path: &Path) -> io::Result<PathBuf> {
    let function_dir = mass_storage_function_dir(gadget_path);
    let config_link = mass_storage_config_link(gadget_path);

    fs::create_dir(&function_dir)?;
    let result = configure_mass_storage_lun(&lun_dir(&function_dir, 0))
        .and_then(|()| create_extra_mass_storage_luns(&function_dir))
        .and_then(|()| symlink(&function_dir, &config_link));
    if result.is_err() {
        cleanup_mass_storage_function(gadget_path);
    }
    result.map(|()| function_dir)
}

fn create_extra_mass_storage_luns(function_dir: &Path) -> io::Result<()> {
    for lun in 1..MAX_UMS_LUNS {
        let lun_dir = lun_dir(function_dir, lun);
        fs::create_dir(&lun_dir)?;
        configure_mass_storage_lun(&lun_dir)?;
    }
    Ok(())
}

fn configure_mass_storage_lun(lun_dir: &Path) -> io::Result<()> {
    fs::write(lun_dir.join("ro"), "0")?;
    fs::write(lun_dir.join("cdrom"), "0")?;
    fs::write(lun_dir.join("nofua"), "0")?;
    fs::write(lun_dir.join("removable"), "1")?;
    fs::write(lun_dir.join("inquiry_string"), MASS_STORAGE_INQUIRY)
}

fn attach_mass_storage_lun(lun_dir: &Path, backing: &Path) -> io::Result<()> {
    fs::write(lun_dir.join("file"), backing.as_os_str().as_bytes())
}

fn detach_mass_storage_lun(lun_dir: &Path) -> io::Result<()> {
    fs::write(lun_dir.join("file"), b"\n")
}

fn remove_mass_storage_function(gadget_path: &Path) -> io::Result<()> {
    remove_optional_file(&mass_storage_config_link(gadget_path))?;
    remove_mass_storage_function_dir(&mass_storage_function_dir(gadget_path))
}

fn cleanup_mass_storage_function(gadget_path: &Path) {
    if let Err(err) = remove_mass_storage_function(gadget_path) {
        tracing::debug!(error = ?err, "failed to clean up UMS function");
    }
}

fn remove_mass_storage_function_dir(function_dir: &Path) -> io::Result<()> {
    for lun in (1..MAX_UMS_LUNS).rev() {
        remove_optional_dir(&lun_dir(function_dir, lun))?;
    }
    remove_optional_dir(function_dir)
}

fn remove_optional_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn remove_optional_dir(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn rebind_after_error(
    reg: &RegGadget,
    udc: &Udc,
    err: io::Error,
    operation: &str,
) -> io::Result<()> {
    let udc_name = udc.name().to_string_lossy();
    match reg.bind(Some(udc)) {
        Ok(()) => {
            tracing::info!(udc = %udc_name, operation, "USB gadget rebound after UMS error");
            Err(err)
        }
        Err(rebind_err) => {
            tracing::error!(udc = %udc_name, operation, error = ?rebind_err, "USB rebind failed after UMS error");
            Err(io::Error::new(
                rebind_err.kind(),
                format!("{operation} failed: {err}; USB rebind failed: {rebind_err}"),
            ))
        }
    }
}

fn mass_storage_function_dir(gadget_path: &Path) -> PathBuf {
    gadget_path.join(FUNCTIONS_DIR).join(MASS_STORAGE_FUNCTION)
}

fn mass_storage_config_link(gadget_path: &Path) -> PathBuf {
    gadget_path.join(CONFIG_DIR).join(MASS_STORAGE_FUNCTION)
}

fn lun_dir(function_dir: &Path, index: usize) -> PathBuf {
    function_dir.join(format!("lun.{index}"))
}

fn mount_configfs() -> io::Result<()> {
    fs::create_dir_all(CONFIGFS)?;
    let result = unsafe {
        libc::mount(
            c"configfs".as_ptr(),
            c"/sys/kernel/config".as_ptr(),
            c"configfs".as_ptr(),
            0,
            std::ptr::null::<libc::c_void>(),
        )
    };
    if result == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EBUSY) {
        Ok(())
    } else {
        Err(err)
    }
}

fn wait_for_udc(timeout: Duration) -> io::Result<Udc> {
    let start = std::time::Instant::now();
    loop {
        match default_udc() {
            Ok(udc) => return Ok(udc),
            Err(err) if start.elapsed() < timeout => {
                tracing::debug!(error = ?err, "waiting for UDC");
                thread::sleep(Duration::from_millis(250));
            }
            Err(err) => return Err(err),
        }
    }
}
