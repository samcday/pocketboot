use std::io;

use crate::{
    ab_slots::{Slot, Slots},
    fastboot::{Command, CommandContext, CommandResult},
};

const SET_ACTIVE_PREFIX: &str = "set_active:";

#[derive(Clone)]
pub(super) struct FastbootSlots {
    slots: Slots,
}

impl FastbootSlots {
    pub(super) fn new(slots: Slots) -> Self {
        Self { slots }
    }

    pub(super) fn commands(self) -> Vec<Command> {
        vec![Command::prefix(
            SET_ACTIVE_PREFIX,
            move |context: &mut CommandContext<'_>, command: &str| {
                self.handle_set_active(context, command)
            },
        )]
    }

    fn handle_set_active(
        &self,
        context: &mut CommandContext<'_>,
        command: &str,
    ) -> io::Result<CommandResult> {
        let slot = parse_set_active(command)?;
        self.slots.set_active(slot)?;
        context.okay(format!("slot {} is active", slot.name()))?;
        Ok(CommandResult::continue_())
    }
}

fn parse_set_active(command: &str) -> io::Result<Slot> {
    let slot = command
        .strip_prefix(SET_ACTIVE_PREFIX)
        .ok_or_else(|| invalid_input("invalid set_active command"))?;
    if slot.is_empty() {
        return Err(invalid_input("set_active slot is empty"));
    }
    Slot::parse(slot).ok_or_else(|| invalid_input(format!("invalid slot {slot:?}")))
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_set_active_slot() {
        assert_eq!(parse_set_active("set_active:a").unwrap(), Slot::A);
        assert_eq!(parse_set_active("set_active:_b").unwrap(), Slot::B);
    }

    #[test]
    fn rejects_empty_set_active_slot() {
        let err = parse_set_active("set_active:").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
