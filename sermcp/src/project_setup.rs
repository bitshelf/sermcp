//! Project-local sermcp integrations for the selected skill agents.
//!
//! The SKILL selection and its deploy come from the dynamic catalog
//! (`skill_catalog` + `agent_deploy`: `~/.config/dutabo/config.jsonc`
//! leaves, fetched from their git sources). What remains project-local
//! is the sermcp-specific integration: the embedded `embedded-debug`
//! skill, the Claude/Codex hooks, the pi.dev extension
//! and the dsh plugin — installed for every selected agent that maps to
//! a known harness (`harness_for_agent`, the ONE compat table).
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/project_assets.rs"));
}

/// The harnesses sermcp knows how to integrate. Dynamic agent names map
/// here through ONE compat table — unknown agents deploy their skills
/// but get no sermcp integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Codex,
    Claude,
    Pi,
    DeepSeek,
}

/// The single agent-name → harness compat table (names as they appear in
/// `~/.config/dutabo/config.jsonc`).
pub fn harness_for_agent(agent: &str) -> Option<Harness> {
    match agent.trim().to_ascii_lowercase().as_str() {
        "claude-code" | "claude code" | "claude" => Some(Harness::Claude),
        "codex" | "openai-codex" => Some(Harness::Codex),
        "pi.dev" | "pi" => Some(Harness::Pi),
        "dsh" | "deepseek" | "deepseek harness" | "deepseek-harness" => Some(Harness::DeepSeek),
        _ => None,
    }
}

/// The project-level skill dir one harness scans.
fn harness_skill_dir(harness: Harness) -> &'static str {
    match harness {
        Harness::Codex => ".agents/skills",
        Harness::Claude => ".claude/skills",
        Harness::Pi => ".pi/skills",
        Harness::DeepSeek => ".dsh/skills",
    }
}

/// The persisted skill selection (`.sermcp/project-setup.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedSelection {
    /// The selected skill path (below the agent), empty when unselected.
    #[serde(default)]
    pub path: Vec<String>,
    /// The selected agents; a single "*" entry marks the All choice.
    #[serde(default)]
    pub agents: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    selection: SavedSelection,
    files: BTreeMap<String, Vec<u8>>,
}

pub fn saved_selection(root: &Path) -> Option<SavedSelection> {
    read_manifest(root).ok().map(|m| m.selection)
}

fn read_manifest(root: &Path) -> Result<Manifest, String> {
    let path = root.join(".sermcp/project-setup.json");
    if !path.exists() {
        return Ok(Manifest::default());
    }
    serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub struct InstallPlan {
    root: PathBuf,
    writes: BTreeMap<String, Vec<u8>>,
    remove: Vec<String>,
    manifest: Manifest,
}

fn local_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let mut result = root.to_path_buf();
    for part in Path::new(relative).components() {
        let Component::Normal(part) = part else {
            return Err("invalid integration path".into());
        };
        result.push(part);
        if std::fs::symlink_metadata(&result).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!(
                "project integration refuses symlink: {}",
                result.display()
            ));
        }
    }
    Ok(result)
}

fn read_json(root: &Path, path: &str) -> Result<Value, String> {
    let path = local_path(root, path)?;
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let value: Value = jsonc_parser::parse_to_serde_value::<Value>(&text, &Default::default())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !value.is_object() {
        return Err(format!("{} must be an object", path.display()));
    }
    Ok(value)
}

fn append_hook(config: &mut Value, event: &str, entry: Value) -> Result<(), String> {
    let obj = config
        .as_object_mut()
        .ok_or("hook config must be an object")?;
    let hooks = obj
        .entry("hooks")
        .or_insert(json!({}))
        .as_object_mut()
        .ok_or("hooks must be an object")?;
    let entries = hooks
        .entry(event)
        .or_insert(json!([]))
        .as_array_mut()
        .ok_or("hook event must be an array")?;
    if !entries.contains(&entry) {
        entries.push(entry);
    }
    Ok(())
}

