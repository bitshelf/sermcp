//! Typed error system — pattern from adancurusul/serial-mcp-server.
//!
//! Provides SerialError with log/metric categories (category()), replacing
//! ad-hoc json!({"error":...}). Only variants that are actually constructed
//! are kept; the recovery-hint layer (is_recoverable) was removed when the
//! MCP layer moved to rmcp's ErrorData.

use thiserror::Error;

/// Main error type for serial MCP operations.
#[derive(Error, Debug)]
pub enum SerialError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("RFC 2217 negotiation error: {0}")]
    TelnetNegotiation(String),

    #[error("RFC 2217 negotiation/ack timed out")]
    NegotiationTimeout,

    #[error("Baud rate mismatch: requested {requested}, server applied {actual}")]
    BaudMismatch { requested: u32, actual: u32 },

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl SerialError {
    /// Error category for logging and metrics.
    pub fn category(&self) -> &'static str {
        match self {
            SerialError::ConnectionFailed(_) => "connection",
            SerialError::NegotiationTimeout => "communication",
            SerialError::TelnetNegotiation(_) => "communication",
            SerialError::BaudMismatch { .. } => "communication",
            SerialError::InvalidConfig(_) => "configuration",
            SerialError::Io(_) => "system",
        }
    }
}
