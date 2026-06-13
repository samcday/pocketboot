use std::{fs, io, thread, time::Duration};

use gadgetry_most_foul::{
    Class, Config, Gadget, Id, Strings, default_udc,
    function::serial::{Serial, SerialClass},
};

use crate::fastboot::{self, PostResponseAction};

const CONFIGFS: &str = "/sys/kernel/config";
const VENDOR_ID: u16 = Id::LINUX_FOUNDATION_VID;
const PRODUCT_ID: u16 = 0x0104;
pub(crate) type ThreadResult = io::Result<Option<PostResponseAction>>;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Mode {
    Console,
    Fastboot,
}

pub(crate) fn spawn(mode: Mode) -> io::Result<thread::JoinHandle<ThreadResult>> {
    let thread = thread::Builder::new()
        .name("pocketboot-fastboot".to_string())
        .spawn(move || match run(mode) {
            Ok(action) => Ok(action),
            Err(err) => {
                tracing::error!(error = ?err, "USB gadget failed");
                Err(err)
            }
        })?;
    tracing::info!(
        thread = "pocketboot-fastboot",
        ?mode,
        "USB gadget thread spawned"
    );
    Ok(thread)
}

fn run(mode: Mode) -> ThreadResult {
    setup_gadget(mode)
}

fn setup_gadget(mode: Mode) -> ThreadResult {
    mount_configfs()?;
    tracing::debug!(path = CONFIGFS, "configfs mounted");

    let (_serial, serial_handle) = Serial::new(SerialClass::Acm);
    let fastboot_function = matches!(mode, Mode::Fastboot).then(fastboot::UsbFunction::new);
    let mut config = Config::new("pocketboot").with_function(serial_handle);
    if let Some(fastboot_function) = &fastboot_function {
        config.add_function(fastboot_function.handle());
    }

    let gadget = Gadget::new(
        Class::INTERFACE_SPECIFIC,
        Id::new(VENDOR_ID, PRODUCT_ID),
        Strings::new("pocketboot", "pocketboot", "0001"),
    )
    .with_config(config);

    let mut reg = gadget.register()?;
    tracing::info!(path = %reg.path().display(), "USB gadget registered");

    let udc = wait_for_udc(Duration::from_secs(10))?;
    let udc_name = udc.name().to_string_lossy().into_owned();
    reg.bind(Some(&udc))?;
    tracing::info!(udc = %udc_name, "USB gadget bound");

    let Some(fastboot_function) = fastboot_function else {
        reg.detach();
        tracing::debug!("USB gadget detached from setup thread");
        return Ok(None);
    };

    let (server, event_loop) = fastboot_function.start(fastboot::commands::default_commands())?;
    let server_result = server.run();
    tracing::info!("fastboot server exited");

    event_loop.stop();
    let unbind_result = reg.bind(None);
    match &unbind_result {
        Ok(()) => tracing::info!("USB gadget unbound"),
        Err(err) => tracing::warn!(error = ?err, "USB gadget unbind failed"),
    }

    event_loop.join();

    let action = server_result?;
    unbind_result?;
    Ok(action)
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

fn wait_for_udc(timeout: Duration) -> io::Result<gadgetry_most_foul::Udc> {
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
