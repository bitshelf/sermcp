use super::*;
use crate::config::{Config, ConfigFormat};
use crate::engine_registry::EngineRegistry;

#[tokio::test]
async fn catalog_unions_capabilities_but_dispatch_checks_the_selected_dut() {
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join(".target.jsonc");
    std::fs::write(
        &path,
        json!({
            "serial":{"baudrate":115200},
            "dev_hosts":[{"ip":"127.0.0.1","duts":[
                {"dut_name":"a","serial":{"port":31001},"control":{"dev_ctrl":"none"}},
                {"dut_name":"b","serial":{"port":31002},"control":{"dev_ctrl":"ch340-relay"},
                 "relay":{"port":31003,"reset_ch":1,"power_ch":2}}
            ]}]
        })
        .to_string(),
    )
    .unwrap();
    let config = Config {
        values: crate::config::defaults(),
        config_path: Some(path),
        project_dir: Some(project.path().to_path_buf()),
        format: ConfigFormat::Jsonc,
    };
    let registry = EngineRegistry::from_config(&config).unwrap();
    // Build a session while startup holds the primary engine mutex. The
    // registry's pre-start capability snapshots must still be authoritative.
    let _busy = registry.first().engine.lock().await;
    let handler = McpHandler::new(
        registry.first().engine.clone(),
        crate::task_manager::TaskManager::new(),
    )
    .with_registry(registry.clone());
    assert!(Arc::ptr_eq(&handler.tasks, &registry.tasks));
    let tools = handler.visible_tools(ToolProfile::Agent);
    assert!(tools.iter().any(|tool| tool.name == "serial_power_cycle"));
    assert!(!tools.iter().any(|tool| tool.name == "serial_set_baud"));
    for (name, expected) in [("a", false), ("b", true)] {
        let mut request: CallToolRequestParams = serde_json::from_value(json!({
            "name":"serial_power_cycle", "arguments":{"dut":name}
        }))
        .unwrap();
        let selected = handler.route_request(&mut request).unwrap();
        assert_eq!(
            selected.tool_visible("serial_power_cycle", ToolProfile::Agent),
            expected
        );
        assert!(!request.arguments.unwrap().contains_key("dut"));
    }
    assert!(handler.takeover_rejection("serial_list_duts").is_none());
    let discovery = tokio::time::timeout(
        Duration::from_millis(200),
        handler.list_duts(rmcp::handler::server::wrapper::Parameters(
            crate::tools::params::NoArgs {},
        )),
    )
    .await
    .expect("discovery must not wait for an engine lock")
    .unwrap();
    assert_ne!(discovery.is_error, Some(true));
    drop(_busy);
    registry.stop().await;
}
