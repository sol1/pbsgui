//! IPC protocol between the GUI and the engine.
//!
//! The unprivileged Tauri GUI connects to the engine over a Windows named pipe
//! and exchanges newline-delimited JSON messages. The GUI sends [`Request`]s and
//! receives [`Response`]s; long running jobs also stream [`Event`]s so the UI can
//! show live progress bars and logs.

use serde::{Deserialize, Serialize};

use crate::jobs::BackupRequest;

/// Default named pipe the engine listens on.
pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\pbsgui-engine";

/// A request from the GUI to the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check.
    Ping,
    /// Enumerate SQL Server instances and their detected topology.
    ListSqlInstances,
    /// Start a backup job. The engine replies with [`Response::Accepted`] and
    /// then streams [`Event`]s until the job finishes.
    StartBackup(BackupRequest),
    /// Request cancellation of a running job.
    CancelJob { job_id: String },
}

/// An immediate reply to a [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    /// Reply to [`Request::Ping`].
    Pong,
    /// A job was accepted; progress will arrive as [`Event`]s.
    Accepted { job_id: String },
    /// The request failed.
    Error { message: String },
}

/// An asynchronous event streamed for a running job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Progress update (0.0 to 1.0) with a human readable status line.
    Progress {
        job_id: String,
        fraction: f32,
        message: String,
    },
    /// A line of log output for the job.
    Log { job_id: String, line: String },
    /// The job finished.
    Finished {
        job_id: String,
        success: bool,
        message: String,
    },
}
