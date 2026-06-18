//! Messages exchanged between the GUI and the engine.
//!
//! The GUI sends one [`Request`] per connection; the engine replies with a
//! stream of [`Reply`] messages (newline-delimited JSON), ending in a terminal
//! one ([`Reply::Pong`], [`Reply::Finished`], or [`Reply::Error`]), then closes.

use serde::{Deserialize, Serialize};

/// Where a backup is sent: the PBS connection and snapshot identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PbsDestination {
    /// Full repository string, e.g. `user@pbs!token@host:8007:datastore`.
    pub repository: String,
    /// API token secret.
    pub secret: String,
    /// Expected server certificate SHA-256 fingerprint.
    pub fingerprint: String,
    /// Backup id (the snapshot group id).
    pub backup_id: String,
}

/// The kind of backup to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupKind {
    SqlFull,
    SqlDifferential,
    SqlLog,
    FilesystemFull,
}

/// What to back up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum Target {
    /// A SQL Server database on a named instance.
    SqlDatabase { instance: String, database: String },
    /// A set of filesystem paths.
    Filesystem { paths: Vec<String> },
}

/// A request to run a backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRequest {
    pub target: Target,
    pub kind: BackupKind,
    /// Take a COPY_ONLY backup (required for full backups on an AG secondary).
    #[serde(default)]
    pub copy_only: bool,
}

/// A message from the GUI to the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check.
    Ping,
    /// Start a backup; the engine streams [`Reply`] progress until it finishes.
    StartBackup {
        destination: PbsDestination,
        job: BackupRequest,
    },
}

/// A message from the engine to the GUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// Reply to [`Request::Ping`].
    Pong,
    /// A backup was accepted; progress follows.
    Accepted { job_id: String },
    /// Progress update (0.0 to 1.0) with a status line.
    Progress { fraction: f32, message: String },
    /// A line of log output.
    Log { line: String },
    /// Terminal: the job finished.
    Finished { success: bool, message: String },
    /// Terminal: the request failed.
    Error { message: String },
}

impl Reply {
    /// Whether this reply ends the stream.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Reply::Pong | Reply::Finished { .. } | Reply::Error { .. }
        )
    }
}
