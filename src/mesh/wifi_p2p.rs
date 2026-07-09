//! Wi-Fi P2P link backend (placeholder).
//!
//! This backend is intentionally stubbed for v0. A future implementation
//! should:
//!
//! * Detect WLAN interfaces.
//! * Bring firmware up if available.
//! * Use a small external `wpa_supplicant`/`wpa_cli` backend first, not a
//!   Rust P2P supplicant.
//! * Once a P2P group interface exists, treat it exactly like any other
//!   `MeshLink` and let Babel handle routing.
//!
//! `wpa_supplicant` is not added to the initrd in this task.

use super::link::MeshLink;

/// Discover Wi-Fi P2P mesh links.
///
/// Returns an empty list until a backend exists.
pub(crate) fn discover_links() -> Vec<MeshLink> {
    Vec::new()
}
