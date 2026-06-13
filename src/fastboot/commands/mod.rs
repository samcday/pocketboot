use std::collections::HashMap;

use crate::fastboot::{CommandHandler, CommandMap};

mod boot;
mod dmesg;
mod reboot;

pub(crate) fn default_commands() -> CommandMap {
    HashMap::from([
        ("boot", boot::handle_boot as CommandHandler),
        ("oem dmesg", dmesg::handle as CommandHandler),
        ("oem kexec-load", boot::handle_kexec_load as CommandHandler),
        (
            "oem kexec-status",
            boot::handle_kexec_status as CommandHandler,
        ),
        ("reboot", reboot::handle as CommandHandler),
    ])
}
