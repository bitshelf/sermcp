//! Project-wide DUT registry. Each entry owns an independent serial engine.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::Value;

use crate::config::Config;
use crate::serial_engine::{SharedEngine, new_shared_engine};

#[derive(Clone)]
pub struct ManagedDut {
    pub engine: SharedEngine,
    pub connecting_state: Value,
    pub(crate) capabilities: crate::mcp_handler::DutCapabilities,
    endpoint: Arc<std::sync::Mutex<(String, String)>>,
}

#[derive(Clone)]
pub struct EngineRegistry {
    entries: Arc<BTreeMap<String, ManagedDut>>,
    reconcile_gate: Arc<tokio::sync::Mutex<()>>,
    restart_tasks: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    pub tasks: Arc<crate::task_manager::TaskManager>,
    pub(crate) inventory_config: Option<Config>,
}

impl EngineRegistry {
    pub fn from_config(config: &Config) -> Result<Self, String> {
        let configs = config.managed_dut_configs()?;
        let mut names = HashSet::new();
        let mut endpoints = HashSet::new();
        let mut directories = BTreeMap::new();
        // Validate all identities before opening any log files or connections.
        for config in &configs {
            config.validate().map_err(|errors| errors.join("; "))?;
            let name = config.get_str_or("DUT_NAME", "default");
            if !names.insert(name.clone()) {
                return Err(format!("Duplicate DUT name: {name}"));
            }
            if !endpoints.insert(endpoint_identity(
                &config.dev_host_ip(),
                &config.serial_target(),
            )) {
                return Err(format!(
                    "DUT {name} shares a serial endpoint with another DUT"
                ));
            }
            let project = config
                .project_dir
                .as_deref()
                .unwrap_or(std::path::Path::new("."));
            for path in [
                project.join(config.dut_dir()),
                project.join(".dut-serial").join(&name),
            ] {
                let path = normalize_path(&path);
                if let Some(owner) = directories.insert(path, name.clone())
                    && owner != name
                {
                    return Err(format!(
                        "DUT {name} shares a log/state directory with {owner}"
                    ));
                }
            }
        }
        let entries = configs
            .into_iter()
            .map(|config| {
                let name = config.get_str_or("DUT_NAME", "default");
                let endpoint = Arc::new(std::sync::Mutex::new((
                    config.dev_host_ip(),
                    config.serial_target(),
                )));
                let engine = new_shared_engine(config);
                let snapshot = engine
                    .try_lock()
                    .expect("new engine is unlocked")
                    .connecting_state_dict();
                let capabilities = crate::mcp_handler::DutCapabilities::from_engine(&engine);
                (
                    name,
                    ManagedDut {
                        engine,
                        connecting_state: snapshot,
                        capabilities,
                        endpoint,
                    },
                )
            })
            .collect();
        Ok(Self {
            entries: Arc::new(entries),
            reconcile_gate: Arc::new(tokio::sync::Mutex::new(())),
            restart_tasks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            tasks: crate::task_manager::TaskManager::new(),
            inventory_config: Some(config.clone()),
        })
    }

    /// Compatibility adapter for single-engine library callers.
    pub fn single(engine: SharedEngine, connecting_state: Value) -> Self {
        let name = connecting_state["dut_name"]
            .as_str()
            .unwrap_or("default")
            .to_string();
        let capabilities = crate::mcp_handler::DutCapabilities::from_engine(&engine);
        let inventory_config = engine.try_lock().ok().map(|engine| engine.config.clone());
        let endpoint = Arc::new(std::sync::Mutex::new(
            inventory_config
                .as_ref()
                .map(|config| (config.dev_host_ip(), config.serial_target()))
                .unwrap_or_default(),
        ));
        Self {
            reconcile_gate: Arc::new(tokio::sync::Mutex::new(())),
            restart_tasks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            inventory_config,
            tasks: crate::task_manager::TaskManager::new(),
            entries: Arc::new(BTreeMap::from([(
                name,
                ManagedDut {
                    engine,
                    connecting_state,
                    capabilities,
                    endpoint,
                },
            )])),
        }
    }

    pub fn entries(&self) -> &BTreeMap<String, ManagedDut> {
        &self.entries
    }

    pub fn is_multi(&self) -> bool {
        self.entries.len() > 1
    }

    pub fn first(&self) -> &ManagedDut {
        self.entries
            .first_key_value()
            .expect("registry has at least one DUT")
            .1
    }

    pub fn select(&self, name: Option<&str>) -> Result<&ManagedDut, String> {
        match name {
            Some(name) => self
                .entries
                .get(name)
                .filter(|dut| !dut.engine.flags.removed.load(Ordering::Acquire))
                .ok_or_else(|| {
                    format!(
                        "Unknown DUT {name:?}; available: {}",
                        self.names().join(", ")
                    )
                }),
            None if self.is_multi() => Err(format!(
                "Specify dut in this multi-DUT project; available: {}",
                self.names().join(", ")
            )),
            None if self.first().engine.flags.removed.load(Ordering::Acquire) => {
                Err("DUT removed from project; restart the server to load new devices".into())
            }
            None => Ok(self.first()),
        }
    }

