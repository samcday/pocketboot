use std::io;

use crate::fastboot::{CommandContext, CommandResult};
use crate::power;

pub(super) fn handle(
    context: &mut CommandContext<'_>,
    _command: &str,
) -> io::Result<CommandResult> {
    context.okay_then_exit(b"rebooting", power::reboot)
}
