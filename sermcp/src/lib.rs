//! sermcp — shared library for MCP server and dutabo CLI.
//!
//! Public API for use by binaries (`main.rs`, `bin/dutabo.rs`).

pub mod ansi_strip;
pub mod boot_detector;
pub mod command_queue;
pub mod config;
pub mod connection_learner;
pub mod console;
pub mod dut_state;
pub mod error;
pub mod flash;
pub mod highlight;
pub mod init;
pub mod inotify_watcher;
pub mod loader;
pub mod lock_manager;
pub mod log_manager;
pub mod marker;
pub mod mcp_handler;
pub mod mcp_http;
pub mod ports;
pub use dut_ctrl as power_control;
pub mod reconnect;
pub mod serial_engine;
pub mod serial_term;
pub mod serial_text_detector;
pub mod similarity;
pub mod state_manager;
pub mod stdio_bridge;
pub mod stdio_transport;
pub mod task_manager;
pub mod telnet_filter;
pub mod tools;
pub mod trace_perfetto;
// Compatibility for downstream callers while the public module name migrates.
pub use trace_perfetto as trace_chrome;
pub mod uboot;
pub mod uefi;

pub mod agent_deploy;
pub mod project_setup;
pub mod skill_catalog;
