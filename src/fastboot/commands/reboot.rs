use std::io;

use crate::fastboot::{CommandContext, CommandResult};

const LINUX_REBOOT_CMD_RESTART: libc::c_int = 0x0123_4567;

pub(super) fn handle(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<CommandResult> {
    context.okay_then_exit(b"rebooting", reboot_system)
}

fn reboot_system() -> io::Result<()> {
    tracing::info!("rebooting system");
    let rc = unsafe { libc::reboot(LINUX_REBOOT_CMD_RESTART) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    Err(io::Error::other("reboot returned unexpectedly"))
}
