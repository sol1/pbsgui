//! Messages exchanged between the GUI and the engine.
//!
//! The GUI sends one [`Request`] per connection; the engine replies with a
//! stream of [`Reply`] messages (newline-delimited JSON), ending in a terminal
//! one (see [`Reply::is_terminal`]), then closes.
//!
//! Secret handling: jobs carry no secrets. A job references saved connections
//! (a [`SqlConnection`] and/or a [`PbsServer`]) by id, and each connection's
//! secret is stored by the engine in the OS credential store.

use serde::{Deserialize, Serialize};

/// A saved Proxmox Backup Server connection (a backup destination). Managed
/// independently of jobs; the API token secret is stored separately under
/// `pbs:<id>` in the credential store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PbsServer {
    pub id: String,
    pub name: String,
    /// Repository string, e.g. `user@pbs!token@host:8007:datastore`.
    pub repository: String,
    /// Expected server certificate SHA-256 fingerprint.
    pub fingerprint: String,
}

/// The backup type for a SQL Server source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlBackupType {
    Full,
    Differential,
    Log,
}

/// What a job backs up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobSource {
    /// Files and folders on disk.
    Files {
        sources: Vec<String>,
        #[serde(default)]
        excludes: Vec<String>,
        #[serde(default)]
        change_detection: bool,
    },
    /// One or more SQL Server databases via a saved connection.
    Sql {
        connection_id: String,
        databases: Vec<String>,
        backup_type: SqlBackupType,
        #[serde(default)]
        copy_only: bool,
    },
}

/// Where a job sends its backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobDestination {
    /// A saved PBS server. `backup_id` is the snapshot group; for a SQL source
    /// with several databases it is the group prefix (one group per database).
    Pbs {
        server_id: String,
        backup_id: String,
    },
    /// A local or network folder (e.g. for SQL `.bak` files).
    Folder { path: String },
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

/// A persisted backup job. References saved connections by id; carries no
/// secrets (those live with the SQL connection and PBS server).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub source: JobSource,
    pub destination: JobDestination,
    pub schedule: Schedule,
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
    /// Whether the backup is client-side encrypted (AES-256-GCM, the PBS scheme).
    /// The key is stored separately under `enc:<id>` in the credential store and
    /// never travels in this struct; restores decrypt transparently using it.
    #[serde(default)]
    pub encrypted: bool,
}

/// A backup encryption key, for display and import. `key` is the raw key the
/// user copies into a password manager; `fingerprint` identifies which key a
/// backup needs (the PBS key-fingerprint scheme), so two keys can be told apart
/// without revealing either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionKeyInfo {
    /// Base64 of the 32-byte key.
    pub key: String,
    /// Colon-grouped lowercase hex of the key fingerprint.
    pub fingerprint: String,
}

/// Transport security for the SMTP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailSecurity {
    /// Plain connection upgraded with STARTTLS (typical on port 587).
    Starttls,
    /// Implicit TLS for the whole connection (typical on port 465).
    Tls,
    /// No transport security (for a trusted local relay only).
    None,
}

/// Email (SMTP) notification settings. The password is stored separately in the
/// credential store under `notify:smtp`, never in this struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailSettings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub security: EmailSecurity,
    /// SMTP username; empty for an unauthenticated relay.
    #[serde(default)]
    pub username: String,
    /// From address (may be `Name <addr@host>`).
    pub from: String,
    /// Recipient addresses.
    #[serde(default)]
    pub to: Vec<String>,
}

/// Webhook notification settings. The URL is stored separately in the credential
/// store under `notify:webhook` (it is a capability secret). The payload is JSON
/// with a `text` summary (so a Slack incoming webhook renders it) plus structured
/// fields for generic consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookSettings {
    pub enabled: bool,
}

/// Global notification settings. Secrets (SMTP password, webhook URL) live in the
/// credential store, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSettings {
    /// Notify when a job finishes successfully (including "no changes").
    #[serde(default)]
    pub on_success: bool,
    /// Notify when a job fails.
    #[serde(default = "default_true")]
    pub on_failure: bool,
    pub email: EmailSettings,
    pub webhook: WebhookSettings,
}

fn default_true() -> bool {
    true
}

/// A notification channel, for the Test action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyChannel {
    Email,
    Webhook,
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

/// A saved SQL Server connection (managed in the SQL Servers tab). Any password
/// is stored separately under `sql:<id>` in the credential store; `Integrated`
/// auth needs none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlConnection {
    pub id: String,
    pub name: String,
    /// Connection target: "HOST" or "HOST\\INSTANCE".
    pub server: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub auth: SqlAuth,
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

/// The outcome of a single readiness check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

