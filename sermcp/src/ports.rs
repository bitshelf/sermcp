//! Deterministic MCP HTTP port allocation, shared by both binaries
//! (`main.rs`, `bin/dutabo.rs`) and mirrored by the Claude session hook
//! `hooks/claude/session-start.py` (`_project_http_port`). Both
//! implementations MUST stay byte-identical. The project path is the
//! ownership and port-allocation key; DUT names never allocate more MCP
//! endpoints.

use md5::{Digest, Md5};
use std::path::Path;

/// Base of the reserved loopback port range (3001..3099).
pub const MC_PORT_BASE: u16 = 3001;
/// Number of ports in the reserved range.
pub const MC_PORT_RANGE: u16 = 99;

/// Project-scoped control port. MUST match
/// `hooks/claude/session-start.py::_project_http_port`
/// (`md5(resolve(project_dir))[:8] % 99 + 3001`).
pub fn project_mcp_port(project_dir: &Path) -> u16 {
    let mut hasher = Md5::new();
    hasher.update(canonical(project_dir).to_string_lossy().as_bytes());
    port_from_digest(&md5_hex(&hasher.finalize()))
}

/// Compatibility helper for inventory consumers that store a port on each
/// DUT entry. Every entry resolves to the one project MCP port.
pub fn project_dut_mcp_port(project_dir: &Path, _name: &str) -> u16 {
    project_mcp_port(project_dir)
}

/// Actual control port recorded by this project's singleton. Consumers
/// still verify the endpoint before using it because inventory can be stale.
pub fn recorded_mcp_port(project_dir: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(project_dir.join(".dut-serial/inventory.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if value["project_dir"].as_str()? != canonical(project_dir).to_str()? {
        return None;
    }
    u16::try_from(value["mcp_http_port"].as_u64()?)
        .ok()
        .filter(|port| *port > 0)
}

/// Remove repeated candidates while preserving discovery preference.
pub fn deduplicate_ports(ports: &mut Vec<u16>) {
    let mut seen = std::collections::HashSet::new();
    ports.retain(|port| seen.insert(*port));
}

/// Bind candidates for a stdio singleton's control transport: the
/// deterministic project port first, then the following slots. Fallback
/// slots exist because the 99-port hash space is shared by every project
/// on the host — a collision (or an unrelated process on the port) must
/// degrade to a neighbor instead of killing the stdio service. Consumers
/// (bridge, dutabo) discover the ACTUAL bound port via inventory.json's
/// `mcp_http_port`; `primary + 8` stays far below any ephemeral range.
pub fn control_port_candidates(primary: u16) -> impl Iterator<Item = u16> {
    std::iter::once(primary).chain((1..=8u16).map(move |offset| primary + offset))
}

/// Zero-padded lowercase hex of an md5 digest (32 chars), byte-identical
/// to Python's `hashlib.md5(...).hexdigest()` — the value the mirrored
/// session hook (`hooks/claude/session-start.py`) computes. digest 0.11
/// (via md-5 0.11) swapped GenericArray for hybrid_array, whose output
/// type dropped the `LowerHex` impl that `format!("{:x}", ...)` relied on,
/// so encode explicitly (per-byte `{:02x}`, matching GenericArray's
/// zero-padded formatting).
pub fn md5_hex(digest: &[u8]) -> String {
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn canonical(project_dir: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf())
}

fn port_from_digest(hex_digest: &str) -> u16 {
    let value = u64::from_str_radix(&hex_digest[..8], 16).unwrap_or(0);
    MC_PORT_BASE + (value % MC_PORT_RANGE as u64) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use md5::{Digest, Md5};

    #[test]
    fn recorded_port_requires_matching_project_and_valid_port() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".dut-serial");
        std::fs::create_dir(&dir).unwrap();
        for (project, port, expected) in [
            (canonical(tmp.path()), 3010u64, Some(3010)),
            (std::path::PathBuf::from("/other-project"), 3010, None),
            (canonical(tmp.path()), 0, None),
            (canonical(tmp.path()), 65536, None),
        ] {
            std::fs::write(
                dir.join("inventory.json"),
                serde_json::json!({"project_dir": project, "mcp_http_port": port}).to_string(),
            )
            .unwrap();
            assert_eq!(recorded_mcp_port(tmp.path()), expected);
        }
    }

    #[test]
    fn discovery_removes_nonadjacent_duplicates_without_reordering() {
        let mut ports = vec![3005, 3006, 3005, 3007, 3006];
        deduplicate_ports(&mut ports);
        assert_eq!(ports, vec![3005, 3006, 3007]);
    }

    #[test]
    fn md5_hex_matches_python_hexdigest() {
        // Known-answer: md5("") == d41d8cd98f00b204e9800998ecf8427e —
        // zero-padded lowercase, the exact format Python's hashlib
        // hexdigest() and the mirrored session hook produce.
        let digest = Md5::digest(b"");
        assert_eq!(md5_hex(&digest), "d41d8cd98f00b204e9800998ecf8427e");
        // Small leading bytes must stay zero-padded (byte 0x0f -> "0f").
        assert_eq!(md5_hex(&[0x0f, 0x00, 0xff]), "0f00ff");
    }
}
