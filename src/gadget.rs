use std::{
    ffi::OsStr,
    fs,
    io::{self, Write},
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
    usb_device_role: bool,
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

    fn set_function(&mut self, function_dir: PathBuf) {
        self.function_dir = Some(function_dir);
        self.slots = vec![None; MAX_UMS_LUNS];
    }
}

pub(crate) enum MassStorageStart {
    Started { lun: usize },
    AlreadyStarted { lun: usize },
}

pub(crate) enum MassStorageStop {
    Stopped { lun: usize },
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
            usb_device_role: false,
        }
    }

    pub(crate) fn with_usb_device_role(mut self, requested: bool) -> Self {
        self.usb_device_role = requested;
        self
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
        Ok(MassStorageStop::Stopped { lun })
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
        self.register_and_bind(config, false)?;
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
        self.register_and_bind(config, true)?;

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

        resolve_fastboot_result(server_result, unbind_result)
    }

    fn register_and_bind(&self, config: Config, include_mass_storage: bool) -> io::Result<()> {
        let gadget = RawGadget::new(
            Class::INTERFACE_SPECIFIC,
            Id::new(VENDOR_ID, PRODUCT_ID),
            Strings::new("pocketboot", "pocketboot", &self.serialno),
        )
        .with_config(config);

        let reg = gadget.register()?;
        tracing::info!(path = %reg.path().display(), "USB gadget registered");

        let mass_storage_function_dir = if include_mass_storage {
            let function_dir = create_mass_storage_function(reg.path())?;
            tracing::info!(path = %function_dir.display(), luns = MAX_UMS_LUNS, "UMS function registered");
            Some(function_dir)
        } else {
            None
        };

        let udc = wait_for_udc(Duration::from_secs(10))?;
        let udc_name = udc.name().to_string_lossy().into_owned();
        reg.bind(Some(&udc))?;
        tracing::info!(udc = %udc_name, "USB gadget bound");
        // Keep reg local until this succeeds: its drop unbinds/removes on error.
        self.request_usb_device_role(Path::new("/sys/class"), udc.name())?;

        let mut state = self.state.lock().unwrap();
        if state.reg.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "USB gadget is already registered",
            ));
        }
        state.udc = Some(udc);
        if let Some(function_dir) = mass_storage_function_dir {
            state.ums.set_function(function_dir);
        } else {
            state.ums.clear();
        }
        state.reg = Some(reg);
        Ok(())
    }

    fn request_usb_device_role(&self, sys_class: &Path, udc: &OsStr) -> io::Result<()> {
        if !self.usb_device_role {
            return Ok(());
        }
        let role_switch = request_device_role(sys_class, udc).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("request USB device role for UDC {}: {err}", udc.display()),
            )
        })?;
        tracing::info!(
            udc = %udc.display(),
            role_switch = %role_switch.display(),
            "USB device role requested"
        );
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

fn resolve_fastboot_result(
    server_result: ThreadResult,
    unbind_result: io::Result<()>,
) -> ThreadResult {
    let action = server_result?;
    if action.is_some() {
        if let Err(err) = unbind_result {
            tracing::warn!(
                error = ?err,
                "USB gadget cleanup failed after fastboot action was acknowledged; preserving action"
            );
        }
        return Ok(action);
    }

    unbind_result?;
    Ok(None)
}

fn request_device_role(sys_class: &Path, udc: &OsStr) -> io::Result<PathBuf> {
    let controller = canonical_controller(&sys_class.join("udc").join(udc))?;
    let role_class = sys_class.join("usb_role");
    let entries = fs::read_dir(&role_class).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("read USB role-switch class {}: {err}", role_class.display()),
        )
    })?;
    let mut matched: Option<PathBuf> = None;
    for entry in entries {
        let role_switch = entry?.path();
        // Fail closed on unresolved entries: uniqueness is unproven.
        if canonical_controller(&role_switch)? != controller {
            continue;
        }
        if let Some(previous) = matched {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ambiguous USB role switches for {}: {} and {}",
                    controller.display(),
                    previous.display(),
                    role_switch.display()
                ),
            ));
        }
        matched = Some(role_switch);
    }
    let role_switch = matched.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no USB role switch matches UDC device {}",
                controller.display()
            ),
        )
    })?;
    let role = role_switch.join("role");
    // Prove a unique match before opening; never create a missing attribute.
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&role)
        .and_then(|mut file| file.write_all(b"device"))
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("write USB role attribute {}: {err}", role.display()),
            )
        })?;
    Ok(role_switch)
}

