use std::{fs, io, thread, time::Duration};

use gadgetry_most_foul::{
    Class, Config, Gadget, Id, Strings, default_udc,
    function::serial::{Serial, SerialClass},
};

use crate::log_line;

const CONFIGFS: &str = "/sys/kernel/config";
const VENDOR_ID: u16 = Id::LINUX_FOUNDATION_VID;
const PRODUCT_ID: u16 = 0x0101;

pub(crate) fn spawn() {
    match thread::Builder::new()
        .name("pocketboot-gadget".to_string())
        .spawn(run)
    {
        Ok(_thread) => log_line("pocketboot: gadget init thread spawned"),
        Err(err) => log_line(&format!(
            "pocketboot: failed to spawn gadget init thread: {err}"
        )),
    }
}

fn run() {
    if let Err(err) = setup_acm() {
        log_line(&format!("pocketboot: CDC-ACM gadget setup failed: {err}"));
    }
}

fn setup_acm() -> io::Result<()> {
    mount_configfs()?;
    log_line("pocketboot: configfs mounted");

    let (_serial, handle) = Serial::new(SerialClass::Acm);
    let gadget = Gadget::new(
        Class::INTERFACE_SPECIFIC,
        Id::new(VENDOR_ID, PRODUCT_ID),
        Strings::new("pocketboot", "pocketboot console", "0001"),
    )
    .with_config(Config::new("pocketboot").with_function(handle));

    let mut reg = gadget.register()?;
    log_line(&format!(
        "pocketboot: CDC-ACM gadget registered at {}",
        reg.path().display()
    ));

    let udc = wait_for_udc(Duration::from_secs(10))?;
    let udc_name = udc.name().to_string_lossy().into_owned();
    reg.bind(Some(&udc))?;
    log_line(&format!(
        "pocketboot: CDC-ACM gadget bound to UDC {udc_name}"
    ));

    reg.detach();
    Ok(())
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
                log_line(&format!("pocketboot: waiting for UDC: {err}"));
                thread::sleep(Duration::from_millis(250));
            }
            Err(err) => return Err(err),
        }
    }
}
