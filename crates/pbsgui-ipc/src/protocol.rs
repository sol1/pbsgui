//! Messages exchanged between the GUI and the engine.
//!
//! The GUI sends one [`Request`] per connection; the engine replies with a
//! stream of [`Reply`] messages (newline-delimited JSON), ending in a terminal
//! one (see [`Reply::is_terminal`]), then closes.
//!
//! Secret handling: a [`Job`] never carries the PBS token secret. The secret
//! travels only on [`Request::SaveJob`] and is stored by the engine in the OS
//! credential store; [`Reply::Jobs`] returns jobs without it.

use serde::{Deserialize, Serialize};

/// Where a backup is sent: the PBS connection and snapshot identity. No secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PbsDestination {
    /// Full repository string, e.g. `user@pbs!token@host:8007:datastore`.
    pub repository: String,
    /// Expected server certificate SHA-256 fingerprint.
    pub fingerprint: String,
    /// Backup id (the snapshot group id).
    pub backup_id: String,
}

/// When a job runs automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Only on demand.
    Manual,
    /// Every `minutes` minutes.
    Interval { minutes: u32 },
    /// Every day at the given local time.
    Daily { hour: u8, minute: u8 },
}

/// A persisted backup job. Never contains the token secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub destination: PbsDestination,
    /// Files and folders to back up.
    pub sources: Vec<String>,
    /// Optional glob patterns to exclude.
    #[serde(default)]
    pub excludes: Vec<String>,
    pub schedule: Schedule,
    /// Last run time, unix seconds.
    #[serde(default)]
    pub last_run: Option<i64>,
    /// Outcome of the last run ("ok" or an error message).
    #[serde(default)]
    pub last_status: Option<String>,
}

/// Summary of a snapshot for the browse view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    /// Backup time, unix seconds.
    pub backup_time: i64,
    /// Total archive size in bytes, if known.
    #[serde(default)]
    pub size: Option<u64>,
}

/// A file inside a snapshot's archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
}

/// A message from the GUI to the engine.
// SaveJob carries a whole Job, so the enum's largest variant dominates its size.
// These messages are sent once per connection, so the size is not a concern.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check.
    Ping,
    /// List all saved jobs (without secrets).
    ListJobs,
    /// Create or update a job (matched by id). If `secret` is `Some`, it is
    /// stored in the credential store; if `None`, any existing secret is kept.
    SaveJob {
        job: Job,
        #[serde(default)]
        secret: Option<String>,
    },
    /// Delete a job and its stored secret.
    DeleteJob { id: String },
    /// Run a saved job now; the engine streams progress until it finishes.
    RunJob { id: String },
    /// List snapshots for a job's backup group, by date/time.
    ListSnapshots { job_id: String },
    /// List the files inside a snapshot's archive.
    ListFiles { job_id: String, backup_time: i64 },
    /// Restore a snapshot to `destination`. `files` is `None` for a full restore,
    /// or the selected paths for a partial restore. Streams progress.
    Restore {
        job_id: String,
        backup_time: i64,
        #[serde(default)]
        files: Option<Vec<String>>,
        destination: String,
    },
}

/// A message from the engine to the GUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// Reply to [`Request::Ping`].
    Pong,
    /// Reply to [`Request::ListJobs`].
    Jobs { jobs: Vec<Job> },
    /// Reply to [`Request::SaveJob`].
    Saved { id: String },
    /// Reply to [`Request::DeleteJob`].
    Deleted,
    /// Reply to [`Request::ListSnapshots`].
    Snapshots { snapshots: Vec<SnapshotInfo> },
    /// Reply to [`Request::ListFiles`].
    Files { files: Vec<FileInfo> },
    /// A job run was accepted; progress follows.
    Accepted { job_id: String },
    /// Progress update (0.0 to 1.0) with a status line.
    Progress { fraction: f32, message: String },
    /// A line of log output.
    Log { line: String },
    /// Terminal: a job run finished.
    Finished { success: bool, message: String },
    /// Terminal: the request failed.
    Error { message: String },
}

impl Reply {
    /// Whether this reply ends the stream.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Reply::Pong
                | Reply::Jobs { .. }
                | Reply::Saved { .. }
                | Reply::Deleted
                | Reply::Snapshots { .. }
                | Reply::Files { .. }
                | Reply::Finished { .. }
                | Reply::Error { .. }
        )
    }
}
