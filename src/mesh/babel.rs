//! Babel routing backend.
//!
//! For v0, PocketMesh shells out to a bundled `/sbin/babeld` binary. This
//! module generates the daemon config from the known mesh interfaces and
//! node ULA, then supervises the process on a dedicated thread.
//!
//! If `/sbin/babeld` is absent, the subsystem still configures the USB
//! netdev and ULA and continues without dynamic routing.

use std::{fs, io, path::Path, thread, time::Duration};

use super::Config;
use super::identity::MeshIdentity;
use super::link::MeshLink;

const BABELD_PATH: &str = "/sbin/babeld";
const BABELD_CONFIG_PATH: &str = "/run/pocketboot-babeld.conf";
const BABELD_STATE_PATH: &str = "/run/pocketboot-babeld.state";
const RESTART_DELAY: Duration = Duration::from_secs(2);
const LIVENESS_POLL: Duration = Duration::from_millis(500);

/// Observed routing state, recorded for the status file.
#[derive(Clone, Debug)]
#[allow(dead_code)] // config_path is recorded for diagnostics.
pub(crate) struct RoutingState {
    pub(crate) backend: &'static str,
    pub(crate) running: bool,
    pub(crate) config_path: String,
    pub(crate) available: bool,
}

impl RoutingState {
    pub(crate) fn unavailable() -> Self {
        Self {
            backend: "babeld",
            running: false,
            config_path: BABELD_CONFIG_PATH.to_string(),
            available: false,
        }
    }
}

/// Start babeld for the known links. Returns the observed routing state.
pub(crate) fn start(config: &Config, links: &[MeshLink]) -> io::Result<RoutingState> {
    if links.is_empty() {
        tracing::info!("mesh has no links; skipping babeld");
        return Ok(RoutingState {
            backend: "babeld",
            running: false,
            config_path: BABELD_CONFIG_PATH.to_string(),
            available: false,
        });
    }

    if !Path::new(BABELD_PATH).is_file() {
        tracing::warn!(path = BABELD_PATH, "mesh babel unavailable");
        return Ok(RoutingState::unavailable());
    }

    let conf = generate_config(&config.identity, links);
    if let Err(err) = write_config(BABELD_CONFIG_PATH, &conf) {
        tracing::warn!(path = BABELD_CONFIG_PATH, error = %err, "mesh babel config write failed");
        return Ok(RoutingState::unavailable());
    }
    tracing::info!(path = BABELD_CONFIG_PATH, "mesh babel config written");

    let pid = match spawn_babeld(links) {
        Ok(pid) => pid,
        Err(err) => {
            tracing::warn!(error = %err, "mesh babel spawn failed");
            return Ok(RoutingState {
                backend: "babeld",
                running: false,
                config_path: BABELD_CONFIG_PATH.to_string(),
                available: true,
            });
        }
    };
    tracing::info!(pid, "mesh babel started");

    // Supervise on a dedicated thread so the mesh supervisor is not
    // blocked. Liveness is checked with kill(pid, 0); the PID 1 reaper
    // takes care of reaping the zombie.
    let ifnames: Vec<String> = links.iter().map(|link| link.name.clone()).collect();
    thread::Builder::new()
        .name("pocketboot-mesh-babel".to_string())
        .spawn(move || supervise(pid, ifnames))
        .map_err(|err| io::Error::other(format!("spawn babel supervisor: {err}")))?;

    Ok(RoutingState {
        backend: "babeld",
        running: true,
        config_path: BABELD_CONFIG_PATH.to_string(),
        available: true,
    })
}

fn supervise(pid: i32, ifnames: Vec<String>) {
    // First lifetime: wait until exit.
    wait_for_exit(pid);
    tracing::warn!(pid, "mesh babel exited");

    // One restart attempt after a short delay.
    thread::sleep(RESTART_DELAY);
    match spawn_babeld_by_args(&ifnames) {
        Ok(pid2) => {
            tracing::info!(pid = pid2, "mesh babel restarted");
            wait_for_exit(pid2);
            tracing::warn!(pid = pid2, "mesh babel exited again; giving up");
        }
        Err(err) => {
            tracing::warn!(error = %err, "mesh babel restart failed; giving up");
        }
    }
}

