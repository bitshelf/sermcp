//! Skill catalog — `~/.config/dutabo/config.jsonc` (schema v1, arbitrary
//! depth).
//!
//! The document is the ONLY source of the `dutabo init` skill selectors —
//! zero presets: agents, groups and leaves are ordinary user keys and the
//! selector count is whatever the document shapes it to be.
//!
//! ```jsonc
//! { "version": 1,
//!   "agents": { "claude-code": {
//!       "deploy": { "dst": "", "mode": "merge",
//!                   "post_hook": "install.sh --project . --harness claude" },
//!       "targets": {
//!         "Rockchip": { "items": {
//!             "Linux": { "src": "https://gitcode.com/JZ_Loh/skills.git",
//!                        "ref": "main",
//!                        "post_hook": "install.sh --project . --profile linux --harness claude" }
//!         } },
//!         "Rust": { "src": "~/repos/rust-skills" }
//!       }
//! } } }
//! ```
//!
//! Path model: `Agent → node → … → Leaf(src)` at ANY depth. A node is a
//! `Leaf` when it contains `src`, a `Group` when it contains `items` —
//! exactly one of the two. The UI walks the path FIRST (target nodes,
//! dynamic selector per level) and only shows the agent selector once a
//! leaf is reached; the agents offered are the ones that own that leaf.
//!
//! `src` accepts remote git URLs (`https://`, `ssh://`, `git@host:path`)
//! and local directories (`file:///…`, absolute, `~/…`, config-relative
//! `../…`). The UI never distinguishes them.
//!
//! Structural keys (`agents`, `targets`, `items`, `deploy`, `defaults`)
//! are reserved and can never be selector names. Unknown keys are
//! rejected. Source order is preserved everywhere (serde_json
//! `preserve_order`).
//!
//! Effective config: `src`/`ref`/`download_provider` from the leaf,
//! `post_hook` = `leaf.post_hook ?? agent.deploy.post_hook` (a COMMAND
//! LINE — the first token is a script relative to the repo root, the
//! rest are its arguments), `dst`/`mode` from the agent `deploy` block.
//!
//! Path resolution: `DUTABO_CONFIG` env (tests/CI) →
//! `$XDG_CONFIG_HOME/dutabo/config.jsonc` →
//! `$HOME/.config/dutabo/config.jsonc`. A MISSING file is
//! `CatalogStatus::Missing` (a hint, not an error); parse/validation
//! failures are `CatalogStatus::Invalid(path, reason)`.

use std::path::{Path, PathBuf};

/// Env override for the catalog path (mirrors the TARGET_CONF precedent).
pub const SKILL_CATALOG_ENV: &str = "DUTABO_CONFIG";
/// The only accepted document version.
pub const CURRENT_VERSION: u32 = 1;

/// Schema limits — enforced here (hand-rolled validation, no jsonschema
/// dependency).
const NAME_MAX: usize = 64;
const SRC_MAX: usize = 2048;
const REF_MAX: usize = 256;
const DST_MAX: usize = 256;
const HOOK_MAX: usize = 512;

/// Structural keys that can never be selector names.
const RESERVED_KEYS: [&str; 5] = ["agents", "targets", "items", "deploy", "defaults"];

/// A skill source: a remote git URL or a local directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitSource {
    Remote(String),
    Local(PathBuf),
}

impl GitSource {
    /// Parse a `src` value. Remote forms: `https://…`, `http://`,
    /// `ssh://…`, `git://…` and scp-style `git@host:path`. Local forms:
    /// `file:///…`, absolute paths, `~/…`, and config-relative paths
    /// (`../repo`, `./repo`, `repo` — resolved against the catalog's
    /// directory).
    pub fn parse(value: &str, config_dir: &Path) -> Result<Self, String> {
        let v = value.trim();
        if v.is_empty() || v.len() > SRC_MAX {
            return Err(format!("src must be 1..{SRC_MAX} chars"));
        }
        for prefix in ["https://", "http://", "ssh://", "git://"] {
            if v.starts_with(prefix) {
                return Ok(Self::Remote(v.to_string()));
            }
        }
        if let Some(rest) = v.strip_prefix("git@") {
            if rest.contains(':') {
                return Ok(Self::Remote(v.to_string()));
            }
            return Err(format!("scp-style src {v:?} needs a `host:path` form"));
        }
        let path = if let Some(rest) = v.strip_prefix("file://") {
            PathBuf::from(rest)
        } else if v.contains("://") {
            return Err(format!("unsupported src scheme in {v:?}"));
        } else if let Some(rest) = v.strip_prefix("~/") {
            home_dir()
                .map(|h| h.join(rest))
                .ok_or("cannot expand ~ (no HOME)")?
        } else {
            PathBuf::from(v)
        };
        let path = if path.is_absolute() {
            path
        } else {
            config_dir.join(path)
        };
        Ok(Self::Local(path))
    }

