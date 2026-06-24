use crate::fastboot::{Command, CommandMap};

mod boot;
mod cat;
mod dmesg;
mod flash;
mod getvar;
mod partitions;
mod reboot;
mod shell;
mod slots;
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
        Command::prefix("oem shell:", shell::handle),
        Command::exact("oem shell-staged", shell::handle_staged),
    ]
}

pub(crate) fn getvar_commands(serialno: String, slots: crate::ab_slots::Slots) -> CommandMap {
    getvar::FastbootGetvar::new(serialno, slots).commands()
}

pub(crate) fn flash_commands() -> CommandMap {
    vec![
        Command::prefix("flash:", flash::handle),
        Command::prefix("erase:", flash::handle_erase),
    ]
}

pub(crate) fn slot_commands(slots: crate::ab_slots::Slots) -> CommandMap {
    slots::FastbootSlots::new(slots).commands()
}

pub(crate) fn reboot_command() -> Command {
    Command::exact("reboot", reboot::handle)
}

pub(crate) fn ums_commands(gadget: crate::gadget::Gadget) -> CommandMap {
    ums::FastbootUms::new(gadget).commands()
}
