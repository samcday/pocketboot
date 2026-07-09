//! Mesh link abstraction.
//!
//! Each mesh-bearing interface is represented as a `MeshLink`. The first
//! real backend is USB gadget Ethernet; Wi-Fi P2P is a later backend that
//! plugs into the same abstraction.

use std::io;

use super::address;
use super::command;
use super::identity::MeshIdentity;

pub(crate) const DEFAULT_WIRED_METRIC: u16 = 96;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MeshLink {
    pub(crate) name: String,
    pub(crate) kind: MeshLinkKind,
    pub(crate) metric_hint: u16,
    pub(crate) wired: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // WifiP2p/BluetoothPan are future backends.
pub(crate) enum MeshLinkKind {
    UsbGadget,
    WifiP2p,
    BluetoothPan,
    Manual,
}

impl MeshLinkKind {
    pub(crate) fn babel_type(&self) -> &'static str {
        match self {
            Self::UsbGadget | Self::Manual => "wired",
            Self::WifiP2p => "wireless",
            Self::BluetoothPan => "tunnel",
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::UsbGadget => "usb-gadget",
            Self::WifiP2p => "wifi-p2p",
            Self::BluetoothPan => "bluetooth-pan",
            Self::Manual => "manual",
        }
    }
}

/// Bring a link up and assign a deterministic link-local IPv6 address.
pub(crate) fn bring_up(link: &MeshLink, identity: &MeshIdentity) -> io::Result<()> {
    command::run_ip(["link", "set", "dev", link.name.as_str(), "up"])?;

    let ll = address::link_local_cidr(identity);
    match command::run_ip(["-6", "addr", "add", ll.as_str(), "dev", link.name.as_str()]) {
        Ok(()) => {
            tracing::info!(
                ifname = %link.name,
                kind = link.kind.label(),
                link_local = %ll,
                "mesh link up"
            );
            Ok(())
        }
        Err(err) if err.is_already_exists() => {
            tracing::info!(
                ifname = %link.name,
                kind = link.kind.label(),
                "mesh link up (link-local already present)"
            );
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn babel_type_maps_kinds() {
        assert_eq!(MeshLinkKind::UsbGadget.babel_type(), "wired");
        assert_eq!(MeshLinkKind::Manual.babel_type(), "wired");
        assert_eq!(MeshLinkKind::WifiP2p.babel_type(), "wireless");
        assert_eq!(MeshLinkKind::BluetoothPan.babel_type(), "tunnel");
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(MeshLinkKind::UsbGadget.label(), "usb-gadget");
        assert_eq!(MeshLinkKind::Manual.label(), "manual");
    }
}