    /// The value as a display/clone string (remote URL or local path).
    pub fn as_display(&self) -> String {
        match self {
            Self::Remote(url) => url.clone(),
            Self::Local(path) => path.display().to_string(),
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The agent-level deploy block.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeployConfig {
    /// Copy destination INSIDE the project dir; "" = never copy (hook-only
    /// deploys).
    pub dst: String,
    /// `"sync"` or `"merge"`.
    pub mode: String,
    /// Optional post-install COMMAND LINE: the first token is a script
    /// path relative to the repo root, the rest are its arguments.
    pub post_hook: Option<String>,
}

/// One node of an agent's target tree.
#[derive(Debug, Clone, PartialEq)]
pub enum SkillNode {
    Leaf(Leaf),
    Group(Group),
}

/// A group: `items` of further nodes (any depth).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Group {
    pub items: Vec<(String, SkillNode)>,
}

/// A deployable leaf: `src` required; `ref`, `post_hook` and the
/// `download_provider` capability optional.
#[derive(Debug, Clone, PartialEq)]
pub struct Leaf {
    pub src: GitSource,
    pub r#ref: Option<String>,
    pub post_hook: Option<String>,
    pub download_provider: Option<String>,
}

/// One agent: the `deploy` block + the ordered `targets` tree.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentEntry {
    pub deploy: DeployConfig,
    pub targets: Vec<(String, SkillNode)>,
}

/// The resolved, UI-facing leaf: everything one deploy run needs.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillLeaf {
    pub agent: String,
    /// The path BELOW the agent (targets root → … → leaf name).
    pub path: Vec<String>,
    pub src: GitSource,
    pub git_ref: Option<String>,
    /// Effective post hook (leaf-level ?? agent-level).
    pub post_hook: Option<String>,
    pub download_provider: Option<String>,
    pub deploy: DeployConfig,
}

/// Catalog load state surfaced to the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum CatalogStatus {
    /// Parsed and validated (the path is kept for the hint line).
    Loaded(PathBuf, Catalog),
    /// No file — the TUI renders the path + a hint; deploy is disabled.
    Missing(PathBuf),
    /// File exists but does not parse/validate (the reason is shown).
    Invalid(PathBuf, String),
}

/// The parsed catalog.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Catalog {
    pub agents: Vec<(String, AgentEntry)>,
}

impl Catalog {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse + validate JSONC text. `config_dir` resolves config-relative
    /// local sources. Errors name the offending path.
    pub fn parse(text: &str, config_dir: &Path) -> Result<Self, String> {
        let value = crate::config::parse_jsonc_value(text)?;
        let object = value.as_object().ok_or("catalog must be an object")?;
        if let Some(version) = object.get("version") {
            let version = version.as_u64().ok_or("version must be a number (1)")?;
            if version != u64::from(CURRENT_VERSION) {
                return Err(format!(
                    "unsupported catalog version {version} (expected {CURRENT_VERSION})"
                ));
            }
        } else {
            return Err("catalog needs \"version\": 1".into());
        }
        let Some(agents_value) = object.get("agents") else {
            return Err("catalog needs an \"agents\" object".into());
        };
        let agents_object = agents_value
            .as_object()
            .ok_or("\"agents\" must be an object")?;
        let mut agents = Vec::new();
        for (agent, entry) in agents_object {
            agents.push((agent.clone(), parse_agent(agent, entry, config_dir)?));
        }
        let catalog = Self { agents };
        validate(&catalog)?;
        Ok(catalog)
    }

    /// The agent keys in source order.
    pub fn agents(&self) -> Vec<&str> {
        self.agents.iter().map(|(k, _)| k.as_str()).collect()
    }