fn canonical_controller(class_device: &Path) -> io::Result<PathBuf> {
    let device = class_device.join("device");
    fs::canonicalize(&device).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("resolve USB controller device {}: {err}", device.display()),
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    struct UsbRoleFixture {
        root: PathBuf,
    }

    impl UsbRoleFixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "pocketboot-usb-role-test-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(root.join("class/udc")).unwrap();
            fs::create_dir_all(root.join("class/usb_role")).unwrap();
            let fixture = Self { root };
            let controller = fixture.controller("chosen");
            let udc = fixture.root.join("class/udc/chosen");
            fs::create_dir(&udc).unwrap();
            symlink(controller, udc.join("device")).unwrap();
            fixture
        }

        fn controller(&self, name: &str) -> PathBuf {
            let path = self.root.join("devices").join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn role_switch(&self, name: &str, controller: &Path) -> PathBuf {
            let path = self.root.join("class/usb_role").join(name);
            fs::create_dir(&path).unwrap();
            symlink(controller, path.join("device")).unwrap();
            fs::write(path.join("role"), b"none\n").unwrap();
            path
        }

        fn request(&self) -> io::Result<()> {
            Gadget::new("test")
                .with_usb_device_role(true)
                .request_usb_device_role(&self.root.join("class"), OsStr::new("chosen"))
        }
    }

    impl Drop for UsbRoleFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn assert_role_unchanged(role_switch: &Path) {
        assert_eq!(fs::read(role_switch.join("role")).unwrap(), b"none\n");
    }

    #[test]
    fn default_usb_role_does_not_access_sysfs() {
        let gadget = Gadget::new("test");
        assert!(!gadget.usb_device_role);
        gadget
            .request_usb_device_role(Path::new("/nonexistent"), OsStr::new("missing"))
            .unwrap();
        assert!(gadget.with_usb_device_role(true).clone().usb_device_role);
    }

    #[test]
    fn usb_role_matches_only_the_exact_canonical_udc_controller() {
        let fixture = UsbRoleFixture::new();
        let controller = fixture.controller("chosen");
        let alias = fixture.root.join("alias");
        symlink(&controller, &alias).unwrap();
        let matched = fixture.role_switch("z-matched", &alias);
        let unrelated = fixture.role_switch("a-unrelated", &fixture.controller("other"));
        let child = fixture.role_switch("b-child", &fixture.controller("chosen/child"));
        let parent = fixture.role_switch("c-parent", &fixture.root.join("devices"));

        fixture.request().unwrap();

        assert_eq!(fs::read(matched.join("role")).unwrap(), b"device");
        for role_switch in [&unrelated, &child, &parent] {
            assert_role_unchanged(role_switch);
        }
    }

    #[test]
    fn missing_usb_role_match_leaves_unrelated_controller_unchanged() {
        let fixture = UsbRoleFixture::new();
        let unrelated = fixture.role_switch("other", &fixture.controller("other"));

        let err = fixture.request().unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("no USB role switch matches"));
        assert!(err.to_string().contains("UDC chosen"));
        assert_role_unchanged(&unrelated);
    }

    #[test]
    fn ambiguous_usb_role_match_leaves_all_controllers_unchanged() {
        let fixture = UsbRoleFixture::new();
        let controller = fixture.controller("chosen");
        let first = fixture.role_switch("first", &controller);
        let second = fixture.role_switch("second", &controller);
        let unrelated = fixture.role_switch("other", &fixture.controller("other"));

        let err = fixture.request().unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("ambiguous"));
        for role_switch in [&first, &second, &unrelated] {
            assert_role_unchanged(role_switch);
        }
    }

    #[test]
    fn unresolved_usb_role_controller_prevents_writes() {
        let fixture = UsbRoleFixture::new();
        let matched = fixture.role_switch("matched", &fixture.controller("chosen"));
        let broken = fixture.role_switch("broken", &fixture.root.join("missing"));

        let err = fixture.request().unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("resolve USB controller device"));
        assert_role_unchanged(&matched);
        assert_role_unchanged(&broken);
    }

    #[test]
    fn missing_usb_role_attribute_is_not_created() {
        let fixture = UsbRoleFixture::new();
        let matched = fixture.role_switch("matched", &fixture.controller("chosen"));
        let role = matched.join("role");
        fs::remove_file(&role).unwrap();

        let err = fixture.request().unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains(&role.display().to_string()));
        assert!(!role.exists());
    }

    #[test]
    fn acknowledged_action_takes_precedence_over_unbind_failure() {
        let ran = Arc::new(AtomicBool::new(false));
        let action_ran = ran.clone();
        let action: PostResponseAction = Box::new(move || {
            action_ran.store(true, Ordering::Relaxed);
            Ok(())
        });

        let returned = resolve_fastboot_result(
            Ok(Some(action)),
            Err(io::Error::other("configfs cleanup failed")),
        )
        .expect("acknowledged action must survive cleanup failure")
        .expect("action was dropped");
        returned().unwrap();

        assert!(ran.load(Ordering::Relaxed));
    }

    #[test]
    fn unbind_failure_is_fatal_without_an_acknowledged_action() {
        let result =
            resolve_fastboot_result(Ok(None), Err(io::Error::other("configfs cleanup failed")));

        assert!(result.is_err());
    }
}
