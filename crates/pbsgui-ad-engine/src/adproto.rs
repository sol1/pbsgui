//! The AD engine's IPC protocol.
//!
//! Deliberately small for now: enough for a GUI to reach the engine and identify
//! it. It grows alongside the capabilities: job CRUD and RunJob (M4), snapshot
//! listing and the `ntds.dit` tree browse/diff (M6), and restore (M7). Kept in the
//! engine crate until a second consumer (the AD GUI, M8) needs it, at which point
//! it moves to a shared crate like `pbsgui-ipc`.
//!
//! Uses the generic transport in `pbsgui-ipc` ([`pbsgui_ipc::serve_typed`]), so it
//! defines only the message types, not the wire handling.

use serde::{Deserialize, Serialize};

/// A request from the GUI to the AD engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdRequest {
    /// Liveness check.
    Ping,
    /// Ask the engine to identify itself.
    Version,
}

/// A reply from the AD engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdReply {
    Pong,
    Version { version: String },
    Error { message: String },
}

impl pbsgui_ipc::ErrorReply for AdReply {
    fn error(message: String) -> Self {
        AdReply::Error { message }
    }
}