    pub fn entry(&self, agent: &str) -> Option<&AgentEntry> {
        self.agents.iter().find(|(k, _)| k == agent).map(|(_, e)| e)
    }

    fn node_at(&self, agent: &str, path: &[String]) -> Option<&SkillNode> {
        static EMPTY: &[(String, SkillNode)] = &[];
        let mut items: &[(String, SkillNode)] = &self.entry(agent)?.targets;
        let mut node: Option<&SkillNode> = None;
        for segment in path {
            node = items.iter().find(|(k, _)| k == segment).map(|(_, n)| n);
            items = match node? {
                SkillNode::Group(g) => &g.items,
                SkillNode::Leaf(_) => EMPTY,
            };
        }
        node
    }

    /// Child selector names at `prefix` across ALL agents — the union in
    /// agent order, de-duplicated, source order preserved.
    pub fn children_at(&self, prefix: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (_, entry) in &self.agents {
            let mut items: &[(String, SkillNode)] = &entry.targets;
            let mut valid = true;
            for segment in prefix {
                match items.iter().find(|(k, _)| k == segment) {
                    Some((_, SkillNode::Group(g))) => items = &g.items,
                    _ => {
                        valid = false;
                        break;
                    }
                }
            }
            if !valid {
                continue;
            }
            for (name, _) in items {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
        out
    }

    /// Whether `path` reaches a leaf (a node containing `src`) for at
    /// least one agent.
    pub fn is_leaf_path(&self, path: &[String]) -> bool {
        self.agents
            .iter()
            .any(|(agent, _)| matches!(self.node_at(agent, path), Some(SkillNode::Leaf(_))))
    }

    /// The agents whose tree has a LEAF at exactly `path`, in catalog
    /// order — the agent selector's option list once the leaf is reached.
    pub fn agents_for_leaf(&self, path: &[String]) -> Vec<String> {
        self.agents
            .iter()
            .filter(|(agent, _)| matches!(self.node_at(agent, path), Some(SkillNode::Leaf(_))))
            .map(|(agent, _)| agent.clone())
            .collect()
    }

    /// Resolve one (path, agent) to the effective deployable leaf.
    pub fn resolve_leaf(&self, path: &[String], agent: &str) -> Option<SkillLeaf> {
        let entry = self.entry(agent)?;
        let SkillNode::Leaf(leaf) = self.node_at(agent, path)? else {
            return None;
        };
        Some(SkillLeaf {
            agent: agent.to_string(),
            path: path.to_vec(),
            src: leaf.src.clone(),
            git_ref: leaf.r#ref.clone(),
            post_hook: leaf
                .post_hook
                .clone()
                .or_else(|| entry.deploy.post_hook.clone()),
            download_provider: leaf.download_provider.clone(),
            deploy: entry.deploy.clone(),
        })
    }

    /// Every deployable leaf in source order (agent-major).
    pub fn leaves(&self) -> Vec<SkillLeaf> {
        let mut out = Vec::new();
        for (agent, entry) in &self.agents {
            collect_leaves(
                agent,
                &entry.deploy,
                &entry.targets,
                &mut Vec::new(),
                &mut out,
            );
        }
        out
    }
}

fn collect_leaves(
    agent: &str,
    deploy: &DeployConfig,
    items: &[(String, SkillNode)],
    prefix: &mut Vec<String>,
    out: &mut Vec<SkillLeaf>,
) {
    for (name, node) in items {
        prefix.push(name.clone());
        match node {
            SkillNode::Leaf(leaf) => {
                out.push(SkillLeaf {
                    agent: agent.to_string(),
                    path: prefix.clone(),
                    src: leaf.src.clone(),
                    git_ref: leaf.r#ref.clone(),
                    post_hook: leaf.post_hook.clone().or_else(|| deploy.post_hook.clone()),
                    download_provider: leaf.download_provider.clone(),
                    deploy: deploy.clone(),
                });
            }
            SkillNode::Group(group) => collect_leaves(agent, deploy, &group.items, prefix, out),
        }
        prefix.pop();
    }
}

fn parse_agent(
    agent: &str,
    value: &serde_json::Value,
    config_dir: &Path,
) -> Result<AgentEntry, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("agent {agent:?} must be an object"))?;
    for key in object.keys() {
        if !matches!(key.as_str(), "deploy" | "targets") {
            return Err(format!(
                "agent {agent:?}: unknown key {key:?} (expected deploy/targets)"
            ));
        }
    }
    let deploy = match object.get("deploy") {
        Some(v) => parse_deploy(agent, v)?,
        None => DeployConfig {
            dst: String::new(),
            mode: "merge".into(),
            post_hook: None,
        },
    };
    let targets_value = object
        .get("targets")
        .ok_or_else(|| format!("agent {agent:?} needs a \"targets\" object"))?;
    let targets_object = targets_value
        .as_object()
        .ok_or_else(|| format!("agent {agent:?}: \"targets\" must be an object"))?;
    if targets_object.is_empty() {
        return Err(format!("agent {agent:?} has no targets"));
    }
    let mut targets = Vec::new();
    for (name, node) in targets_object {
        targets.push((
            name.clone(),
            parse_node(agent, std::slice::from_ref(name), node, config_dir)?,
        ));
    }
    Ok(AgentEntry { deploy, targets })
}

