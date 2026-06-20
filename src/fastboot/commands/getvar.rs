use std::{fs, io};

use crate::{
    ab_slots::{Slot, Slots},
    fastboot::{Command, CommandContext, CommandResult, MAX_DOWNLOAD_SIZE},
};

use super::partitions;

const COMMAND_PREFIX: &str = "getvar:";
const FDT_BASE_COMPATIBLE: &str = "/sys/firmware/devicetree/base/compatible";
const FASTBOOT_VERSION: &str = "0.4";
const PRODUCT: &str = "pocketboot";

#[derive(Clone)]
pub(super) struct FastbootGetvar {
    serialno: String,
    slots: Slots,
}

impl FastbootGetvar {
    pub(super) fn new(serialno: String, slots: Slots) -> Self {
        Self { serialno, slots }
    }

    pub(super) fn commands(self) -> Vec<Command> {
        vec![Command::prefix(
            COMMAND_PREFIX,
            move |context: &mut CommandContext<'_>, command: &str| self.handle(context, command),
        )]
    }

    fn handle(&self, context: &mut CommandContext<'_>, command: &str) -> io::Result<CommandResult> {
        let variable = parse_variable(command)?;
        if variable == "all" {
            self.send_all(context)?;
            context.okay(b"")?;
            return Ok(CommandResult::continue_());
        }

        let value = self.value(variable)?.unwrap_or_default();
        context.okay(value)?;
        Ok(CommandResult::continue_())
    }

    fn send_all(&self, context: &mut CommandContext<'_>) -> io::Result<()> {
        for (name, value) in self.fixed_variables() {
            context.info(format!("{name}: {value}"))?;
        }
        for (name, value) in self.slot_variables()? {
            context.info(format!("{name}: {value}"))?;
        }

        for partition in partitions::list()? {
            let name = partition.fastboot_name();
            context.info(format!(
                "partition-size:{name}: 0x{:x}",
                partition.size_bytes
            ))?;
            context.info(format!("partition-type:{name}: raw"))?;
        }

        Ok(())
    }

    fn value(&self, variable: &str) -> io::Result<Option<String>> {
        if let Some(value) = self.fixed_value(variable) {
            return Ok(Some(value));
        }

        if let Some(value) = self.slot_value(variable)? {
            return Ok(Some(value));
        }

        if let Some(partition) = variable.strip_prefix("partition-size:") {
            let partition = find_partition_for_getvar(partition)?;
            return Ok(partition.map(|partition| format!("0x{:x}", partition.size_bytes)));
        }

        if let Some(partition) = variable.strip_prefix("partition-type:") {
            return Ok(find_partition_for_getvar(partition)?.map(|_| "raw".to_string()));
        }

        if let Some(partition) = variable.strip_prefix("has-slot:") {
            if partition.is_empty() {
                return Err(invalid_input("partition name is empty"));
            }
            return Ok(Some(if self.slots.has_slot(partition)? {
                "yes".to_string()
            } else {
                "no".to_string()
            }));
        }

        Ok(None)
    }

    fn fixed_value(&self, variable: &str) -> Option<String> {
        self.fixed_variables()
            .into_iter()
            .find_map(|(name, value)| (name == variable).then_some(value))
    }

    fn fixed_variables(&self) -> Vec<(&'static str, String)> {
        let mut variables = vec![
            ("version", FASTBOOT_VERSION.to_string()),
            ("version-bootloader", PRODUCT.to_string()),
            ("product", PRODUCT.to_string()),
            ("serialno", self.serialno.clone()),
            ("secure", "no".to_string()),
            ("unlocked", "yes".to_string()),
            ("is-userspace", "yes".to_string()),
            ("max-download-size", format!("0x{MAX_DOWNLOAD_SIZE:08x}")),
        ];
        if let Some(compatible) = fdt_base_compatible() {
            variables.push(("compatible", compatible));
        }
        variables
    }

