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
