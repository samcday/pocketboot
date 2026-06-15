use crate::fastboot::{Command, CommandHandler, CommandMap};

mod boot;
mod cat;
mod dmesg;
mod reboot;

pub(crate) fn default_commands() -> CommandMap {
    vec![
        Command::exact("boot", boot::handle_boot as CommandHandler),
        Command::prefix("oem cat:", cat::handle as CommandHandler),
        Command::exact("oem dmesg", dmesg::handle as CommandHandler),
        Command::exact("oem kexec-load", boot::handle_kexec_load as CommandHandler),
        Command::exact(
            "oem kexec-status",
            boot::handle_kexec_status as CommandHandler,
        ),
        Command::exact("reboot", reboot::handle as CommandHandler),
    ]
}