fn parse_deploy(agent: &str, value: &serde_json::Value) -> Result<DeployConfig, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("agent {agent:?}: deploy must be an object"))?;
    for key in object.keys() {
        if !matches!(key.as_str(), "dst" | "mode" | "post_hook") {
            return Err(format!(
                "agent {agent:?}: unknown deploy key {key:?} (expected dst/mode/post_hook)"
            ));
        }
    }
    let dst = match object.get("dst") {
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("agent {agent:?}: deploy.dst must be a string"))?;
            if s.len() > DST_MAX {
                return Err(format!(
                    "agent {agent:?}: deploy.dst exceeds {DST_MAX} chars"
                ));
            }
            s.to_string()
        }
        None => String::new(),
    };
    let mode = match object.get("mode") {
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("agent {agent:?}: deploy.mode must be a string"))?;
            if s != "sync" && s != "merge" {
                return Err(format!(
                    "agent {agent:?}: deploy.mode {:?} (expected \"sync\" or \"merge\")",
                    s
                ));
            }
            s.to_string()
        }
        None => "merge".into(),
    };
    let post_hook = match object.get("post_hook") {
        Some(serde_json::Value::Null) | None => None,
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("agent {agent:?}: deploy.post_hook must be a string"))?;
            Some(s.to_string())
        }
    };
    Ok(DeployConfig {
        dst,
        mode,
        post_hook,
    })
}

fn parse_node(
    agent: &str,
    path: &[String],
    value: &serde_json::Value,
    config_dir: &Path,
) -> Result<SkillNode, String> {
    let where_ = format!("{}/{}", agent, path.join("/"));
    let object = value
        .as_object()
        .ok_or_else(|| format!("{where_}: node must be an object"))?;
    let has_src = object.contains_key("src");
    let has_items = object.contains_key("items");
    match (has_src, has_items) {
        (true, true) => {
            return Err(format!(
                "{where_}: a node is either a leaf (\"src\") or a group (\"items\"), not both"
            ));
        }
        (false, false) => {
            return Err(format!(
                "{where_}: a node needs \"src\" (leaf) or \"items\" (group)"
            ));
        }
        _ => {}
    }
    if has_src {
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "src" | "ref" | "post_hook" | "download_provider"
            ) {
                return Err(format!(
                    "{where_}: unknown leaf key {key:?} (expected src/ref/post_hook/download_provider)"
                ));
            }
        }
        let src = object["src"]
            .as_str()
            .ok_or_else(|| format!("{where_}: src must be a string"))?;
        let src = GitSource::parse(src, config_dir).map_err(|e| format!("{where_}: {e}"))?;
        let git_ref = match object.get("ref") {
            Some(serde_json::Value::Null) | None => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("{where_}: ref must be a string"))?;
                if s.is_empty() || s.len() > REF_MAX || s.starts_with('-') {
                    return Err(format!("{where_}: invalid ref {s:?}"));
                }
                Some(s.to_string())
            }
        };
        let post_hook = match object.get("post_hook") {
            Some(serde_json::Value::Null) | None => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("{where_}: post_hook must be a string"))?;
                Some(s.to_string())
            }
        };
        let download_provider = match object.get("download_provider") {
            Some(serde_json::Value::Null) | None => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("{where_}: download_provider must be a string"))?;
                Some(s.to_string())
            }
        };
        Ok(SkillNode::Leaf(Leaf {
            src,
            r#ref: git_ref,
            post_hook,
            download_provider,
        }))
    } else {
        for key in object.keys() {
            if key != "items" {
                return Err(format!(
                    "{where_}: unknown group key {key:?} (a group only holds \"items\")"
                ));
            }
        }
        let items_object = object["items"]
            .as_object()
            .ok_or_else(|| format!("{where_}: \"items\" must be an object"))?;
        if items_object.is_empty() {
            return Err(format!("{where_}: \"items\" must be non-empty"));
        }
        let mut items = Vec::new();
        for (name, node) in items_object {
            let mut child = path.to_vec();
            child.push(name.clone());
            items.push((name.clone(), parse_node(agent, &child, node, config_dir)?));
        }
        Ok(SkillNode::Group(Group { items }))
    }
}