/// One readiness check for backing up an instance, with a fix hint on failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    /// How to fix it (shown when the check is not Ok).
    #[serde(default)]
    pub hint: Option<String>,
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
    /// Create or update a job (matched by id). Secrets live with the connections
    /// the job references, not the job.
    SaveJob { job: Job },
    /// Delete a job.
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
    /// Run readiness checks against an instance (connectivity, login identity,
    /// and the sysadmin role VDI requires), returning a hint for each failure.
    CheckSql {
        server: String,
        #[serde(default)]
        port: Option<u16>,
        auth: SqlAuth,
        #[serde(default)]
        password: Option<String>,
    },
    /// Back up a database over VDI to a local `.bak` file (the validation step
    /// before streaming to PBS). Streams progress; the connection must be
    /// `sysadmin`.
    BackupSqlToFile {
        server: String,
        #[serde(default)]
        port: Option<u16>,
        auth: SqlAuth,
        #[serde(default)]
        password: Option<String>,
        database: String,
        output_path: String,
    },
    /// Back up a database over VDI, streaming it to PBS as a deduplicated
    /// snapshot. Sends it to the saved PBS server `pbs_server_id` (its
    /// repository, fingerprint, and stored secret); `backup_id` is the snapshot
    /// group. Streams progress.
    BackupSqlToPbs {
        server: String,
        #[serde(default)]
        port: Option<u16>,
        auth: SqlAuth,
        #[serde(default)]
        password: Option<String>,
        database: String,
        pbs_server_id: String,
        backup_id: String,
    },

    /// List the PBS snapshots for one database of a SQL backup job, by date/time.
    ListSqlSnapshots { job_id: String, database: String },
    /// Restore a SQL database snapshot via VDI. `target_database` is where it is
    /// restored (the original name, or a new one). Streams progress.
    RestoreSql {
        job_id: String,
        database: String,
        backup_time: i64,
        target_database: String,
    },

    /// List saved SQL Server connections (without secrets).
    ListSqlConnections,
    /// Create or update a SQL connection. `secret` (a password) is stored when
    /// present; `None` keeps any existing secret.
    SaveSqlConnection {
        connection: SqlConnection,
        #[serde(default)]
        secret: Option<String>,
    },
    /// Delete a SQL connection and its stored secret.
    DeleteSqlConnection { id: String },

    /// List saved PBS servers (without secrets).
    ListPbsServers,
    /// Create or update a PBS server. `secret` (the token secret) is stored when
    /// present; `None` keeps any existing secret.
    SavePbsServer {
        server: PbsServer,
        #[serde(default)]
        secret: Option<String>,
    },
    /// Delete a PBS server and its stored secret.
    DeletePbsServer { id: String },

    /// Generate a fresh random encryption key for a job and store it under
    /// `enc:<job_id>`. Replies with the key (for the user to copy to a password
    /// manager) and its fingerprint. Fails if a key already exists.
    GenerateEncryptionKey { job_id: String },
    /// Import an existing base64 encryption key for a job (to reuse one key
    /// across jobs or machines), storing it under `enc:<job_id>`. Replies with
    /// the key and fingerprint.
    ImportEncryptionKey { job_id: String, key: String },
    /// Reveal a job's stored encryption key (to copy it again), or report that
    /// none is stored.
    GetEncryptionKey { job_id: String },
    /// Delete a job's stored encryption key.
    ClearEncryptionKey { job_id: String },

    /// Get the global notification settings (without secrets; flags report which
    /// secrets are stored).
    GetNotifications,
    /// Save the global notification settings. `smtp_password` / `webhook_url` are
    /// stored when present; `None` keeps the existing secret.
    SaveNotifications {
        settings: NotificationSettings,
        #[serde(default)]
        smtp_password: Option<String>,
        #[serde(default)]
        webhook_url: Option<String>,
    },
    /// Send a test notification through one channel, using the saved settings and
    /// secrets, and report the outcome.
    TestNotification { channel: NotifyChannel },
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
    /// Reply to [`Request::CheckSql`].
    SqlChecks { checks: Vec<SqlCheck> },
    /// Reply to [`Request::ListSqlConnections`].
    SqlConnections { connections: Vec<SqlConnection> },
    /// Reply to [`Request::ListPbsServers`].
    PbsServers { servers: Vec<PbsServer> },
    /// Reply to the encryption-key requests. `info` is `None` when the job has
    /// no stored key (a `GetEncryptionKey` miss, or after `ClearEncryptionKey`).
    EncryptionKey { info: Option<EncryptionKeyInfo> },
    /// Reply to [`Request::GetNotifications`]. The flags report whether each
    /// secret is stored, so the UI can show "set" without revealing it.
    Notifications {
        settings: NotificationSettings,
        has_smtp_password: bool,
        has_webhook_url: bool,
    },
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
                | Reply::SqlChecks { .. }
                | Reply::SqlConnections { .. }
                | Reply::PbsServers { .. }
                | Reply::EncryptionKey { .. }
                | Reply::Notifications { .. }
                | Reply::Finished { .. }
                | Reply::Error { .. }
        )
    }
}
