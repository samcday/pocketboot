use crate::fastboot::{Command, CommandMap};

mod boot;
mod cat;
mod dmesg;
mod reboot;
mod ums;

pub(crate) fn boot_commands() -> CommandMap {
    vec![
        Command::exact("boot", boot::handle_boot),
        Command::exact("oem kexec-load", boot::handle_kexec_load),
        Command::exact("oem kexec-status", boot::handle_kexec_status),
    ]
}

pub(crate) fn diagnostic_commands() -> CommandMap {
    vec![
        Command::prefix("oem cat:", cat::handle),
        Command::exact("oem dmesg", dmesg::handle),
    ]
}

pub(crate) fn reboot_command() -> Command {
    Command::exact("reboot", reboot::handle)
}

pub(crate) fn ums_commands(gadget: crate::gadget::Gadget) -> CommandMap {
    ums::FastbootUms::new(gadget).commands()
}