/// Semantic validation. Errors name the full path.
fn validate(catalog: &Catalog) -> Result<(), String> {
    if catalog.agents.is_empty() {
        return Err("agents must be non-empty".into());
    }
    for (agent, entry) in &catalog.agents {
        if !valid_name(agent) {
            return Err(format!(
                "invalid agent name {agent:?} (1..{NAME_MAX} chars, no leading/trailing spaces, no control chars)"
            ));
        }
        let deploy = &entry.deploy;
        if !valid_dst(&deploy.dst) {
            return Err(format!(
                "agent {agent:?}: deploy.dst {:?} must be a relative path without \"..\"",
                deploy.dst
            ));
        }
        if deploy.mode == "sync" && (deploy.dst.is_empty() || deploy.dst == ".") {
            return Err(format!(
                "agent {agent:?}: deploy.mode \"sync\" with dst {:?} (syncing the project root is forbidden)",
                deploy.dst
            ));
        }
        if let Some(hook) = &deploy.post_hook
            && !valid_post_hook(hook)
        {
            return Err(format!(
                "agent {agent:?}: deploy.post_hook {hook:?} must be a non-empty command line whose script path is relative without \"..\""
            ));
        }
        validate_nodes(agent, &entry.targets)?;
    }
    Ok(())
}

