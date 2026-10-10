use std::{fs, io, path::Path};

const USB_ROLE_PARAM: &str = "pocketboot.usb_role";

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
        self.values(key).next()
    }

    pub(crate) fn values<'a>(&'a self, key: &str) -> impl Iterator<Item = &'a str> + 'a {
        let key = key.to_string();
        self.args.iter().filter_map(move |arg| {
            let value = arg.strip_prefix(key.as_str())?.strip_prefix('=')?;
            (!value.is_empty()).then_some(value)
        })
    }

    pub(crate) fn is_set(&self, key: &str) -> bool {
        self.args.iter().any(|arg| arg == key)
    }

    pub(crate) fn usb_device_role(&self) -> Result<bool, String> {
        // values() omits empty values, but these must not silently opt out.
        if self.is_set(USB_ROLE_PARAM) || self.is_set(&format!("{USB_ROLE_PARAM}=")) {
            return Err(format!("{USB_ROLE_PARAM} requires the value 'device'"));
        }
        let mut requested = false;
        for value in self.values(USB_ROLE_PARAM) {
            if value != "device" {
                return Err(format!(
                    "unsupported {USB_ROLE_PARAM}={value}; only 'device' is supported"
                ));
            }
            requested = true;
        }
        Ok(requested)
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

    #[test]
    fn detects_bare_flags() {
        let cmdline = KernelCommandLine::parse("foo pocketboot.acm pocketboot.acm.extra=1 bar");

        assert!(cmdline.is_set("pocketboot.acm"));
        assert!(!cmdline.is_set("pocketboot.acm.extra"));
        assert!(!cmdline.is_set("missing"));
    }

    #[test]
    fn returns_repeated_key_values() {
        let cmdline = KernelCommandLine::parse("console=tty0 console=ttyS0,115200 empty=");

        assert_eq!(
            cmdline.values("console").collect::<Vec<_>>(),
            vec!["tty0", "ttyS0,115200"]
        );
        assert_eq!(
            cmdline.values("empty").collect::<Vec<_>>(),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn usb_device_role_requires_explicit_opt_in() {
        for contents in [
            "",
            "pocketboot.usb_role.extra=device",
            "other.usb_role=device",
        ] {
            assert!(
                !KernelCommandLine::parse(contents)
                    .usb_device_role()
                    .unwrap()
            );
        }
        for contents in [
            "foo pocketboot.usb_role=device bar",
            "pocketboot.usb_role=device pocketboot.usb_role=device",
        ] {
            assert!(
                KernelCommandLine::parse(contents)
                    .usb_device_role()
                    .unwrap()
            );
        }
    }

    #[test]
    fn usb_device_role_rejects_unsupported_or_missing_values() {
        for contents in [
            "pocketboot.usb_role=host",
            "pocketboot.usb_role=none",
            "pocketboot.usb_role=otg",
            "pocketboot.usb_role=Device",
            "pocketboot.usb_role",
            "pocketboot.usb_role=",
            "pocketboot.usb_role=device pocketboot.usb_role=host",
            "pocketboot.usb_role=host pocketboot.usb_role=device",
            "pocketboot.usb_role=device pocketboot.usb_role=",
        ] {
            let err = KernelCommandLine::parse(contents)
                .usb_device_role()
                .unwrap_err();
            assert!(err.contains(USB_ROLE_PARAM), "{contents}: {err}");
        }
    }
}
