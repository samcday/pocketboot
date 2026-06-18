use std::{fs, io, path::Path};

#[derive(Clone, Debug, Default)]
pub(crate) struct KernelCommandLine {
    args: Vec<String>,
}

impl KernelCommandLine {
    pub(crate) fn read(path: impl AsRef<Path>) -> io::Result<Self> {
        fs::read_to_string(path).map(|contents| Self::parse(&contents))
    }

    pub(crate) fn parse(contents: &str) -> Self {
        Self {
            args: contents.split_whitespace().map(str::to_string).collect(),
        }
    }

    pub(crate) fn value(&self, key: &str) -> Option<&str> {
        self.args.iter().find_map(|arg| {
            let value = arg.strip_prefix(key)?.strip_prefix('=')?;
            (!value.is_empty()).then_some(value)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_values() {
        let cmdline = KernelCommandLine::parse("foo androidboot.serialno=6ea45af6 empty= bar");

        assert_eq!(cmdline.value("androidboot.serialno"), Some("6ea45af6"));
        assert_eq!(cmdline.value("empty"), None);
        assert_eq!(cmdline.value("missing"), None);
    }
}
