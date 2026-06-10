//! Pure core for the `serial_list_duts` MCP tool.
//!
//! The tool is a thin wrapper over [`crate::config::build_inventory_json`]
//! — the SAME builder the server inventory file and `dutabo list --json`
//! use — with per-DUT state from [`crate::dut_state::inventory_state_for`]
//! (the ONE shared state derivation). The wire shape here is the tool-only
//! subset of the canonical inventory schema: file metadata (written_by,
//! mcp_pid, ...) and the ANSI statusline text are omitted, so agents get
//! exactly the decision-relevant fields (state, state-file age, critical).
//!
//! Everything in this module is pure — no env reads, no engine, no TTY —
//! so it is fully unit-testable without hardware or a config file.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::config::{
    Config, DevHostConfig, DutConfig, InventoryDevHost, InventoryJson, InventoryMeta,
    build_inventory_json, parse_config_file,
};

/// DUT row of the tool's wire shape — state, state-file age and the critical
/// flag are included; the ANSI statusline text is not (agents parse, they
/// don't render).
#[derive(Debug, serde::Serialize)]
pub(super) struct ListDutsDut {
    pub dut_name: String,
    pub dev_host_ip: String,
    pub ssh_user: String,
    pub serial_port: String,
    pub dut_dir: String,
    pub state: String,
    pub state_label: String,
    pub age_secs: Option<u64>,
    pub critical: bool,
}

/// The `serial_list_duts` payload: DUTs grouped by dev host, with
/// `current_dut` naming the DUT this server controls (TARGET_DUT_NAME)
/// when that name exists in the config.
#[derive(Debug, serde::Serialize)]
pub(super) struct ListDutsPayload {
    pub dev_host_count: usize,
    pub dut_count: usize,
    pub current_dut: Option<String>,
    /// Host rows — the canonical [`InventoryDevHost`] shape (ip/user/host_name).
    pub dev_hosts: Vec<InventoryDevHost>,
    pub duts: Vec<ListDutsDut>,
}

/// Built FROM [`InventoryJson`] (never from the raw inputs) so this tool,
/// the server's inventory file, and `dutabo list --json` can never diverge.
impl From<InventoryJson> for ListDutsPayload {
    fn from(inv: InventoryJson) -> Self {
        ListDutsPayload {
            dev_host_count: inv.dev_host_count,
            dut_count: inv.dut_count,
            current_dut: inv.current_dut,
            dev_hosts: inv.dev_hosts,
            duts: inv
                .duts
                .into_iter()
                .map(|dut| ListDutsDut {
                    dut_name: dut.dut_name,
                    dev_host_ip: dut.dev_host_ip,
                    ssh_user: dut.ssh_user,
                    serial_port: dut.serial_port,
                    dut_dir: dut.dut_dir,
                    state: dut.state,
                    state_label: dut.state_label,
                    age_secs: dut.age_secs,
                    critical: dut.critical,
                })
                .collect(),
        }
    }
}

/// Resolve the dev-host + DUT registry the inventory is built from.
///
/// With a config file, re-parse it — the loaded [`Config`] only carries the
/// single-DUT flat values map, so the multi-DUT view must come from the
/// file. Without one ([`crate::config::ConfigFormat::None`] test rigs and a
/// server running on defaults), synthesize the single DUT this server
/// controls from the flat values. An unparsable file is a loud error — the
/// tool must never invent a registry.
fn resolve_registry(config: &Config) -> Result<(Vec<DevHostConfig>, Vec<DutConfig>), String> {
    match &config.config_path {
        Some(path) => {
            let parsed = parse_config_file(path)?;
            Ok((parsed.dev_hosts, parsed.duts))
        }
        None => Ok(single_dut_registry(config)),
    }
}

/// No config file → the registry is exactly the DUT this server controls
/// (the values map already has TARGET_DUT_NAME merged by `load_config`).
/// Reads only [`Config`] getters, so defaults behave identically to the
/// runtime engine's view of the world.
fn single_dut_registry(config: &Config) -> (Vec<DevHostConfig>, Vec<DutConfig>) {
    let name = config.get_str_or("DUT_NAME", "default");
    let host = DevHostConfig {
        ip: config.dev_host_ip(),
        user: config.get_str_or("DEV_HOST_USER", ""),
        pass: config.get("DEV_HOST_PASS").to_string(),
        host_name: String::new(),
    };
    let dut = DutConfig {
        dut_name: name,
        dev_host_ip: host.ip.clone(),
        dev_host_user: host.user.clone(),
        dev_host_pass: host.pass.clone(),
        serial_port: config.get("SERIAL_PORT").to_string(),
        relay_ip: config.relay_ip(),
        relay_type: config.relay_type(),
        dev_ctl: config.dev_ctl(),
        relay_port: config.relay_port(),
        reset_ch: config.reset_channel(),
        download_ch: config.download_channel(),
        power_ch: config.power_channel(),
        power_off_time_ms: config.power_off_time_ms(),
        reference_log: config.reference_log(),
        dut_dir: config.dut_dir(),
        login_user: config.login_user(),
        login_pass: config.login_pass(),
        learner_stage_threshold: config.learner_stage_threshold(),
        learner_crash_threshold: config.learner_crash_threshold(),
        crash_patterns: config.crash_patterns(),
        section_values: HashMap::new(),
    };
    (vec![host], vec![dut])
}

