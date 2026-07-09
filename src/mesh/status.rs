//! Mesh status diagnostics.
//!
//! Writes `/run/pocketboot/mesh/status.json` with the current mesh state.
//! The JSON is built manually to avoid pulling a JSON serializer into the
//! init binary. The file is readable over existing ADB sync/shell paths.

use std::{fs, io, path::Path};

use super::Config;
use super::babel::RoutingState;
use super::link::MeshLink;

const STATUS_DIR: &str = "/run/pocketboot/mesh";
const STATUS_PATH: &str = "/run/pocketboot/mesh/status.json";

/// Write the mesh status file.
pub(crate) fn write(config: &Config, links: &[MeshLink], routing: &RoutingState) -> io::Result<()> {
    let json = build_status_json(config, links, routing);
    fs::create_dir_all(STATUS_DIR)?;
    let path = Path::new(STATUS_PATH);
    fs::write(path, json.as_bytes())
        .map_err(|err| io::Error::other(format!("write {}: {err}", path.display())))?;
    tracing::debug!(path = STATUS_PATH, "mesh status written");
    Ok(())
}

fn build_status_json(config: &Config, links: &[MeshLink], routing: &RoutingState) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"enabled\": true,\n");
    out.push_str(&format!(
        "  \"node_id\": {},\n",
        json_string(&config.identity.node_id_hex)
    ));
    out.push_str(&format!(
        "  \"ula\": {},\n",
        json_string(&config.identity.ula_addr.to_string())
    ));
    out.push_str("  \"links\": [");
    if links.is_empty() {
        out.push_str("],\n");
    } else {
        out.push('\n');
        for (i, link) in links.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!("      \"name\": {},\n", json_string(&link.name)));
            out.push_str(&format!(
                "      \"kind\": {},\n",
                json_string(link.kind.label())
            ));
            out.push_str("      \"up\": true\n");
            out.push_str("    }");
            if i + 1 < links.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ],\n");
    }
    out.push_str("  \"routing\": {\n");
    out.push_str(&format!(
        "    \"backend\": {},\n",
        json_string(routing.backend)
    ));
    out.push_str(&format!("    \"running\": {},\n", routing.running));
    out.push_str(&format!("    \"available\": {}\n", routing.available));
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::babel::RoutingState;
    use crate::mesh::identity::derive_from_inputs;
    use crate::mesh::link::{MeshLink, MeshLinkKind};

    #[test]
    fn status_json_is_well_formed() {
        let identity = derive_from_inputs("abc", "M", &["c".to_string()]);
        let config = Config {
            identity,
            manual_ifaces: vec![],
            usb_net_enabled: true,
        };
        let links = vec![MeshLink {
            name: "pbmesh0".into(),
            kind: MeshLinkKind::UsbGadget,
            metric_hint: 96,
            wired: true,
        }];
        let routing = RoutingState {
            backend: "babeld",
            running: true,
            config_path: "/run/pocketboot-babeld.conf".into(),
            available: true,
        };
        let json = build_status_json(&config, &links, &routing);
        assert!(json.contains("\"enabled\": true"));
        assert!(json.contains("\"node_id\":"));
        assert!(json.contains("\"pbmesh0\""));
        assert!(json.contains("\"usb-gadget\""));
        assert!(json.contains("\"running\": true"));
        assert!(json.contains("\"available\": true"));
        assert!(json.trim().ends_with('}'));
    }

    #[test]
    fn json_string_escapes_quotes() {
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn empty_links_produces_empty_array() {
        let identity = derive_from_inputs("abc", "M", &[]);
        let config = Config {
            identity,
            manual_ifaces: vec![],
            usb_net_enabled: false,
        };
        let routing = RoutingState::unavailable();
        let json = build_status_json(&config, &[], &routing);
        assert!(json.contains("\"links\": []"));
    }
}