fn validate_nodes(agent: &str, items: &[(String, SkillNode)]) -> Result<(), String> {
    for (name, node) in items {
        if !valid_name(name) {
            return Err(format!(
                "agent {agent:?}: invalid selector name {name:?} (1..{NAME_MAX} chars, no leading/trailing spaces, no control chars)"
            ));
        }
        if RESERVED_KEYS.contains(&name.as_str()) {
            return Err(format!(
                "agent {agent:?}: {name:?} is a structural key and cannot be a selector name"
            ));
        }
        match node {
            SkillNode::Leaf(leaf) => {
                if let Some(hook) = &leaf.post_hook
                    && !valid_post_hook(hook)
                {
                    return Err(format!(
                        "{agent}/{}: post_hook {hook:?} must be a non-empty command line whose script path is relative without \"..\"",
                        name
                    ));
                }
                if let Some(provider) = &leaf.download_provider
                    && provider.trim().is_empty()
                {
                    return Err(format!(
                        "{agent}/{name}: download_provider must be a provider id"
                    ));
                }
            }
            SkillNode::Group(group) => validate_nodes(agent, &group.items)?,
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && !name.starts_with(' ')
        && !name.ends_with(' ')
        && !name.chars().any(char::is_control)
}

fn valid_dst(dst: &str) -> bool {
    dst.len() <= DST_MAX
        && !dst.starts_with('/')
        && !Path::new(dst)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// A post hook is a COMMAND LINE: `script [args…]`. The script (first
/// token) must be a non-empty relative path without `..`.
fn valid_post_hook(hook: &str) -> bool {
    let trimmed = hook.trim();
    if trimmed.is_empty() || trimmed.len() > HOOK_MAX {
        return false;
    }
    match split_command_line(trimmed) {
        Some(parts) => {
            let script = &parts[0];
            !script.is_empty()
                && !script.starts_with('/')
                && !Path::new(script)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
        }
        None => false,
    }
}

/// Minimal shell-style splitter for hook command lines: whitespace
/// separated, single/double quoted segments allowed. None on an
/// unterminated quote.
pub fn split_command_line(line: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for ch in line.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    started = true;
                }
                c if c.is_whitespace() => {
                    if started {
                        parts.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                c => {
                    current.push(c);
                    started = true;
                }
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        parts.push(current);
    }
    Some(parts)
}

/// The catalog file path (`SKILL_CATALOG_ENV` → XDG → HOME).
pub fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(SKILL_CATALOG_ENV) {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|p| p.join("dutabo").join("config.jsonc"))
}

/// Load the catalog from disk. Missing → `Missing`; parse/validation
/// failure → `Invalid`.
pub fn load() -> CatalogStatus {
    let Some(path) = config_path() else {
        return CatalogStatus::Missing(PathBuf::from("~/.config/dutabo/config.jsonc"));
    };
    if !path.exists() {
        return CatalogStatus::Missing(path);
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return CatalogStatus::Invalid(path, e.to_string()),
    };
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    match Catalog::parse(&text, &dir) {
        Ok(catalog) => CatalogStatus::Loaded(path, catalog),
        Err(reason) => CatalogStatus::Invalid(path, reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Catalog, String> {
        Catalog::parse(text, Path::new("/cfg"))
    }

    #[test]
    fn parses_arbitrary_depth_and_builds_leaf_index() {
        let catalog = parse(
            r#"{
          "version": 1,
          "agents": {
            "claude-code": {
              "deploy": {"dst": ".claude", "mode": "sync", "post_hook": "install.sh"},
              "targets": {
                "Rockchip": {"items": {
                  "Linux": {"items": {
                    "Kernel": {"items": {
                      "6.1": {"src": "https://git.example.com/rk-kernel.git", "ref": "main"}
                    }}
                  }}
                }},
                "Rust": {"src": "/local/rust-skills"}
              }
            },
            "pi.dev": {
              "deploy": {"dst": ".pi", "mode": "merge"},
              "targets": {
                "Rockchip": {"items": {
                  "Linux": {"src": "https://git.example.com/pi-rk.git"}
                }}
              }
            }
          }
        }"#,
        )
        .unwrap();
        // Dynamic children: union across agents, source order.
        assert_eq!(
            catalog.children_at(&[]),
            vec!["Rockchip".to_string(), "Rust".to_string()]
        );
        assert_eq!(
            catalog.children_at(&["Rockchip".into()]),
            vec!["Linux".to_string()]
        );
        assert_eq!(
            catalog.children_at(&["Rockchip".into(), "Linux".into()]),
            vec!["Kernel".to_string()]
        );
        // 4-level path reaches a leaf for claude-code only.
        let deep = vec![
            "Rockchip".to_string(),
            "Linux".to_string(),
            "Kernel".to_string(),
            "6.1".to_string(),
        ];
        assert!(catalog.is_leaf_path(&deep));
        assert_eq!(
            catalog.agents_for_leaf(&deep),
            vec!["claude-code".to_string()]
        );
        let leaf = catalog.resolve_leaf(&deep, "claude-code").unwrap();
        assert_eq!(
            leaf.src,
            GitSource::Remote("https://git.example.com/rk-kernel.git".into())
        );
        assert_eq!(leaf.git_ref.as_deref(), Some("main"));
        assert_eq!(leaf.deploy.dst, ".claude");
        assert_eq!(leaf.post_hook.as_deref(), Some("install.sh"));
        // 2-level: Rust leaf for claude-code; local src resolved against
        // the config dir.
        let rust = vec!["Rust".to_string()];
        assert_eq!(
            catalog.agents_for_leaf(&rust),
            vec!["claude-code".to_string()]
        );
        assert_eq!(
            catalog.resolve_leaf(&rust, "claude-code").unwrap().src,
            GitSource::Local(PathBuf::from("/local/rust-skills"))
        );
        // Mid-path: [Rockchip, Linux] is a leaf for pi.dev but a GROUP
        // for claude-code — is_leaf_path is true (any agent).
        let mid = vec!["Rockchip".to_string(), "Linux".to_string()];
        assert!(catalog.is_leaf_path(&mid));
        assert_eq!(catalog.agents_for_leaf(&mid), vec!["pi.dev".to_string()]);
        // The full leaf walk visits everything in order.
        let leaves = catalog.leaves();
        let paths: Vec<String> = leaves.iter().map(|l| l.path.join("/")).collect();
        assert_eq!(
            paths,
            vec![
                "Rockchip/Linux/Kernel/6.1".to_string(),
                "Rust".to_string(),
                "Rockchip/Linux".to_string(),
            ]
        );
    }

    #[test]
    fn rejects_structural_keys_unknown_fields_and_bad_shapes() {
        for text in [
            r#"{"version":1,"agents":{"a":{"deploy":{"dst":"","mode":"merge"},"targets":{"items":{"x":{"src":"https://a.invalid/b.git"}}}}}}"#,
            r#"{"version":1,"agents":{"a":{"deploy":{"dst":"","mode":"merge"},"targets":{"agents":{"src":"https://a.invalid/b.git"}}}}}"#,
            r#"{"version":1,"agents":{"a":{"deploy":{"dst":"","mode":"merge"},"targets":{"ok":{"src":"https://a.invalid/b.git","typo":1}}}}}"#,
            r#"{"version":1,"agents":{"a":{"deploy":{"dst":"","mode":"merge"},"targets":{"both":{"src":"https://a.invalid/b.git","items":{"x":{"src":"https://a.invalid/c.git"}}}}}}}"#,
            r#"{"version":1,"agents":{"a":{"deploy":{"dst":"","mode":"merge"},"targets":{"none":{}}}}"#,
            r#"{"version":2,"agents":{}}"#,
            r#"{"agents":{}}"#,
            r#"{"version":1}"#,
            r#"{"version":1,"agents":{}}"#,
        ] {
            assert!(parse(text).is_err(), "must reject: {text}");
        }
    }

    #[test]
    fn sync_needs_dst_and_post_hook_may_carry_args() {
        assert!(parse(
            r#"{"version":1,"agents":{"a":{"deploy":{"dst":"","mode":"sync"},"targets":{"t":{"src":"https://a.invalid/b.git"}}}}}}"#
        )
        .is_err());
        let catalog = parse(
            r#"{"version":1,"agents":{"a":{
              "deploy":{"dst":"","mode":"merge"},
              "targets":{"Linux":{"src":"https://gitcode.com/JZ_Loh/skills.git",
                                   "post_hook":"install.sh --project . --profile linux"}}}}}"#,
        )
        .unwrap();
        let leaf = catalog.resolve_leaf(&["Linux".to_string()], "a").unwrap();
        assert_eq!(
            leaf.post_hook.as_deref(),
            Some("install.sh --project . --profile linux")
        );
        assert!(split_command_line("install.sh --project . --profile linux").is_some());
        assert_eq!(
            split_command_line("s.sh 'a b'").unwrap(),
            vec!["s.sh".to_string(), "a b".to_string()]
        );
        assert!(split_command_line("s.sh 'unterminated").is_none());
    }

    #[test]
    fn git_source_remote_and_local_forms() {
        let cases = [
            (
                "https://gitcode.com/JZ_Loh/skills.git",
                GitSource::Remote("https://gitcode.com/JZ_Loh/skills.git".into()),
            ),
            (
                "ssh://git@example.com/a.git",
                GitSource::Remote("ssh://git@example.com/a.git".into()),
            ),
            (
                "git@example.com:a.git",
                GitSource::Remote("git@example.com:a.git".into()),
            ),
            (
                "file:///home/u/repo",
                GitSource::Local(PathBuf::from("/home/u/repo")),
            ),
            (
                "/home/u/repo",
                GitSource::Local(PathBuf::from("/home/u/repo")),
            ),
            ("../repo", GitSource::Local(PathBuf::from("/cfg/../repo"))),
            ("~/repo", GitSource::Local(home_dir().unwrap().join("repo"))),
        ];
        for (value, expected) in cases {
            assert_eq!(
                GitSource::parse(value, Path::new("/cfg")).unwrap(),
                expected,
                "{value}"
            );
        }
        assert!(GitSource::parse("", Path::new("/cfg")).is_err());
        assert!(GitSource::parse("git@hostonly", Path::new("/cfg")).is_err());
    }
}