/// Build the `serial_list_duts` payload. Pure: every input is a parameter —
/// the caller supplies the raw TARGET_DUT_NAME value and a clock timestamp
/// so tests never touch env or wall-clock time.
pub(super) fn list_duts_payload(
    config: &Config,
    current_dut_name: Option<&str>,
    written_at_unix: i64,
) -> Result<Value, String> {
    let (dev_hosts, duts) = resolve_registry(config)?;
    let project_dir = config
        .project_dir
        .as_deref()
        .unwrap_or_else(|| Path::new("."));
    let inventory = build_inventory_json(
        config,
        &dev_hosts,
        &duts,
        current_dut_name,
        InventoryMeta {
            written_by: "mcp-server",
            written_at_unix,
            mcp_pid: None,
            mcp_http_port: None,
            heartbeat_secs: None,
            owner: None,
        },
        |dut| crate::dut_state::inventory_state_for(project_dir, dut),
    );
    serde_json::to_value(ListDutsPayload::from(inventory))
        .map_err(|error| format!("serialize inventory: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigFormat;
    use std::collections::HashMap;
    use tempfile::TempDir;

    struct InventoryFixture {
        jsonc: String,
        first_host: String,
        ports: [String; 3],
    }

    fn available_port() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        drop(listener);
        port
    }

    fn inventory_fixture() -> InventoryFixture {
        let first_host = "host-a.invalid".to_string();
        let second_host = "host-b.invalid";
        let ports = [available_port(), available_port(), available_port()];
        let jsonc = format!(
            r#"{{
  "dev_hosts": [
    {{
      "ip": "{first_host}",
      "user": "root",
      "duts": [
        {{ "dut_name": "dut-a", "serial": {{ "port": {} }} }},
        {{ "dut_name": "dut-b", "serial": {{ "port": "{}" }} }}
      ]
    }},
    {{
      "ip": "{second_host}",
      "user": "linaro",
      "duts": [
        {{ "dut_name": "dut-c", "serial": {{ "port": {} }} }}
      ]
    }}
  ]
}}"#,
            ports[0], ports[1], ports[2]
        );
        InventoryFixture {
            jsonc,
            first_host,
            ports,
        }
    }

    fn values_with(dut_name: &str, port: &str) -> HashMap<String, String> {
        let mut values = HashMap::new();
        values.insert("DEV_HOST_IP".into(), "host.invalid".into());
        values.insert("DEV_HOST_USER".into(), "builder".into());
        values.insert("SERIAL_PORT".into(), port.into());
        values.insert("DUT_NAME".into(), dut_name.into());
        values.insert("DUT_DIR".into(), format!(".dut-serial/{dut_name}"));
        values
    }

    fn config_from_values(values: HashMap<String, String>) -> Config {
        Config {
            values,
            config_path: None,
            project_dir: Some(Path::new("/tmp/proj").to_path_buf()),
            format: ConfigFormat::None,
        }
    }

    fn config_with_fixture(tmp: &TempDir, content: &str, name: &str) -> Config {
        let path = tmp.path().join(name);
        std::fs::write(&path, content).unwrap();
        Config {
            values: HashMap::new(),
            config_path: Some(path),
            project_dir: Some(tmp.path().to_path_buf()),
            format: ConfigFormat::Jsonc,
        }
    }

    fn payload(config: &Config, current_dut_name: Option<&str>) -> Value {
        list_duts_payload(config, current_dut_name, 1_700_000_000).unwrap()
    }

    fn dut_by_name<'a>(payload: &'a Value, name: &str) -> &'a Value {
        payload["duts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|dut| dut["dut_name"] == name)
            .unwrap_or_else(|| panic!("DUT {name} missing in payload"))
    }

    /// TDD-16: without a config file the registry is the single DUT this
    /// server controls, derived from the flat values map.
    #[test]
    fn resolve_registry_without_config_file_synthesizes_single_dut() {
        let serial_port = available_port();
        let config = config_from_values(values_with("test-dut", &serial_port));
        let (hosts, duts) = resolve_registry(&config).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].ip, "host.invalid");
        assert_eq!(hosts[0].user, "builder");
        assert_eq!(hosts[0].host_name, "", "synthesized host has no name");
        assert_eq!(duts.len(), 1);
        assert_eq!(duts[0].dut_name, "test-dut");
        assert_eq!(duts[0].serial_port, serial_port);
        assert_eq!(duts[0].dut_dir, ".dut-serial/test-dut");
    }

    /// TDD-16: with a config file the full multi-host/multi-DUT registry is
    /// re-parsed (the loaded Config only carries the single-DUT flat map).
    #[test]
    fn resolve_registry_parses_config_file() {
        let tmp = TempDir::new().unwrap();
        let fixture = inventory_fixture();
        let config = config_with_fixture(&tmp, &fixture.jsonc, ".target.jsonc");
        let (hosts, duts) = resolve_registry(&config).unwrap();
        assert_eq!(hosts.len(), 2);
        assert_eq!(duts.len(), 3);
        let dut_a = duts.iter().find(|dut| dut.dut_name == "dut-a").unwrap();
        assert_eq!(dut_a.dev_host_ip, fixture.first_host);
        assert_eq!(dut_a.dev_host_user, "root");
        assert_eq!(dut_a.serial_port, fixture.ports[0]);
        assert_eq!(dut_a.dut_dir, ".dut-serial/dut-a");
    }

    /// A non-JSONC file is the plain-parse-error case: the tool reports the
    /// parse failure instead of pretending to know the config.
    #[test]
    fn resolve_registry_rejects_non_jsonc_with_parse_error() {
        let tmp = TempDir::new().unwrap();
        let config = config_with_fixture(&tmp, "alias = 'old'\n", ".target.txt");
        let error = resolve_registry(&config).unwrap_err();
        assert!(
            error.contains("JSONC parse error"),
            "expected a plain JSONC parse error, got: {error}"
        );
        assert!(!error.contains("no longer supported"), "{error}");
        assert!(!error.contains("dutabo init"), "{error}");
    }

    /// A config file that fails strict JSONC parse is a loud error — the
    /// tool must never invent a registry from defaults.
    #[test]
    fn resolve_registry_reports_parse_errors() {
        let tmp = TempDir::new().unwrap();
        let config = config_with_fixture(&tmp, "{ dev_hosts: [ }", ".target.jsonc");
        assert!(resolve_registry(&config).is_err());
    }

    /// TDD-16 golden: the payload groups DUTs by host, derives per-DUT
    /// state through the ONE shared helper (active / crashed / missing →
    /// unknown), and resolves current_dut only when the name exists.
    #[test]
    fn payload_golden_from_config_file() {
        let tmp = TempDir::new().unwrap();
        let fixture = inventory_fixture();
        let config = config_with_fixture(&tmp, &fixture.jsonc, ".target.jsonc");
        // Per-DUT state files: dut-a active, dut-b crashed, dut-c none.
        std::fs::create_dir_all(tmp.path().join(".dut-serial/dut-a")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".dut-serial/dut-b")).unwrap();
        std::fs::write(tmp.path().join(".dut-serial/dut-a/target-state"), "active").unwrap();
        std::fs::write(
            tmp.path().join(".dut-serial/dut-b/target-state"),
            "crashed\n",
        )
        .unwrap();

        let payload = payload(&config, Some("dut-a"));
        assert_eq!(payload["dev_host_count"], 2);
        assert_eq!(payload["dut_count"], 3);
        assert_eq!(payload["current_dut"], "dut-a");

        let host_1 = &payload["dev_hosts"][0];
        assert_eq!(host_1["ip"], fixture.first_host);
        assert_eq!(host_1["user"], "root");

        let dut_a = dut_by_name(&payload, "dut-a");
        assert_eq!(dut_a["state"], "active");
        assert_eq!(dut_a["state_label"], "active");
        assert_eq!(dut_a["critical"], false);
        assert!(dut_a["age_secs"].as_u64().is_some());
        assert_eq!(dut_a["serial_port"], fixture.ports[0]);
        assert_eq!(dut_a["ssh_user"], "root");
        assert_eq!(dut_a["dev_host_ip"], fixture.first_host);
        assert_eq!(dut_a["dut_dir"], ".dut-serial/dut-a");

        let dut_b = dut_by_name(&payload, "dut-b");
        assert_eq!(dut_b["state"], "crashed");
        assert_eq!(dut_b["state_label"], "✗ crashed");
        assert_eq!(dut_b["critical"], true);
        assert_eq!(dut_b["serial_port"], fixture.ports[1]);

        let dut_c = dut_by_name(&payload, "dut-c");
        assert_eq!(dut_c["state"], "unknown");
        assert_eq!(dut_c["state_label"], "unknown");
        assert_eq!(dut_c["critical"], false);
        assert_eq!(dut_c["age_secs"], Value::Null);
        assert_eq!(dut_c["ssh_user"], "linaro");
    }

    /// TDD-16: current_dut is None when the env name is unset, and when it
    /// names a DUT that does not exist — the server controls exactly the
    /// listed DUT, nothing else.
    #[test]
    fn current_dut_resolves_only_existing_name() {
        let tmp = TempDir::new().unwrap();
        let fixture = inventory_fixture();
        let config = config_with_fixture(&tmp, &fixture.jsonc, ".target.jsonc");
        assert_eq!(payload(&config, None)["current_dut"], Value::Null);
        assert_eq!(payload(&config, Some("nope"))["current_dut"], Value::Null);
        assert_eq!(payload(&config, Some("dut-c"))["current_dut"], "dut-c");
    }

    /// The tool wire shape omits inventory-file metadata and ANSI statusline
    /// text — agents get decision fields, not render artifacts.
    #[test]
    fn payload_omits_file_metadata_and_ansi_text() {
        let tmp = TempDir::new().unwrap();
        let fixture = inventory_fixture();
        let config = config_with_fixture(&tmp, &fixture.jsonc, ".target.jsonc");
        let payload = payload(&config, None);
        for key in [
            "schema_version",
            "written_by",
            "written_at_unix",
            "project_dir",
            "config_path",
            "mcp_pid",
            "mcp_http_port",
        ] {
            assert!(payload.get(key).is_none(), "payload must not expose {key}");
        }
        for host in payload["dev_hosts"].as_array().unwrap() {
            assert!(host.get("host_name").is_some(), "host_name must be present");
            for key in ["alias", "dut_aliases"] {
                assert!(
                    host.get(key).is_none(),
                    "payload must not expose deleted host {key}"
                );
            }
        }
        for dut in payload["duts"].as_array().unwrap() {
            for key in [
                "state_text",
                "state_plain",
                "mcp_port",
                "dev_host_alias",
                "alias",
            ] {
                assert!(dut.get(key).is_none(), "payload must not expose {key}");
            }
        }
    }

    /// Single-source gate: the per-DUT state fields in the tool payload are
    /// byte-identical to the canonical InventoryJson built from the same
    /// inputs — the tool, `dutabo list --json` and inventory.json share one
    /// derivation.
    #[test]
    fn payload_state_fields_match_canonical_inventory() {
        let tmp = TempDir::new().unwrap();
        let fixture = inventory_fixture();
        let config = config_with_fixture(&tmp, &fixture.jsonc, ".target.jsonc");
        std::fs::create_dir_all(tmp.path().join(".dut-serial/dut-a")).unwrap();
        std::fs::write(tmp.path().join(".dut-serial/dut-a/target-state"), "DUT-off").unwrap();

        let (hosts, duts) = resolve_registry(&config).unwrap();
        let inventory = build_inventory_json(
            &config,
            &hosts,
            &duts,
            Some("dut-a"),
            InventoryMeta {
                written_by: "mcp-server",
                written_at_unix: 1_700_000_000,
                mcp_pid: Some(42),
                mcp_http_port: Some(9001),
                heartbeat_secs: None,
                owner: None,
            },
            |dut| crate::dut_state::inventory_state_for(tmp.path(), dut),
        );
        let payload = payload(&config, Some("dut-a"));

        for inv_dut in &inventory.duts {
            let dut = dut_by_name(&payload, &inv_dut.dut_name);
            assert_eq!(dut["state"], inv_dut.state.as_str());
            assert_eq!(dut["state_label"], inv_dut.state_label.as_str());
            assert_eq!(dut["age_secs"], serde_json::Value::from(inv_dut.age_secs));
            assert_eq!(dut["critical"], inv_dut.critical);
        }
        // The DUT-off state must be critical and prominent in both views.
        let dut_a = dut_by_name(&payload, "dut-a");
        assert_eq!(dut_a["state"], "DUT-off");
        assert_eq!(dut_a["state_label"], "✗ DUT-off");
        assert_eq!(dut_a["critical"], true);
    }

    /// The single-DUT registry path (no config file) still yields a valid
    /// payload — the tool works on defaults and test rigs.
    #[test]
    fn payload_works_without_config_file() {
        let tmp = TempDir::new().unwrap();
        let serial_port = available_port();
        let mut config = config_from_values(values_with("solo", &serial_port));
        config.project_dir = Some(tmp.path().to_path_buf());
        let payload = payload(&config, Some("solo"));
        assert_eq!(payload["dev_host_count"], 1);
        assert_eq!(payload["dut_count"], 1);
        assert_eq!(payload["current_dut"], "solo");
        assert_eq!(payload["duts"][0]["state"], "unknown");
        assert_eq!(payload["duts"][0]["age_secs"], Value::Null);
    }
}
