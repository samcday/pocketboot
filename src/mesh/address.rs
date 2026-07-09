//! IPv6 address derivation and loopback configuration.
//!
//! PocketMesh uses a PocketBoot-specific ULA `/48` prefix derived from a
//! hardcoded domain separation string. Each node's `/128` is the ULA
//! prefix plus the last 80 bits of its identity hash.

use std::net::Ipv6Addr;

use sha2::{Digest, Sha256};

use super::identity::MeshIdentity;

const ULA_PREFIX_DOMAIN: &[u8] = b"pocketboot mesh ula prefix v1";

/// Build the node's ULA `/128` from a 32-byte identity seed.
///
/// The ULA `/48` prefix is `fd` + the first 40 bits of
/// `SHA256("pocketboot mesh ula prefix v1")`. The remaining 80 bits of
/// the address are the last 80 bits of the identity seed.
pub(crate) fn ula_address(seed: &[u8]) -> Ipv6Addr {
    let prefix = ula_prefix();
    let interface_id: [u8; 10] = seed[22..32].try_into().expect("32 bytes");

    let mut octets = [0u8; 16];
    octets[..6].copy_from_slice(&prefix);
    octets[6..].copy_from_slice(&interface_id);
    Ipv6Addr::from(octets)
}

/// PocketBoot ULA `/48` prefix (6 bytes, first byte is `0xfd`).
fn ula_prefix() -> [u8; 6] {
    let mut hasher = Sha256::new();
    hasher.update(ULA_PREFIX_DOMAIN);
    let hash = hasher.finalize();
    let mut prefix: [u8; 6] = hash[..6].try_into().expect("6 bytes");
    prefix[0] = 0xfd;
    prefix
}

/// Render the node ULA as a stable string with `/128`.
pub(crate) fn ula_cidr(addr: Ipv6Addr) -> String {
    format!("{addr}/128")
}

/// Render a link-local `fe80::<low64>/64` for the given identity.
pub(crate) fn link_local_cidr(identity: &MeshIdentity) -> String {
    let low = identity.interface_id_low64;
    format!(
        "fe80::{:x}:{:x}:{:x}:{:x}/64",
        u16::from_be_bytes([low[0], low[1]]),
        u16::from_be_bytes([low[2], low[3]]),
        u16::from_be_bytes([low[4], low[5]]),
        u16::from_be_bytes([low[6], low[7]]),
    )
}

/// Assign the node ULA `/128` to loopback.
///
/// "Already exists" errors are tolerated: the address may have been
/// configured by an earlier boot stage or a retry.
pub(crate) fn assign_loopback_ula(identity: &MeshIdentity) -> std::io::Result<()> {
    let cidr = ula_cidr(identity.ula_addr);
    match super::command::run_ip(["-6", "addr", "add", cidr.as_str(), "dev", "lo"]) {
        Ok(()) => {
            tracing::info!(addr = %cidr, "mesh loopback ULA assigned");
            Ok(())
        }
        Err(err) if err.is_already_exists() => {
            tracing::debug!(addr = %cidr, "mesh loopback ULA already assigned");
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::identity::derive_from_inputs;

    #[test]
    fn ula_rendering_is_stable() {
        let id = derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()]);
        let rendered = ula_cidr(id.ula_addr);
        let id2 = derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()]);
        assert_eq!(rendered, ula_cidr(id2.ula_addr));
        assert!(rendered.ends_with("/128"));
    }

    #[test]
    fn link_local_rendering_is_stable() {
        let id = derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()]);
        let rendered = link_local_cidr(&id);
        assert!(rendered.starts_with("fe80::"));
        assert!(rendered.ends_with("/64"));
    }

    #[test]
    fn empty_inputs_do_not_panic() {
        let id = derive_from_inputs("", "", &[]);
        let _ = ula_cidr(id.ula_addr);
        let _ = link_local_cidr(&id);
    }
}
