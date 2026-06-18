use std::io;

const LINUX_REBOOT_CMD_RESTART: libc::c_int = 0x0123_4567;
const LINUX_REBOOT_CMD_POWER_OFF: libc::c_int = 0x4321_fedc_u32 as libc::c_int;

pub(crate) fn reboot() -> io::Result<()> {
    reboot_command(LINUX_REBOOT_CMD_RESTART, "rebooting system")
}

pub(crate) fn power_off() -> io::Result<()> {
    reboot_command(LINUX_REBOOT_CMD_POWER_OFF, "powering off system")
}

fn reboot_command(command: libc::c_int, message: &str) -> io::Result<()> {
    tracing::info!("{message}");
    let rc = unsafe { libc::reboot(command) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    Err(io::Error::other("reboot returned unexpectedly"))
}
