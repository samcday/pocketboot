//! Deterministic mesh node identity.
//!
//! PocketBoot may not have persistent writable storage, so v0 identity is
//! deterministic across boots. The raw device serial is never used as the
//! network-visible identity; it is only one input to a SHA-256 derivation
//! that also mixes in FDT model/compatible strings and a hardcoded domain
//! separation string.

use std::fs;
use std::net::Ipv6Addr;

use sha2::{Digest, Sha256};

use super::address;

const IDENTITY_DOMAIN: &[u8] = b"pocketboot mesh identity v1";

const FDT_MODEL_PATH: &str = "/sys/firmware/devicetree/base/model";
const FDT_COMPATIBLE_PATH: &str = "/sys/firmware/devicetree/base/compatible";

/// Stable mesh identity for this node.
///
/// All fields are derived from the identity hash, not from raw boot
/// signals. The raw serial is intentionally not exposed here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MeshIdentity {
    /// Hex-encoded node identifier (first 16 bytes of the identity hash).
    pub(crate) node_id_hex: String,
    /// Stable ULA `/128` assigned to loopback.
    pub(crate) ula_addr: Ipv6Addr,
    /// Locally-administered unicast MAC for the USB gadget device side.
    pub(crate) usb_dev_mac: [u8; 6],
    /// Locally-administered unicast MAC for the USB gadget host side.
    pub(crate) usb_host_mac: [u8; 6],
    /// Low 64 bits of the identity hash, used for link-local interface ids.
    pub(crate) interface_id_low64: [u8; 8],
}

impl MeshIdentity {
    /// Derive mesh identity from the detected serial and FDT signals.
    pub(crate) fn derive(serialno: &str) -> Self {
        let model = read_fdt_string(FDT_MODEL_PATH).unwrap_or_default();
        let compatibles = read_fdt_strings(FDT_COMPATIBLE_PATH);
        derive_from_inputs(serialno, &model, &compatibles)
    }
}

/// Pure identity derivation used by tests.
pub(crate) fn derive_from_inputs(
    serialno: &str,
    model: &str,
    compatibles: &[String],
) -> MeshIdentity {
    let mut hasher = Sha256::new();
    hasher.update(IDENTITY_DOMAIN);
    hasher.update([0]);
    hasher.update(serialno.as_bytes());
    hasher.update([0]);
    hasher.update(model.as_bytes());
    hasher.update([0]);
    let joined = compatibles.join(",");
    hasher.update(joined.as_bytes());
    hasher.update([0]);
    let seed = hasher.finalize();

    let node_id_hex = hex_encode(&seed[..16]);
    let ula_addr = address::ula_address(&seed);
    let interface_id_low64 = seed[24..32].try_into().expect("32 bytes");
    let usb_dev_mac = locally_administered_mac(&seed[8..14]);
    let usb_host_mac = locally_administered_mac(&seed[18..24]);

    MeshIdentity {
        node_id_hex,
        ula_addr,
        usb_dev_mac,
        usb_host_mac,
        interface_id_low64,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Mark a MAC as locally administered unicast: set the local bit and clear
/// the multicast bit of the first octet.
fn locally_administered_mac(bytes: &[u8]) -> [u8; 6] {
    let mut mac: [u8; 6] = bytes.try_into().expect("6 bytes");
    mac[0] = (mac[0] | 0x02) & 0xfe;
    mac
}

fn read_fdt_string(path: &str) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let value = bytes
        .split(|byte| *byte == 0)
        .find(|value| !value.is_empty())?;
    let trimmed = value
        .iter()
        .copied()
        .skip_while(|byte| byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    let trimmed = trimmed
        .iter()
        .rev()
        .skip_while(|byte| byte.is_ascii_whitespace())
        .copied()
        .collect::<Vec<_>>();
    std::str::from_utf8(&trimmed).ok().map(str::to_string)
}

fn read_fdt_strings(path: &str) -> Vec<String> {
    fs::read(path)
        .ok()
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|value| !value.is_empty())
                .filter_map(|value| std::str::from_utf8(value).ok())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_inputs_produce_same_identity() {
        let a = derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()]);
        let b = derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()]);
        assert_eq!(a, b);
    }

    #[test]
    fn different_serials_produce_different_node_ids() {
        let a = derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()]);
        let b = derive_from_inputs("def456", "Phone X", &["qcom,foo".to_string()]);
        assert_ne!(a.node_id_hex, b.node_id_hex);
        assert_ne!(a.ula_addr, b.ula_addr);
    }

    #[test]
    fn ula_is_inside_fd00_8() {
        let id = derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()]);
        let octets = id.ula_addr.octets();
        assert_eq!(octets[0], 0xfd, "ULA must start with fd");
        assert_eq!(octets[0] & 0xfe, 0xfc, "first octet high bits");
    }

    #[test]
    fn macs_are_locally_administered_unicast() {
        let id = derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()]);
        for mac in [id.usb_dev_mac, id.usb_host_mac] {
            assert_eq!(mac[0] & 0x01, 0, "multicast bit must be clear");
            assert_eq!(mac[0] & 0x02, 0x02, "local bit must be set");
        }
    }

    #[test]
    fn device_and_host_macs_differ() {
        let id = derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()]);
        assert_ne!(id.usb_dev_mac, id.usb_host_mac);
    }

    #[test]
    fn raw_serial_is_not_the_node_id() {
        let id = derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()]);
        assert_ne!(id.node_id_hex, "abc123");
        assert!(!id.node_id_hex.contains("abc123"));
    }

    #[test]
    fn empty_compatibles_does_not_panic() {
        let id = derive_from_inputs("abc123", "", &[]);
        assert!(!id.node_id_hex.is_empty());
    }
}
