use std::io;

use crate::fastboot::{Command, CommandContext, CommandResult, MAX_DOWNLOAD_SIZE};

use super::partitions;

const COMMAND_PREFIX: &str = "getvar:";
const FASTBOOT_VERSION: &str = "0.4";
const PRODUCT: &str = "pocketboot";

#[derive(Clone)]
pub(super) struct FastbootGetvar {
    serialno: String,
}

impl FastbootGetvar {
    pub(super) fn new(serialno: String) -> Self {
        Self { serialno }
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
        for (name, value) in self.static_variables() {
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
        if let Some(value) = self.static_value(variable) {
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
            return Ok(Some("no".to_string()));
        }

        Ok(None)
    }

    fn static_value(&self, variable: &str) -> Option<String> {
        self.static_variables()
            .into_iter()
            .find_map(|(name, value)| (name == variable).then_some(value))
    }

    fn static_variables(&self) -> Vec<(&'static str, String)> {
        vec![
            ("version", FASTBOOT_VERSION.to_string()),
            ("version-bootloader", PRODUCT.to_string()),
            ("product", PRODUCT.to_string()),
            ("serialno", self.serialno.clone()),
            ("secure", "no".to_string()),
            ("unlocked", "yes".to_string()),
            ("is-userspace", "yes".to_string()),
            ("max-download-size", format!("0x{MAX_DOWNLOAD_SIZE:08x}")),
            ("slot-count", "0".to_string()),
            ("current-slot", String::new()),
        ]
    }
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
        let getvar = FastbootGetvar::new("abc123".to_string());

        assert_eq!(getvar.static_value("serialno").as_deref(), Some("abc123"));
        assert_eq!(
            getvar.static_value("max-download-size").as_deref(),
            Some("0x10000000")
        );
    }
}