    pub fn names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, dut)| !dut.engine.flags.removed.load(Ordering::Acquire))
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Legacy process-wide signals cannot identify one DUT. Consume them
    /// without changing any engine in multi-DUT mode; use targeted WS instead.
    pub fn spawn_takeover_signals(&self) -> Option<tokio::task::JoinHandle<()>> {
        if !self.is_multi() {
            return crate::serial_engine::spawn_human_takeover_signals(self.first().engine.clone());
        }
        #[cfg(unix)]
        let handle = tokio::spawn(async {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut yield_signal), Ok(mut resume_signal)) = (
                signal(SignalKind::user_defined1()),
                signal(SignalKind::user_defined2()),
            ) else {
                tracing::error!("Cannot install project takeover signal handlers");
                return;
            };
            loop {
                tokio::select! { _ = yield_signal.recv() => {}, _ = resume_signal.recv() => {} }
                tracing::warn!(
                    "Untargeted serial takeover signal ignored in multi-DUT mode; use /serial/ws?dut=<name>"
                );
            }
        });
        #[cfg(unix)]
        {
            Some(handle)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Reload edited endpoints and revoke endpoints absent from the saved configuration. The request
    /// names the exact config, so a colliding control port cannot affect a
    /// different project, regardless of the server's working directory.
    pub async fn reconcile_saved_config(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<String>, String> {
        let _gate = self.reconcile_gate.lock().await;
        let config = self
            .inventory_config
            .as_ref()
            .ok_or("Server has no project configuration")?;
        let expected = config
            .config_path
            .as_ref()
            .ok_or("Server has no configuration path")?;
        if std::fs::canonicalize(path).map_err(|e| e.to_string())?
            != std::fs::canonicalize(expected).map_err(|e| e.to_string())?
        {
            return Err("Configuration belongs to a different project or TEST session".into());
        }
        // Zero-DUT-tolerant parse: an explicitly empty dev_hosts array (the
        // wizard's zero-host end state) must release every endpoint, while
        // any real parse error still fails loudly.
        let saved = crate::config::parse_config_file_allow_empty(expected)?;
        let mut desired: BTreeMap<_, _> = config
            .managed_dut_configs_allow_empty()?
            .into_iter()
            .map(|config| (config.get_str_or("DUT_NAME", "default"), config))
            .collect();
        // Validate before releasing any current endpoint.
        for config in desired.values() {
            config.validate().map_err(|errors| errors.join("; "))?;
        }
        let endpoints: HashSet<_> = saved
            .duts
            .iter()
            .filter(|dut| !dut.dev_host_ip.is_empty() && !dut.serial_port.is_empty())
            .map(|dut| endpoint_identity(&dut.dev_host_ip, &dut.serial_port))
            .collect();
        let mut released = Vec::new();
        let mut restart = Vec::new();
        for (name, dut) in self.entries.iter() {
            // Unchanged DUTs may be executing long commands. Read the endpoint
            // snapshot without waiting for their engine locks.
            let (host, port) = dut
                .endpoint
                .lock()
                .map_err(|_| "Endpoint snapshot lock poisoned")?
                .clone();
            if let Some(config) = desired.remove(name)
                && (endpoint_identity(&host, &port)
                    != endpoint_identity(&config.dev_host_ip(), &config.serial_target())
                    || dut.engine.flags.removed.load(Ordering::Acquire))
            {
                let mut engine = dut.engine.lock().await;
                engine.retire_from_project().await;
                released.push(format!("{host}:{port}"));
                // Every handler retains this SharedEngine and its flags. Swap
                // the stopped engine in place so existing MCP sessions follow
                // the saved port rather than an obsolete engine instance.
                let mut replacement = crate::serial_engine::SerialEngine::new(config);
                engine
                    .state
                    .transfer_state_change_hook(&mut replacement.state);
                replacement.flags = dut.engine.flags.clone();
                replacement
                    .flags
                    .startup_in_progress
                    .store(true, Ordering::Release);
                replacement.flags.removed.store(false, Ordering::Release);
                *dut.endpoint
                    .lock()
                    .map_err(|_| "Endpoint snapshot lock poisoned")? = (
                    replacement.config.dev_host_ip(),
                    replacement.config.serial_target(),
                );
                *engine = replacement;
                drop(engine);
                restart.push(dut.engine.clone());
            } else if !endpoints.contains(&endpoint_identity(&host, &port)) {
                let mut engine = dut.engine.lock().await;
                engine.retire_from_project().await;
                released.push(format!("{host}:{port}"));
            }
        }
        // Release every old endpoint before claiming replacements, including
        // configurations that exchange ports between boards.
        let mut tasks = self.restart_tasks.lock().await;
        tasks.retain(|task| !task.is_finished());
        for engine in restart {
            tasks.push(tokio::spawn(async move {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(35),
                    crate::serial_engine::start_shared(engine.clone()),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "Could not connect to saved serial endpoint")
                    }
                    Err(_) => crate::serial_engine::record_startup_timeout(&engine, 35).await,
                }
            }));
        }
        Ok(released)
    }

    pub async fn stop(&self) {
        let _gate = self.reconcile_gate.lock().await;
        // A pending reload must never start a connection after shutdown.
        let tasks = std::mem::take(&mut *self.restart_tasks.lock().await);
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::error!(%error, "Serial reload task failed");
            }
        }
        futures_util::future::join_all(self.entries.values().map(|dut| async {
            dut.engine.lock().await.stop().await;
        }))
        .await;
    }
}

fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    // Resolve existing ancestor symlinks even when the DUT directory has
    // not been created. Dot components are normalized before comparison.
    let mut result = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            other => {
                result.push(other.as_os_str());
                if let Ok(canonical) = result.canonicalize() {
                    result = canonical;
                }
            }
        }
    }
    result
}

/// Compare effective endpoints independently of numeric spelling.
pub fn endpoint_identity(host: &str, port: &str) -> (String, String) {
    (
        crate::lock_manager::canonical_host(host),
        crate::lock_manager::canonical_target(port),
    )
}

/// Encode a DUT selector for HTTP query strings and MCP resource URIs.
pub fn dut_query(name: &str) -> String {
    let mut url = reqwest::Url::parse("http://localhost/").expect("literal URL");
    url.query_pairs_mut().append_pair("dut", name);
    url.query().unwrap_or_default().to_string()
}

/// Reject duplicate selectors instead of silently choosing one target.
pub fn query_dut(query: Option<&str>) -> Result<Option<String>, String> {
    let mut url = reqwest::Url::parse("http://localhost/").expect("literal URL");
    url.set_query(query);
    let names: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "dut")
        .map(|(_, name)| name.into_owned())
        .collect();
    match names.as_slice() {
        [] => Ok(None),
        [name] if !name.is_empty() => Ok(Some(name.clone())),
        _ => Err("Expected exactly one non-empty dut selector".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(project: &std::path::Path, document: Value) -> Config {
        let path = project.join(".target.jsonc");
        std::fs::write(&path, document.to_string()).unwrap();
        Config {
            values: crate::config::defaults(),
            config_path: Some(path),
            project_dir: Some(project.to_path_buf()),
            format: crate::config::ConfigFormat::Jsonc,
        }
    }

    #[tokio::test]
    async fn boards_inherit_globals_without_inheriting_each_other() {
        let project = tempfile::tempdir().unwrap();
        let cfg = config(
            project.path(),
            json!({
                "monitor":{"hang_timeout":120}, "serial":{"baudrate":115200},
                "dev_hosts":[{"ip":"127.0.0.1","duts":[
                    {"dut_name":"a", "serial":{"port":31001,"baudrate":1500000,"rfc2217":true},
                     "target":{"login_user":"alice","login_pass":"a-only"},
                     "control":{"dev_ctrl":"ch340-relay"}, "relay":{"port":31003,"reset_ch":2}},
                    {"dut_name":"b", "serial":{"port":31002},"control":{"dev_ctrl":"none"}}
                ]}]
            }),
        );
        let registry = EngineRegistry::from_config(&cfg).unwrap();
        let a = registry.select(Some("a")).unwrap().engine.lock().await;
        let b = registry.select(Some("b")).unwrap().engine.lock().await;
        assert_eq!(a.config.login_pass(), "a-only");
        assert_eq!(b.config.login_pass(), "");
        assert_eq!(b.config.login_user(), "root");
        assert!(!b.config.serial_rfc2217());
        assert_eq!(b.config.get("SERIAL_BAUDRATE"), "115200");
        assert_eq!(b.config.reset_channel(), 0);
        assert_eq!(b.config.get("HANG_TIMEOUT"), "120");
        assert_ne!(a.config.dut_dir(), b.config.dut_dir());
        assert!(registry.select(None).is_err());
        drop(a);
        drop(b);
        registry.stop().await;
    }

    #[test]
    fn duplicate_endpoints_and_directories_fail_before_log_creation() {
        for same_endpoint in [false, true] {
            let project = tempfile::tempdir().unwrap();
            let cfg = config(
                project.path(),
                json!({"serial":{"baudrate":115200}, "dev_hosts":[{"ip":"127.0.0.1", "duts":[
                    {"dut_name":"a", "dut_dir":".dut-serial/shared", "serial":{"port":31001}},
                    {"dut_name":"b", "dut_dir":if same_endpoint {".dut-serial/b"} else {".dut-serial/./shared"}, "serial":{"port":if same_endpoint {"031001"} else {"31002"}}}
                ]}]}),
            );
            assert!(EngineRegistry::from_config(&cfg).is_err());
            assert!(!project.path().join(".dut-serial/shared/logs").exists());
        }
    }

    #[tokio::test]
    async fn stopping_during_reload_does_not_start_a_late_connection() {
        let project = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut document = json!({
            "lock_dir":project.path().join("locks"), "serial":{"baudrate":115200},
            "dev_hosts":[{"ip":"127.0.0.1","duts":[
                {"dut_name":"board","serial":{"port":if port == 2002 {2003} else {2002}},"control":{"dev_ctrl":"none"}}
            ]}]
        });
        let cfg = config(project.path(), document.clone());
        let registry = EngineRegistry::from_config(&cfg).unwrap();
        document["dev_hosts"][0]["duts"][0]["serial"]["port"] = json!(port);
        let path = cfg.config_path.as_ref().unwrap();
        std::fs::write(path, document.to_string()).unwrap();
        registry.reconcile_saved_config(path).await.unwrap();
        registry.stop().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        assert!(!registry.first().engine.lock().await.owns_serial_lock());
    }

    #[tokio::test]
    async fn saving_a_new_port_reconnects_the_existing_engine() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn device() -> (u16, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                stream.write_all(b"root@board:~# ").await.unwrap();
                let mut buffer = [0; 4096];
                while let Ok(count) = stream.read(&mut buffer).await {
                    if count == 0 {
                        break;
                    }
                    if stream.write_all(&buffer[..count]).await.is_err()
                        || stream.write_all(b"\r\nroot@board:~# ").await.is_err()
                    {
                        break;
                    }
                }
            });
            (port, task)
        }
        let project = tempfile::tempdir().unwrap();
        let (old_port, old_device) = device().await;
        let (new_port, new_device) = device().await;
        let mut document = json!({
            "lock_dir":project.path().join("locks"), "serial":{"baudrate":115200},
            "dev_hosts":[{"ip":"127.0.0.1","duts":[
                {"dut_name":"board","serial":{"port":old_port},"control":{"dev_ctrl":"none"}}
            ]}]
        });
        let cfg = config(project.path(), document.clone());
        let registry = EngineRegistry::from_config(&cfg).unwrap();
        let existing = registry.select(Some("board")).unwrap().engine.clone();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::serial_engine::start_shared(existing.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(existing.lock().await.owns_serial_lock());
        let notifications = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let notified = notifications.clone();
        existing
            .lock()
            .await
            .set_state_change_hook(Box::new(move |_| {
                notified.fetch_add(1, Ordering::Relaxed);
            }));

        document["dev_hosts"][0]["duts"][0]["serial"]["port"] = json!(new_port);
        let path = cfg.config_path.as_ref().unwrap();
        std::fs::write(path, document.to_string()).unwrap();
        assert_eq!(
            registry.reconcile_saved_config(path).await.unwrap(),
            vec![format!("127.0.0.1:{old_port}")]
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while existing.flags.startup_in_progress.load(Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(Arc::ptr_eq(
            &existing.flags,
            &registry.select(Some("board")).unwrap().engine.flags
        ));
        {
            let engine = existing.lock().await;
            assert_eq!(engine.config.serial_target(), new_port.to_string());
            assert!(engine.owns_serial_lock());
            assert!(!engine.flags.removed.load(Ordering::Acquire));
        }
        let lock_dir = project.path().join("locks");
        assert_eq!(
            crate::lock_manager::endpoint_lock_owner(&crate::lock_manager::endpoint_lock_path(
                "127.0.0.1",
                &old_port.to_string(),
                lock_dir.to_str().unwrap()
            )),
            None
        );
        assert!(
            notifications.load(Ordering::Relaxed) >= 2,
            "inventory notifications survive the reload"
        );
        assert!(
            registry
                .reconcile_saved_config(path)
                .await
                .unwrap()
                .is_empty(),
            "a repeated save keeps the new connection"
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), old_device)
            .await
            .unwrap()
            .unwrap();
        registry.stop().await;
        new_device.abort();
        let _ = new_device.await;
    }

    #[test]
    fn selectors_preserve_unicode_and_reject_ambiguity() {
        let name = "板 b & + ? #";
        assert_eq!(
            query_dut(Some(&dut_query(name))).unwrap(),
            Some(name.to_string())
        );
        assert!(query_dut(Some("dut=a&dut=b")).is_err());
        assert!(query_dut(Some("dut=")).is_err());
    }
}
