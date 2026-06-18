//! Backup job and target model.

use serde::{Deserialize, Serialize};

/// The kind of backup to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupKind {
    /// Full database backup (sets the differential base unless `copy_only`).
    SqlFull,
    /// Differential backup since the last full.
    SqlDifferential,
    /// Transaction log backup (FULL / BULK_LOGGED recovery models only).
    SqlLog,
    /// Full filesystem backup of one or more paths via a VSS snapshot.
    FilesystemFull,
}

/// What to back up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum Target {
    /// A single SQL Server database on a named instance.
    SqlDatabase {
        /// Instance name, e.g. `MSSQLSERVER` or `HOST\SQLEXPRESS`.
        instance: String,
        /// Database name.
        database: String,
    },
    /// A set of filesystem paths.
    Filesystem { paths: Vec<String> },
}

/// A request to run a backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRequest {
    pub target: Target,
    pub kind: BackupKind,
    /// Take a COPY_ONLY backup so the normal backup chain is not disturbed.
    /// Required for full backups on an Availability Group secondary replica.
    #[serde(default)]
    pub copy_only: bool,
}