fn wait_for_exit(pid: i32) {
    loop {
        let rc = unsafe { libc::kill(pid, 0) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                return;
            }
            // EPERM means it exists but isn't ours; keep polling.
        }
        thread::sleep(LIVENESS_POLL);
    }
}

fn spawn_babeld(links: &[MeshLink]) -> io::Result<i32> {
    let ifnames: Vec<String> = links.iter().map(|link| link.name.clone()).collect();
    spawn_babeld_by_args(&ifnames)
}

fn spawn_babeld_by_args(ifnames: &[String]) -> io::Result<i32> {
    use std::process::Command;

    // Hold the reaper guard only across spawn so PID 1 doesn't reap the
    // child before we record its pid. After spawn we let the reaper own
    // zombie reaping and track liveness via kill(pid, 0).
    let _guard = crate::reaper::child_guard();
    let mut cmd = Command::new(BABELD_PATH);
    cmd.args([
        "-c",
        BABELD_CONFIG_PATH,
        "-S",
        BABELD_STATE_PATH,
        // No pidfile: the supervisor tracks liveness itself.
        "-I",
        "",
    ]);
    for name in ifnames {
        cmd.arg(name);
    }
    let child = cmd
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("spawn {BABELD_PATH}: {err}")))?;
    Ok(child.id() as i32)
}

/// Generate the babeld configuration text.
pub(crate) fn generate_config(identity: &MeshIdentity, links: &[MeshLink]) -> String {
    let mut out = String::new();
    out.push_str("# Generated by pocketboot mesh. Do not edit.\n");
    for link in links {
        out.push_str(&format!(
            "interface {} type {}\n",
            link.name,
            link.kind.babel_type()
        ));
    }
    out.push('\n');
    out.push_str(&format!(
        "redistribute local ip {}/128 allow\n",
        identity.ula_addr
    ));
    out.push_str("redistribute local deny\n");
    out
}

fn write_config(path: &str, contents: &str) -> io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::identity::derive_from_inputs;
    use crate::mesh::link::{MeshLink, MeshLinkKind};

    fn sample_identity() -> MeshIdentity {
        derive_from_inputs("abc123", "Phone X", &["qcom,foo".to_string()])
    }

    #[test]
    fn config_includes_known_interfaces() {
        let identity = sample_identity();
        let links = vec![
            MeshLink {
                name: "pbmesh0".into(),
                kind: MeshLinkKind::UsbGadget,
                metric_hint: 96,
                wired: true,
            },
            MeshLink {
                name: "wlan0".into(),
                kind: MeshLinkKind::WifiP2p,
                metric_hint: 256,
                wired: false,
            },
        ];
        let conf = generate_config(&identity, &links);
        assert!(conf.contains("interface pbmesh0 type wired"));
        assert!(conf.contains("interface wlan0 type wireless"));
    }

    #[test]
    fn config_includes_node_ula() {
        let identity = sample_identity();
        let links = vec![MeshLink {
            name: "pbmesh0".into(),
            kind: MeshLinkKind::UsbGadget,
            metric_hint: 96,
            wired: true,
        }];
        let conf = generate_config(&identity, &links);
        assert!(conf.contains(&format!(
            "redistribute local ip {}/128 allow",
            identity.ula_addr
        )));
        assert!(conf.contains("redistribute local deny"));
    }

    #[test]
    fn empty_link_list_produces_valid_config() {
        let identity = sample_identity();
        let conf = generate_config(&identity, &[]);
        assert!(conf.contains("redistribute local deny"));
        assert!(!conf.contains("interface "));
    }
}