    fn slot_value(&self, variable: &str) -> io::Result<Option<String>> {
        Ok(match variable {
            "slot-count" => Some(self.slots.slot_count()?.to_string()),
            "current-slot" => Some(
                self.slots
                    .current_slot()?
                    .map(Slot::name)
                    .unwrap_or_default()
                    .to_string(),
            ),
            "slot-suffixes" => Some(
                if self.slots.slot_count()? > 0 {
                    "_a,_b"
                } else {
                    ""
                }
                .to_string(),
            ),
            _ => {
                if let Some(slot) = slot_variable(variable, "slot-successful:")? {
                    Some(slot_bool(self.slots.is_slot_successful(slot)?))
                } else if let Some(slot) = slot_variable(variable, "slot-unbootable:")? {
                    Some(slot_bool(self.slots.is_slot_unbootable(slot)?))
                } else {
                    None
                }
            }
        })
    }

    fn slot_variables(&self) -> io::Result<Vec<(&'static str, String)>> {
        let slot_count = self.slots.slot_count()?;
        let current_slot = self
            .slots
            .current_slot()?
            .map(Slot::name)
            .unwrap_or_default()
            .to_string();
        let slot_suffixes = if slot_count > 0 { "_a,_b" } else { "" }.to_string();

        Ok(vec![
            ("slot-count", slot_count.to_string()),
            ("current-slot", current_slot),
            ("slot-suffixes", slot_suffixes),
            (
                "slot-successful:a",
                slot_bool(self.slots.is_slot_successful(Slot::A)?),
            ),
            (
                "slot-successful:b",
                slot_bool(self.slots.is_slot_successful(Slot::B)?),
            ),
            (
                "slot-unbootable:a",
                slot_bool(self.slots.is_slot_unbootable(Slot::A)?),
            ),
            (
                "slot-unbootable:b",
                slot_bool(self.slots.is_slot_unbootable(Slot::B)?),
            ),
        ])
    }
}

fn slot_variable(variable: &str, prefix: &str) -> io::Result<Option<Slot>> {
    let Some(value) = variable.strip_prefix(prefix) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(invalid_input("slot variable slot is empty"));
    }
    Slot::parse(value)
        .map(Some)
        .ok_or_else(|| invalid_input(format!("invalid slot {value:?}")))
}

fn slot_bool(value: Option<bool>) -> String {
    value
        .map(|value| if value { "yes" } else { "no" })
        .unwrap_or("")
        .to_string()
}

fn fdt_base_compatible() -> Option<String> {
    let bytes = fs::read(FDT_BASE_COMPATIBLE).ok()?;
    parse_fdt_base_compatible(&bytes).map(str::to_string)
}

fn parse_fdt_base_compatible(bytes: &[u8]) -> Option<&str> {
    bytes
        .split(|byte| *byte == b'\0')
        .find(|value| !value.is_empty())
        .and_then(|value| std::str::from_utf8(value).ok())
}

fn parse_variable(command: &str) -> io::Result<&str> {
    let variable = command
        .strip_prefix(COMMAND_PREFIX)
        .ok_or_else(|| invalid_input("invalid getvar command"))?;
    if variable.is_empty() {
        return Err(invalid_input("getvar variable is empty"));
    }
    Ok(variable)
}

fn find_partition_for_getvar(name: &str) -> io::Result<Option<partitions::Partition>> {
    if name.is_empty() {
        return Err(invalid_input("partition name is empty"));
    }

    match partitions::find(name) {
        Ok(partition) => Ok(Some(partition)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_getvar_name() {
        assert_eq!(parse_variable("getvar:version").unwrap(), "version");
    }

    #[test]
    fn rejects_empty_getvar_name() {
        let err = parse_variable("getvar:").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn returns_static_values() {
        let getvar = FastbootGetvar::new(
            "abc123".to_string(),
            Slots::new(crate::cmdline::KernelCommandLine::default()),
        );

        assert_eq!(getvar.fixed_value("serialno").as_deref(), Some("abc123"));
        assert_eq!(
            getvar.fixed_value("max-download-size").as_deref(),
            Some("0x10000000")
        );
    }

    #[test]
    fn parses_fdt_base_compatible() {
        assert_eq!(
            parse_fdt_base_compatible(b"oneplus,fajita\0qcom,sdm845\0"),
            Some("oneplus,fajita")
        );
    }

    #[test]
    fn ignores_empty_fdt_base_compatible() {
        assert_eq!(parse_fdt_base_compatible(b"\0"), None);
    }

    #[test]
    fn ignores_invalid_fdt_base_compatible() {
        assert_eq!(parse_fdt_base_compatible(b"\xff\0qcom,sdm845\0"), None);
    }
}
