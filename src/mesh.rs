//! Optional PocketMesh substrate.
//!
//! When the `mesh` feature is enabled, PocketBoot derives a stable local
//! mesh identity, assigns a ULA `/128` to loopback, brings up configured
//! mesh-capable netdevs, and (when `babeld` is present) runs a tiny
//! routing process over them.
//!
//! Mesh startup is non-fatal: failures are logged and normal boot
//! discovery/kexec behavior continues. The whole subsystem can be
//! disabled from the kernel command line with `pocketboot.mesh=0`.
//!
//! See `docs/mesh.md` for the full design and current limitations.

pub(crate) mod address;
pub(crate) mod babel;
pub(crate) mod command;
pub(crate) mod identity;
pub(crate) mod link;
pub(crate) mod status;
#[cfg(feature = "mesh-usb-net")]
pub(crate) mod usb;
#[cfg(feature = "mesh-wifi-p2p")]
pub(crate) mod wifi_p2p;

use std::thread;

use crate::cmdline::KernelCommandLine;

pub(crate) use identity::MeshIdentity;

/// Kernel command-line flag that disables mesh even when compiled in.
#[allow(dead_code)] // referenced via cmdline.value in main.rs
pub(crate) const DISABLE_CMDLINE_PARAM: &str = "pocketboot.mesh";

/// Optional list of mesh interfaces requested on the kernel command line.
const IFACES_CMDLINE_PARAM: &str = "pocketboot.mesh.ifaces";

/// Configuration assembled from boot-time signals.
#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) identity: MeshIdentity,
    /// Interface names explicitly requested via `pocketboot.mesh.ifaces`.
    pub(crate) manual_ifaces: Vec<String>,
    /// True when the USB gadget Ethernet backend should compose a function.
    pub(crate) usb_net_enabled: bool,
}

impl Config {
    /// Derive mesh configuration from boot-time signals.
    ///
    /// `serialno` is the already-detected PocketBoot serial. It is used as
    /// an input to identity derivation but is never used directly as the
    /// network-visible node identity.
    pub(crate) fn from_boot(serialno: &str, cmdline: &KernelCommandLine) -> Self {
        let identity = MeshIdentity::derive(serialno);
        let manual_ifaces = cmdline
            .value(IFACES_CMDLINE_PARAM)
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let usb_net_enabled = if cfg!(feature = "mesh-usb-net") {
            !cmdline.is_set("pocketboot.mesh.usb=0")
        } else {
            false
        };
        Self {
            identity,
            manual_ifaces,
            usb_net_enabled,
        }
    }
}

/// Handle to a running mesh subsystem.
///
/// The handle keeps the supervisor thread alive. Dropping it does not
/// currently stop the subsystem; for v0 the supervisor runs until
/// PocketBoot kexecs away.
pub(crate) struct Handle {
    _thread: thread::JoinHandle<()>,
}

/// Start the mesh subsystem.
///
/// This spawns a supervisor thread and returns immediately. Mesh startup
/// failures are logged from within the supervisor and do not propagate:
/// PocketBoot must remain able to boot the real OS even if mesh is broken.
pub(crate) fn spawn(config: Config) -> std::io::Result<Handle> {
    let thread = thread::Builder::new()
        .name("pocketboot-mesh".to_string())
        .spawn(move || run(config))?;
    tracing::info!("mesh subsystem supervisor spawned");
    Ok(Handle { _thread: thread })
}

fn run(config: Config) {
    tracing::info!(
        node_id = %config.identity.node_id_hex,
        ula = %config.identity.ula_addr,
        "mesh identity selected"
    );

    if config.usb_net_enabled {
        tracing::info!("mesh usb net backend enabled");
    } else {
        tracing::info!("mesh usb net backend disabled");
    }

    if !config.manual_ifaces.is_empty() {
        tracing::info!(
            ifaces = config.manual_ifaces.join(","),
            "mesh manual interfaces requested"
        );
    }

    // Bring up loopback ULA first so the node is always locally reachable
    // even if every link backend fails.
    if let Err(err) = address::assign_loopback_ula(&config.identity) {
        tracing::warn!(error = %err, "mesh failed to assign loopback ULA");
    }

    // Collect known mesh links. USB gadget link discovery is performed by
    // the gadget thread (it owns configfs); the runtime learns the
    // resulting ifname by scanning /sys/class/net for the configured MAC.
    // Manual interfaces are added directly here.
    let mut links = Vec::new();

    #[cfg(feature = "mesh-usb-net")]
    if config.usb_net_enabled {
        match usb::wait_for_netdev_default(&config.identity.usb_dev_mac) {
            Ok(Some(ifname)) => {
                tracing::info!(
                    ifname = %ifname,
                    kind = "usb-gadget",
                    "mesh usb netdev discovered"
                );
                links.push(link::MeshLink {
                    name: ifname,
                    kind: link::MeshLinkKind::UsbGadget,
                    metric_hint: link::DEFAULT_WIRED_METRIC,
                    wired: true,
                });
            }
            Ok(None) => {
                tracing::warn!(
                    mac = %usb::mac_string(&config.identity.usb_dev_mac),
                    "mesh usb netdev did not appear within timeout"
                );
            }
            Err(err) => {
                tracing::warn!(error = %err, "mesh usb netdev discovery failed");
            }
        }
    }

    for name in &config.manual_ifaces {
        links.push(link::MeshLink {
            name: name.clone(),
            kind: link::MeshLinkKind::Manual,
            metric_hint: link::DEFAULT_WIRED_METRIC,
            wired: true,
        });
    }

    #[cfg(feature = "mesh-wifi-p2p")]
    links.extend(wifi_p2p::discover_links());

    for link in &links {
        if let Err(err) = link::bring_up(link, &config.identity) {
            tracing::warn!(ifname = %link.name, error = %err, "mesh link bring-up failed");
        }
    }

    // Try to start babeld. If it is missing or fails, we continue without
    // dynamic routing so ADB/SSH/manual testing still works.
    let routing = match babel::start(&config, &links) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(error = %err, "mesh babel startup failed; continuing without dynamic routing");
            babel::RoutingState::unavailable()
        }
    };

    if let Err(err) = status::write(&config, &links, &routing) {
        tracing::warn!(error = %err, "mesh status file write failed");
    }
}