fn strip_owned_hooks(config: &mut Value, root: &Path) {
    let prefix = root.join(".sermcp/hooks").to_string_lossy().into_owned();
    if let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) {
        for entries in hooks.values_mut().filter_map(Value::as_array_mut) {
            entries.retain(|entry| {
                !entry
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|handlers| {
                        handlers.len() == 1
                            && handlers[0]
                                .get("command")
                                .and_then(Value::as_str)
                                .is_some_and(|c| c.contains(&prefix))
                    })
            });
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The embedded sermcp skill installed into every mapped harness's
/// project skill dir.
const EMBEDDED_SKILLS: &[&str] = &["embedded-debug"];

/// Compute and validate every destination before writing configuration
/// or assets. `agents` are the selected catalog agent names (an empty
/// list or a list without a known harness yields no integration).
pub fn plan_integrations(
    root: &Path,
    selection: &SavedSelection,
) -> Result<Option<InstallPlan>, String> {
    let mut harnesses: Vec<Harness> = Vec::new();
    for agent in &selection.agents {
        if let Some(harness) = harness_for_agent(agent)
            && !harnesses.contains(&harness)
        {
            harnesses.push(harness);
        }
    }
    if harnesses.is_empty() {
        return Ok(None);
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    local_path(&root, ".sermcp/project-setup.json")?;
    let old = read_manifest(&root)?;
    let available: BTreeMap<String, Vec<u8>> = assets::ASSETS
        .iter()
        .map(|(p, b)| ((*p).to_string(), b.to_vec()))
        .collect();
    let mut files = BTreeMap::new();
    for name in EMBEDDED_SKILLS {
        let prefix = format!("skills/{name}/");
        for (path, bytes) in &available {
            if let Some(relative) = path.strip_prefix(&prefix) {
                for harness in &harnesses {
                    files.insert(
                        format!("{}/{name}/{relative}", harness_skill_dir(*harness)),
                        bytes.clone(),
                    );
                }
            }
        }
    }
    let claude = harnesses.contains(&Harness::Claude);
    let codex = harnesses.contains(&Harness::Codex);
    if claude || codex {
        for (path, bytes) in &available {
            if (path.starts_with("hooks/claude/") || path.starts_with("hooks/codex/"))
                && path.ends_with(".py")
            {
                files.insert(format!(".sermcp/{path}"), bytes.clone());
            }
        }
    }
    if harnesses.contains(&Harness::Pi)
        && let Some(bytes) = available.get("pi-extensions/sermcp/index.ts")
    {
        files.insert(".pi/extensions/sermcp/index.ts".into(), bytes.clone());
        files.insert(
            ".pi/extensions/sermcp/package.json".into(),
            serde_json::to_vec_pretty(&json!({
                "name":"sermcp-project-bridge", "private":true,
                "dependencies":{"@modelcontextprotocol/sdk":"^1.30.0"}
            }))
            .unwrap(),
        );
    }
    if harnesses.contains(&Harness::DeepSeek) {
        let index = available
            .get("dsh-plugins/dsh-dut/index.js")
            .ok_or("DeepSeek bridge missing")?;
        files.insert(".dsh/sermcp/index.js".into(), index.clone());
        files.insert(".dsh/sermcp/package.json".into(),serde_json::to_vec_pretty(&json!({"name":"dutabo-dsh-project","private":true,"type":"module","dependencies":{"@modelcontextprotocol/sdk":"^1.30.0"}})).unwrap());
        let module = root.join(".dsh/sermcp/index.js");
        let patch = json!([{"insert":[{"id":"dut-statusline","name":module,"inject":["tuiStatus"],"config":{"role":"statusline"}},{"id":"dut-mcp","name":module,"inject":["tools"],"config":{"role":"mcp"}}]}]);
        files.insert(
            ".dsh/sermcp/cordis.patch.yml".into(),
            serde_json::to_vec_pretty(&patch).unwrap(),
        );
        files.insert(
            ".sermcp/dsh.sh".into(),
            format!(
                "#!/bin/sh\nexec dsh --profile dsh-tui --patch {} \"$@\"\n",
                shell_quote(&root.join(".dsh/sermcp/cordis.patch.yml").to_string_lossy())
            )
            .into_bytes(),
        );
    }
    let mut remove = Vec::new();
    for (path, bytes) in &files {
        let dest = local_path(&root, path)?;
        if dest.exists() {
            let current = std::fs::read(&dest).map_err(|e| e.to_string())?;
            if current != *bytes && old.files.get(path) != Some(&current) {
                return Err(format!(
                    "preserving locally edited file: {}",
                    dest.display()
                ));
            }
        }
    }
    for (path, bytes) in &old.files {
        if !files.contains_key(path) {
            let dest = local_path(&root, path)?;
            if dest.exists() {
                if std::fs::read(&dest).map_err(|e| e.to_string())? != *bytes {
                    return Err(format!(
                        "preserving locally edited file: {}",
                        dest.display()
                    ));
                }
                remove.push(path.clone());
            }
        }
    }
    let mut writes = files.clone();
    if claude
        || old
            .selection
            .agents
            .iter()
            .any(|a| harness_for_agent(a) == Some(Harness::Claude))
    {
        let mut claude_config = read_json(&root, ".claude/settings.json")?;
        strip_owned_hooks(&mut claude_config, &root);
        if claude {
            for (event, file, matcher) in [
                ("SessionStart", "session-start.py", None),
                ("SessionEnd", "session-stop.py", None),
                ("UserPromptSubmit", "user-prompt-submit.py", None),
                ("PreToolUse", "pre-tool-use.py", Some("Bash")),
            ] {
                let cmd = format!(
                    "python3 {}",
                    shell_quote(
                        &root
                            .join(format!(".sermcp/hooks/claude/{file}"))
                            .to_string_lossy()
                    )
                );
                let mut entry = json!({"hooks":[{"type":"command","command":cmd}]});
                if let Some(matcher) = matcher {
                    entry["matcher"] = json!(matcher);
                }
                append_hook(&mut claude_config, event, entry)?;
            }
        }
        writes.insert(
            ".claude/settings.json".into(),
            serde_json::to_vec_pretty(&claude_config).unwrap(),
        );
    }
    if codex
        || old
            .selection
            .agents
            .iter()
            .any(|a| harness_for_agent(a) == Some(Harness::Codex))
    {
        let mut codex_config = read_json(&root, ".codex/hooks.json")?;
        strip_owned_hooks(&mut codex_config, &root);
        let cmd = format!(
            "python3 {} --hook",
            shell_quote(
                &root
                    .join(".sermcp/hooks/codex/serial-status.py")
                    .to_string_lossy()
            )
        );
        if codex {
            for event in ["SessionStart", "UserPromptSubmit"] {
                append_hook(
                    &mut codex_config,
                    event,
                    json!({"hooks":[{"type":"command","command":cmd}]}),
                )?;
            }
        }
        writes.insert(
            ".codex/hooks.json".into(),
            serde_json::to_vec_pretty(&codex_config).unwrap(),
        );
        if codex {
            let config_path = local_path(&root, ".codex/config.toml")?;
            let mut config: toml::Table = if config_path.exists() {
                toml::from_str(&std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?)
                    .map_err(|e| format!("{}: {e}", config_path.display()))?
            } else {
                toml::Table::new()
            };
            let features = config
                .entry("features")
                .or_insert(toml::Value::Table(toml::Table::new()))
                .as_table_mut()
                .ok_or("features must be a table")?;
            if features.get("hooks") == Some(&toml::Value::Boolean(false)) {
                return Err(
            "project explicitly disables Codex hooks; enable them before installing integrations"
                .into(),
        );
            }
            features.insert("hooks".into(), toml::Value::Boolean(true));
            let servers = config
                .entry("mcp_servers")
                .or_insert(toml::Value::Table(toml::Table::new()))
                .as_table_mut()
                .ok_or("mcp_servers must be a table")?;
            if !servers.contains_key("sermcp") {
                servers.insert("sermcp".into(), toml::Value::try_from(json!({"command":"sermcp","cwd":"..","startup_timeout_sec":15,"tool_timeout_sec":180})).map_err(|e| e.to_string())?);
            }
            writes.insert(
                ".codex/config.toml".into(),
                toml::to_string_pretty(&config)
                    .map_err(|e| e.to_string())?
                    .into_bytes(),
            );
        }
    }
    Ok(Some(InstallPlan {
        root,
        writes,
        remove,
        manifest: Manifest {
            selection: selection.clone(),
            files,
        },
    }))
}

impl InstallPlan {
    pub fn apply(self) -> Result<(), String> {
        for (path, bytes) in &self.writes {
            let dest = local_path(&self.root, path)?;
            std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::write(&dest, bytes).map_err(|e| format!("{}: {e}", dest.display()))?;
            #[cfg(unix)]
            if path.ends_with(".sh") {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| e.to_string())?;
            }
        }
        for path in &self.remove {
            let dest = local_path(&self.root, path)?;
            std::fs::remove_file(&dest).map_err(|e| e.to_string())?;
            let mut parent = dest.parent();
            while let Some(dir) = parent {
                if dir == self.root || std::fs::remove_dir(dir).is_err() {
                    break;
                }
                parent = dir.parent();
            }
        }
        let manifest = serde_json::to_vec(&self.manifest).map_err(|e| e.to_string())?;
        std::fs::write(
            local_path(&self.root, ".sermcp/project-setup.json")?,
            manifest,
        )
        .map_err(|e| e.to_string())
    }
}

/// Resolve extension dependencies for the selected agents' harnesses.
pub fn install_dependencies_for(root: &Path, agents: &[String]) -> Result<(), String> {
    let harnesses: Vec<Harness> = agents.iter().filter_map(|a| harness_for_agent(a)).collect();
    if harnesses.contains(&Harness::Pi) {
        install_dependencies(root, ".pi/extensions/sermcp", "pi")?;
    }
    if harnesses.contains(&Harness::DeepSeek) {
        install_dependencies(root, ".dsh/sermcp", "deepseek")?;
    }
    Ok(())
}

fn install_dependencies(root: &Path, relative: &str, client: &str) -> Result<(), String> {
    let dir = root.join(relative);
    if dir
        .join("node_modules/@modelcontextprotocol/sdk/package.json")
        .is_file()
    {
        return Ok(());
    }
    let log_path = root.join(format!(".sermcp/{client}-install.log"));
    let log = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new("npm")
        .args([
            "install",
            "--ignore-scripts",
            "--omit=dev",
            "--no-audit",
            "--no-fund",
        ])
        .arg("--prefix")
        .arg(&dir)
        .arg("--cache")
        .arg(root.join(".sermcp/npm-cache"))
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| format!("{client} dependencies need npm: {e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!(
                    "{client} dependency installation failed; see {}",
                    log_path.display()
                ))
            };
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{client} dependency installation timed out; see {}",
                log_path.display()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(agents: &[&str]) -> SavedSelection {
        SavedSelection {
            path: vec!["Rockchip".into(), "Linux".into()],
            agents: agents.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn integrations_follow_the_dynamic_agent_names() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join(".codex")).unwrap();
        std::fs::write(
            root.join(".codex/config.toml"),
            "model = 'custom'\n[mcp_servers.other]\ncommand = 'other'\n",
        )
        .unwrap();
        plan_integrations(root, &selection(&["claude-code", "codex"]))
            .unwrap()
            .unwrap()
            .apply()
            .unwrap();
        assert!(
            root.join(".claude/skills/embedded-debug/SKILL.md")
                .is_file()
        );
        assert!(
            root.join(".agents/skills/embedded-debug/SKILL.md")
                .is_file()
        );
        assert!(!root.join(".pi").exists());
        assert!(root.join(".sermcp/hooks/claude/inventory.py").is_file());
        let config = std::fs::read_to_string(root.join(".codex/config.toml")).unwrap();
        assert!(config.contains("custom") && config.contains("other"));
        assert_eq!(
            saved_selection(root).unwrap().agents,
            vec!["claude-code".to_string(), "codex".to_string()]
        );
        // Unknown agents deploy skills but never get sermcp integration.
        assert!(
            plan_integrations(root, &selection(&["zcode"]))
                .unwrap()
                .is_none()
        );
        // Deselecting codex removes its managed hooks, not custom ones.
        std::fs::write(root.join(".codex/hooks.json"), r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo custom"}]}]}}"#).unwrap();
        plan_integrations(root, &selection(&["claude-code"]))
            .unwrap()
            .unwrap()
            .apply()
            .unwrap();
        let value = read_json(root, ".codex/hooks.json").unwrap();
        let session_start = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(
            session_start.len(),
            1,
            "only the CUSTOM hook remains: {session_start:?}"
        );
        assert!(
            session_start[0].to_string().contains("echo custom"),
            "the custom hook is preserved: {session_start:?}"
        );
        assert!(!root.join(".agents/skills/embedded-debug").exists());
    }

    #[test]
    fn malformed_config_and_symlinks_fail_before_any_install() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join(".claude")).unwrap();
        std::fs::write(root.join(".claude/settings.json"), "invalid").unwrap();
        assert!(plan_integrations(root, &selection(&["claude-code", "codex"])).is_err());
        assert!(!root.join(".agents").exists());
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join(".agents")).unwrap();
            assert!(plan_integrations(root, &selection(&["codex"])).is_err());
            assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn local_edits_are_preserved_across_replans() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        plan_integrations(root, &selection(&["claude-code"]))
            .unwrap()
            .unwrap()
            .apply()
            .unwrap();
        let path = root.join(".claude/skills/embedded-debug/SKILL.md");
        std::fs::write(&path, "my local work").unwrap();
        assert!(plan_integrations(root, &selection(&["claude-code"])).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "my local work");
    }
}
