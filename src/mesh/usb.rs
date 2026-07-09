//! USB gadget Ethernet (NCM/ECM) function composition.
//!
//! This module creates a configfs network function under the existing
//! PocketBoot gadget path and symlinks it into the active config. It is
//! invoked by the gadget thread before UDC bind so the kernel creates the
//! netdev when the gadget enumerates.
//!
//! Preferred function: NCM. Fallback: ECM. RNDIS is intentionally not
//! used unless explicitly requested later.

use std::{fs, io, os::unix::fs::symlink, path::Path, thread, time::Duration};

const NCM_FUNCTION: &str = "ncm.pocketmesh";
const ECM_FUNCTION: &str = "ecm.pocketmesh";
const FUNCTIONS_DIR: &str = "functions";
const CONFIG_DIR: &str = "configs/c.1";
const DEFAULT_IFNAME: &str = "pbmesh0";
const NETDEV_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const NETDEV_POLL_INTERVAL: Duration = Duration::from_millis(250);
const SYS_CLASS_NET: &str = "/sys/class/net";

/// Configuration for the USB gadget Ethernet function.
#[derive(Clone, Debug)]
pub(crate) struct UsbNetConfig {
    /// Requested interface name. The kernel may ignore/rename it.
    pub(crate) ifname: String,
    /// Device-side MAC (locally administered unicast).
    pub(crate) dev_addr: [u8; 6],
    /// Host-side MAC (locally administered unicast, differs from dev_addr).
    pub(crate) host_addr: [u8; 6],
    /// Prefer NCM; fall back to ECM when NCM is unavailable.
    pub(crate) prefer_ncm: bool,
}

impl UsbNetConfig {
    /// Build a config from mesh identity using the default ifname.
    pub(crate) fn from_identity(dev_mac: [u8; 6], host_mac: [u8; 6]) -> Self {
        Self {
            ifname: DEFAULT_IFNAME.to_string(),
            dev_addr: dev_mac,
            host_addr: host_mac,
            prefer_ncm: true,
        }
    }

    fn dev_addr_string(&self) -> String {
        mac_string(&self.dev_addr)
    }

    fn host_addr_string(&self) -> String {
        mac_string(&self.host_addr)
    }
}

/// Result of attempting to create the USB net function.
#[derive(Debug)]
pub(crate) enum UsbNetStart {
    /// Function was created and symlinked into the config.
    Started { ifname: String },
    /// A matching function already existed in this gadget.
    AlreadyPresent { ifname: String },
    /// Neither NCM nor ECM function support is present on this kernel.
    Unsupported,
}

/// Create the USB net configfs function under `gadget_path` and symlink it
/// into the active config. Call this before binding the UDC.
pub(crate) fn create_function(
    gadget_path: &Path,
    config: &UsbNetConfig,
) -> io::Result<UsbNetStart> {
    let candidates: &[&str] = if config.prefer_ncm {
        &[NCM_FUNCTION, ECM_FUNCTION]
    } else {
        &[ECM_FUNCTION, NCM_FUNCTION]
    };

    for &function in candidates {
        let function_dir = gadget_path.join(FUNCTIONS_DIR).join(function);
        if function_dir.exists() {
            tracing::debug!(path = %function_dir.display(), "usb net function already present");
            return Ok(UsbNetStart::AlreadyPresent {
                ifname: config.ifname.clone(),
            });
        }
        match try_create_function(gadget_path, function, config) {
            Ok(()) => {
                tracing::info!(
                    function,
                    ifname = %config.ifname,
                    dev_addr = %config.dev_addr_string(),
                    host_addr = %config.host_addr_string(),
                    "usb net function created"
                );
                return Ok(UsbNetStart::Started {
                    ifname: config.ifname.clone(),
                });
            }
            Err(err) => {
                tracing::debug!(
                    function,
                    error = ?err,
                    "usb net function unavailable, trying fallback"
                );
            }
        }
    }

    tracing::warn!("usb net function unsupported (no NCM/ECM available)");
    Ok(UsbNetStart::Unsupported)
}

fn try_create_function(
    gadget_path: &Path,
    function: &str,
    config: &UsbNetConfig,
) -> io::Result<()> {
    let function_dir = gadget_path.join(FUNCTIONS_DIR).join(function);
    fs::create_dir(&function_dir)?;
    let result = write_optional(&function_dir.join("ifname"), config.ifname.as_bytes())
        .and_then(|()| {
            write_optional(
                &function_dir.join("dev_addr"),
                config.dev_addr_string().as_bytes(),
            )
        })
        .and_then(|()| {
            write_optional(
                &function_dir.join("host_addr"),
                config.host_addr_string().as_bytes(),
            )
        })
        .and_then(|()| write_optional(&function_dir.join("qmult"), b"5"))
        .and_then(|()| {
            let config_link = gadget_path.join(CONFIG_DIR).join(function);
            symlink(&function_dir, &config_link)
        });
    if result.is_err() {
        let _ = fs::remove_dir(&function_dir);
    }
    result
}

fn write_optional(path: &Path, value: &[u8]) -> io::Result<()> {
    match fs::write(path, value) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Wait for a netdev whose address matches `mac` to appear under
/// `/sys/class/net`. Returns the interface name, or `None` on timeout.
///
/// The kernel may rename the requested `ifname`, so discovery is by MAC.
pub(crate) fn wait_for_netdev(mac: &[u8; 6], timeout: Duration) -> io::Result<Option<String>> {
    let target = mac_string(mac);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(ifname) = find_netdev_by_mac(&target)? {
            return Ok(Some(ifname));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(NETDEV_POLL_INTERVAL);
    }
}

/// Convenience wrapper using the default wait timeout.
pub(crate) fn wait_for_netdev_default(mac: &[u8; 6]) -> io::Result<Option<String>> {
    wait_for_netdev(mac, NETDEV_WAIT_TIMEOUT)
}

fn find_netdev_by_mac(target_mac: &str) -> io::Result<Option<String>> {
    let entries = match fs::read_dir(SYS_CLASS_NET) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(name) => name.to_string(),
            None => continue,
        };
        let mac_path = entry.path().join("address");
        if let Ok(mac) = fs::read_to_string(&mac_path)
            && mac.trim().eq_ignore_ascii_case(target_mac)
        {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

pub(crate) fn mac_string(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Remove a previously-created USB net function. Best-effort.
#[allow(dead_code)]
pub(crate) fn remove_function(gadget_path: &Path) -> io::Result<()> {
    for function in [NCM_FUNCTION, ECM_FUNCTION] {
        let config_link = gadget_path.join(CONFIG_DIR).join(function);
        let _ = fs::remove_file(&config_link);
        let function_dir = gadget_path.join(FUNCTIONS_DIR).join(function);
        let _ = fs::remove_dir(&function_dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_string_format() {
        let config = UsbNetConfig {
            ifname: "pbmesh0".into(),
            dev_addr: [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
            host_addr: [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee],
            prefer_ncm: true,
        };
        assert_eq!(config.dev_addr_string(), "02:11:22:33:44:55");
        assert_eq!(config.host_addr_string(), "02:aa:bb:cc:dd:ee");
    }

    #[test]
    fn from_identity_uses_default_ifname() {
        let config = UsbNetConfig::from_identity(
            [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
            [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee],
        );
        assert_eq!(config.ifname, "pbmesh0");
        assert!(config.prefer_ncm);
    }
}
