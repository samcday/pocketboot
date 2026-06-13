use std::collections::HashMap;

use crate::fastboot::{CommandHandler, CommandMap};

mod dmesg;
mod reboot;

pub(crate) fn default_commands() -> CommandMap {
    HashMap::from([
        ("oem dmesg", dmesg::handle as CommandHandler),
        ("reboot", reboot::handle as CommandHandler),
    ])
}
