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
    /// Skip the run if no source file changed (by size + mtime) since last run.
    #[serde(default)]
    pub change_detection: bool,
    /// Command run before the backup; a non-zero exit aborts the job. Empty = none.
    #[serde(default)]
    pub pre_script: Option<String>,
    /// Command run after the backup, with the job status in the environment.
    #[serde(default)]
    pub post_script: Option<String>,
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

/// How an instance was found during discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlDiscoverySource {
    /// Listed in the local registry (instance names hive).
    LocalRegistry,
    /// Found via the SQL Server Browser (UDP 1434).
    Browser,
    /// Found by a network host/subnet scan.
    NetworkScan,
    /// Found via an Active Directory SPN lookup.
    ActiveDirectory,
}

/// Which login types an instance accepts, from its `LoginMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlAuthMode {
    /// Windows authentication only.
    WindowsOnly,
    /// Mixed mode: Windows and SQL logins.
    Mixed,
    /// Not determined (e.g. a remote instance not yet probed).
    Unknown,
}

/// How to authenticate to a SQL Server instance for a connection.
///
/// `Integrated` carries no secret (the engine's service identity is used). The
/// others name a principal; any password is stored separately in the OS
/// credential store, never in this struct (mirrors [`Job`] secret handling).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SqlAuth {
    /// Windows integrated auth as the engine's service account.
    Integrated,
    /// Windows integrated auth as an explicit account.
    WindowsAccount { username: String },
    /// SQL Server authentication.
    SqlLogin { username: String },
    /// Azure AD / Entra token-based auth.
    AzureAd { username: String },
}

/// The detected deployment archetype of a SQL Server instance (probe result).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "topology", rename_all = "snake_case")]
pub enum SqlTopology {
    /// A single instance on local storage.
    Standalone,
    /// A Failover Cluster Instance; back up against the virtual name.
    FailoverClusterInstance {
        virtual_name: String,
        current_node: String,
    },
    /// An Always On Availability Group replica.
    AvailabilityGroup {
        group_name: String,
        /// "primary", "secondary", or "resolving".
        role: String,
        is_preferred_backup_replica: bool,
    },
}

/// A database on a discovered instance (filled by the probe step).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlDatabase {
    pub name: String,
    /// "SIMPLE", "FULL", or "BULK_LOGGED".
    pub recovery_model: String,
    /// "ONLINE", "OFFLINE", "RESTORING", etc.
    pub state: String,
    /// Why the log is not truncating, if applicable (`log_reuse_wait_desc`).
    #[serde(default)]
    pub log_reuse_wait: Option<String>,
    /// Whether the database is in an Availability Group.
    #[serde(default)]
    pub in_availability_group: bool,
    /// Whether this replica is the preferred backup replica for the database.
    #[serde(default)]
    pub is_preferred_backup_replica: Option<bool>,
}

/// Details obtained by connecting to an instance (the probe step).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlProbe {
    /// `SERVERPROPERTY('ProductVersion')`.
    pub product_version: String,
    /// `SERVERPROPERTY('Edition')`.
    pub edition: String,
    pub topology: SqlTopology,
    pub databases: Vec<SqlDatabase>,
}

/// One discovered SQL Server instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlInstance {
    /// Connection target: "HOST" for the default instance or "HOST\\NAME".
    pub server: String,
    /// Instance name ("MSSQLSERVER" for the default instance).
    pub instance_name: String,
    /// The machine hosting the instance (virtual name for an FCI).
    pub host: String,
    /// TCP port, if known from the registry/Browser.
    #[serde(default)]
    pub port: Option<u16>,
    /// How the instance was found.
    pub source: SqlDiscoverySource,
    /// Service running state (local discovery only).
    #[serde(default)]
    pub running: Option<bool>,
    /// The service account the instance runs as, if known.
    #[serde(default)]
    pub service_account: Option<String>,
    /// Login types the instance accepts (from `LoginMode`).
    #[serde(default = "unknown_auth_mode")]
    pub auth_mode: SqlAuthMode,
    /// Whether the instance is flagged clustered (refined to FCI/AG by the probe).
    #[serde(default)]
    pub clustered: Option<bool>,
    /// Whether the TCP/IP protocol is enabled (from the registry). When this is
    /// `Some(false)`, a probe cannot connect until TCP/IP is enabled in SQL
    /// Server Configuration Manager and the service is restarted.
    #[serde(default)]
    pub tcp_enabled: Option<bool>,
    /// Connection result, present once the instance has been probed.
    #[serde(default)]
    pub probe: Option<SqlProbe>,
    /// If probing failed, why (so the UI can show "found but unreachable").
    #[serde(default)]
    pub probe_error: Option<String>,
}

fn unknown_auth_mode() -> SqlAuthMode {
    SqlAuthMode::Unknown
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
    /// Discover SQL Server instances. Local enumeration always runs; when
    /// `include_network` is set, the engine also probes the Browser, scans the
    /// given `targets` (hosts or subnets), and checks Active Directory.
    DiscoverSql {
        #[serde(default)]
        include_network: bool,
        #[serde(default)]
        targets: Vec<String>,
    },
    /// Connect to one instance and report its version, topology, and databases.
    /// `password` is required for SQL and explicit-Windows logins; `Integrated`
    /// uses the engine's service identity and needs none.
    ProbeSql {
        server: String,
        #[serde(default)]
        port: Option<u16>,
        auth: SqlAuth,
        #[serde(default)]
        password: Option<String>,
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
    /// Reply to [`Request::DiscoverSql`].
    SqlInstances { instances: Vec<SqlInstance> },
    /// Reply to [`Request::ProbeSql`].
    SqlProbe { probe: SqlProbe },
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
                | Reply::SqlInstances { .. }
                | Reply::SqlProbe { .. }
                | Reply::Finished { .. }
                | Reply::Error { .. }
        )
    }
}
