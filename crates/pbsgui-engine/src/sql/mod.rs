//! SQL Server topology detection and VDI streaming backup.
//!
//! Detection runs first and decides the backup strategy. The detection queries
//! below are run through a TDS client (tiberius). The actual backup byte stream
//! is driven over the Virtual Device Interface (VDI): a `BACKUP DATABASE/LOG ...
//! TO VIRTUAL_DEVICE = '<name>'` statement is issued through tiberius while a
//! native COM loop on `SQLVDI.dll` reads SQL's backup buffers and forwards them
//! to PBS. The VDI connection must be `sysadmin`.

use serde::{Deserialize, Serialize};

/// The detected deployment archetype of a SQL Server instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "topology", rename_all = "snake_case")]
pub enum Topology {
    /// A single instance on local storage.
    Standalone,
    /// A Failover Cluster Instance on WSFC with shared storage. Backups run
    /// against the virtual name; the physical node may change on failover.
    FailoverClusterInstance {
        virtual_name: String,
        current_node: String,
    },
    /// An Always On Availability Group replica. Backup type rules depend on the
    /// role: secondaries (pre SQL 2025) allow only COPY_ONLY full and regular
    /// log backups, never differentials.
    AvailabilityGroup {
        group_name: String,
        role: ReplicaRole,
        is_preferred_backup_replica: bool,
    },
}

/// The role of the local replica within an Availability Group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaRole {
    Primary,
    Secondary,
    Resolving,
}

/// Detection queries (Transact-SQL). Kept here so the backup logic and the
/// topology checks stay in one place.
pub mod queries {
    /// Is this a Failover Cluster Instance? (`IsClustered` = 1)
    pub const IS_CLUSTERED: &str = "SELECT CAST(SERVERPROPERTY('IsClustered') AS int)";

    /// Is Always On enabled on this instance? (`IsHadrEnabled` = 1)
    pub const IS_HADR_ENABLED: &str = "SELECT CAST(SERVERPROPERTY('IsHadrEnabled') AS int)";

    /// FCI node map (one row per WSFC node, with the current owner flagged).
    pub const CLUSTER_NODES: &str =
        "SELECT NodeName, status_description, is_current_owner FROM sys.dm_os_cluster_nodes";

    /// Availability Group membership and the local replica's role.
    pub const AG_REPLICA_STATE: &str = "\
SELECT ag.name AS ag_name, ag.automated_backup_preference_desc, \
       ar.replica_server_name, rs.role_desc, rs.is_local, \
       rs.synchronization_health_desc \
FROM sys.availability_groups ag \
JOIN sys.availability_replicas ar ON ar.group_id = ag.group_id \
JOIN sys.dm_hadr_availability_replica_states rs ON rs.replica_id = ar.replica_id";

    /// Per database recovery model, state, and (for AGs) whether this replica is
    /// the preferred backup replica.
    pub const DATABASE_BACKUP_STATE: &str = "\
SELECT d.name, d.recovery_model_desc, d.state_desc, d.log_reuse_wait_desc, \
       sys.fn_hadr_backup_is_preferred_replica(d.name) AS is_preferred_backup_replica \
FROM sys.databases d";
}

// TODO: pub async fn detect(conn: &mut Connection) -> anyhow::Result<Topology>
// TODO: native COM VDI device-set loop (Windows only) in a `vdi` submodule.
