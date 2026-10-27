//! Connecting to a SQL Server instance and reading its identity, topology, and
//! databases (the discovery probe step).
//!
//! Connection uses tiberius over TLS. Integrated auth (the engine's service
//! identity) is the common on-host path and needs no credentials; SQL and
//! explicit-Windows logins take a password. Topology is classified from the
//! public `SERVERPROPERTY` values, so it works without elevated DMV rights;
//! Availability Group details are read best-effort.

use anyhow::Context;
use pbsgui_ipc::{SqlAuth, SqlDatabase, SqlProbe, SqlTopology};
use tiberius::{AuthMethod, Client, Config, Row};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub(crate) type SqlClient = Client<Compat<TcpStream>>;

const IDENTITY_SQL: &str = "\
SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), \
       CAST(SERVERPROPERTY('Edition') AS nvarchar(128)), \
       CAST(SERVERPROPERTY('IsClustered') AS int), \
       CAST(SERVERPROPERTY('IsHadrEnabled') AS int), \
       CAST(SERVERPROPERTY('MachineName') AS nvarchar(128)), \
       CAST(ISNULL(SERVERPROPERTY('ComputerNamePhysicalNetBIOS'), \
                   SERVERPROPERTY('MachineName')) AS nvarchar(128))";

const DATABASES_SQL: &str = "\
SELECT d.name, d.recovery_model_desc, d.state_desc, d.log_reuse_wait_desc, \
       CASE WHEN drs.database_id IS NOT NULL THEN 1 ELSE 0 END, \
       CAST(sys.fn_hadr_backup_is_preferred_replica(d.name) AS int), \
       CASE WHEN d.database_id <= 4 THEN 1 ELSE 0 END \
FROM sys.databases d \
LEFT JOIN sys.dm_hadr_database_replica_states drs \
       ON drs.database_id = d.database_id AND drs.is_local = 1 \
WHERE d.database_id <> 2 \
ORDER BY d.name";

const AG_DETAILS_SQL: &str = "\
SELECT TOP 1 ag.name, rs.role_desc \
FROM sys.availability_groups ag \
JOIN sys.availability_replicas ar ON ar.group_id = ag.group_id \
JOIN sys.dm_hadr_availability_replica_states rs ON rs.replica_id = ar.replica_id \
WHERE rs.is_local = 1";

/// Connect to `server` and report its version, topology, and databases.
pub async fn probe(
    server: &str,
    port: Option<u16>,
    auth: &SqlAuth,
    password: Option<&str>,
) -> anyhow::Result<SqlProbe> {
    let mut client = connect(server, port, auth, password).await?;
    let identity = identity(&mut client).await?;
    let databases = databases(&mut client).await?;
    let topology = topology(&mut client, &identity, &databases).await;
    Ok(SqlProbe {
        product_version: identity.product_version,
        edition: identity.edition,
        topology,
        databases,
    })
}

struct Identity {
    product_version: String,
    edition: String,
    is_clustered: bool,
    is_hadr: bool,
    machine_name: String,
    physical_node: String,
}

async fn identity(client: &mut SqlClient) -> anyhow::Result<Identity> {
    let rows = client
        .simple_query(IDENTITY_SQL)
        .await?
        .into_first_result()
        .await?;
    let row = rows
        .into_iter()
        .next()
        .context("identity query returned no rows")?;
    Ok(Identity {
        product_version: string_at(&row, 0),
        edition: string_at(&row, 1),
        is_clustered: int_at(&row, 2) == 1,
        is_hadr: int_at(&row, 3) == 1,
        machine_name: string_at(&row, 4),
        physical_node: string_at(&row, 5),
    })
}

async fn databases(client: &mut SqlClient) -> anyhow::Result<Vec<SqlDatabase>> {
    let rows = client
        .simple_query(DATABASES_SQL)
        .await?
        .into_first_result()
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let in_ag = int_at(&row, 4) == 1;
            SqlDatabase {
                name: string_at(&row, 0),
                recovery_model: string_at(&row, 1),
                state: string_at(&row, 2),
                log_reuse_wait: row.get::<&str, _>(3).map(str::to_string),
                in_availability_group: in_ag,
                // Only meaningful for AG databases; on a standalone the function
                // returns 1 for every database, which would be misleading.
                is_preferred_backup_replica: in_ag.then(|| row.get::<i32, _>(5) == Some(1)),
                system: int_at(&row, 6) == 1,
            }
        })
        .collect())
}

/// Classify topology from the public `SERVERPROPERTY` flags, reading AG details
/// best-effort (they need DMV rights the connection may lack).
async fn topology(client: &mut SqlClient, id: &Identity, dbs: &[SqlDatabase]) -> SqlTopology {
    if id.is_hadr {
        let (group_name, role) = ag_details(client)
            .await
            .unwrap_or_else(|_| ("unknown".to_string(), "unknown".to_string()));
        SqlTopology::AvailabilityGroup {
            group_name,
            role,
            is_preferred_backup_replica: dbs
                .iter()
                .any(|d| d.is_preferred_backup_replica == Some(true)),
        }
    } else if id.is_clustered {
        SqlTopology::FailoverClusterInstance {
            virtual_name: id.machine_name.clone(),
            current_node: id.physical_node.clone(),
        }
    } else {
        SqlTopology::Standalone
    }
}

async fn ag_details(client: &mut SqlClient) -> anyhow::Result<(String, String)> {
    let rows = client
        .simple_query(AG_DETAILS_SQL)
        .await?
        .into_first_result()
        .await?;
    let row = rows
        .into_iter()
        .next()
        .context("no local availability replica")?;
    Ok((string_at(&row, 0), string_at(&row, 1).to_lowercase()))
}

pub(crate) async fn connect(
    server: &str,
    port: Option<u16>,
    auth: &SqlAuth,
    password: Option<&str>,
) -> anyhow::Result<SqlClient> {
    let host = server.split('\\').next().unwrap_or(server).to_string();
    let port = port.unwrap_or(1433);

    let mut config = Config::new();
    config.host(&host);
    config.port(port);
    config.authentication(auth_method(auth, password)?);
    // SQL Server's login-handshake certificate is typically self-signed.
    config.trust_cert();

    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    tcp.set_nodelay(true)?;
    Client::connect(config, tcp.compat_write())
        .await
        .context("SQL Server login failed")
}

/// What [`crate::sql::plan::decide`] needs to know about `database` here: its
/// availability group role, whether this is the preferred backup replica, its
/// recovery model, and whether a log backup can extend the chain.
///
/// The role comes from `sys.dm_hadr_availability_replica_states` (not
/// readability), so a non-readable secondary is still a secondary. This is how
/// pbsgui coordinates replicas WITHOUT any pbsgui-to-pbsgui connection: every
/// replica runs the same job and SQL Server answers consistently cluster-wide.
pub(crate) async fn db_state(
    client: &mut SqlClient,
    database: &str,
) -> anyhow::Result<crate::sql::plan::DbState> {
    use crate::sql::plan::{DbState, Role};
    let db = database.replace('\'', "''");
    let query = format!(
        "SELECT \
           CAST(CASE WHEN d.replica_id IS NULL THEN 0 ELSE 1 END AS int) AS in_ag, \
           CAST(CASE \
             WHEN d.replica_id IS NULL THEN 1 \
             WHEN sys.fn_hadr_backup_is_preferred_replica(d.name) = 1 THEN 1 \
             ELSE 0 END AS int) AS preferred, \
           rs.role_desc AS role, \
           CAST(CASE WHEN d.recovery_model_desc = 'SIMPLE' THEN 1 ELSE 0 END AS int) AS simple, \
           CAST(d.is_read_only AS int) AS read_only, \
           CAST(CASE WHEN drs.last_log_backup_lsn IS NULL THEN 0 ELSE 1 END AS int) AS log_chain \
         FROM sys.databases d \
         LEFT JOIN sys.dm_hadr_availability_replica_states rs \
           ON rs.replica_id = d.replica_id AND rs.is_local = 1 \
         LEFT JOIN sys.database_recovery_status drs ON drs.database_id = d.database_id \
         WHERE d.name = N'{db}'"
    );
    let row = client
        .simple_query(query)
        .await?
        .into_row()
        .await?
        .with_context(|| format!("database [{database}] not found on this instance"))?;
    let in_ag = row.get::<i32, _>("in_ag") == Some(1);
    let role = match (in_ag, row.get::<&str, _>("role")) {
        (false, _) => Role::Standalone,
        (true, Some(r)) if r.eq_ignore_ascii_case("PRIMARY") => Role::Primary,
        // SECONDARY, RESOLVING, or no local replica row: never act as primary.
        (true, _) => Role::Secondary,
    };
    let mut state = DbState {
        role,
        // Proceed unless SQL explicitly says this is not the preferred replica.
        preferred: row.get::<i32, _>("preferred") != Some(0),
        simple_recovery: row.get::<i32, _>("simple") == Some(1),
        read_only: row.get::<i32, _>("read_only") == Some(1),
        log_chain: row.get::<i32, _>("log_chain") == Some(1),
    };
    drop(row);
    // A Basic availability group (Standard edition) allows no backups on a
    // secondary, whatever the backup preference says, so only its primary is
    // the preferred replica.
    if in_ag && is_basic_ag(client, database).await {
        state.preferred = state.role == Role::Primary;
    }
    Ok(state)
}

/// Whether `database` is in a Basic availability group. The column only exists
/// from SQL Server 2016 (where Basic groups were introduced), so a failed query
/// means no.
async fn is_basic_ag(client: &mut SqlClient, database: &str) -> bool {
    let db = database.replace('\'', "''");
    let query = format!(
        "SELECT CAST(ag.basic_features AS int) AS basic \
         FROM sys.databases d \
         JOIN sys.availability_replicas ar ON ar.replica_id = d.replica_id \
         JOIN sys.availability_groups ag ON ag.group_id = ar.group_id \
         WHERE d.name = N'{db}'"
    );
    let row = match client.simple_query(query).await {
        Ok(stream) => stream.into_row().await.ok().flatten(),
        Err(_) => None,
    };
    row.and_then(|r| r.get::<i32, _>("basic")) == Some(1)
}

/// Estimate the backup size of `database` as its allocated data-file bytes
/// (`sys.master_files`, data files only; the log is excluded). The VDI backup
/// stream is roughly the *used* portion, so this is an approximate upper bound,
/// used only to show a progress percentage. Returns `None` if it cannot be read.
pub(crate) async fn data_size_bytes(client: &mut SqlClient, database: &str) -> Option<u64> {
    let db = database.replace('\'', "''");
    let query = format!(
        "SELECT CAST(SUM(CAST(size AS bigint)) * 8192 AS bigint) \
         FROM sys.master_files WHERE database_id = DB_ID(N'{db}') AND type = 0"
    );
    let row = client
        .simple_query(query)
        .await
        .ok()?
        .into_row()
        .await
        .ok()??;
    row.get::<i64, _>(0).filter(|&v| v > 0).map(|v| v as u64)
}

fn auth_method(auth: &SqlAuth, password: Option<&str>) -> anyhow::Result<AuthMethod> {
    match auth {
        SqlAuth::SqlLogin { username } => {
            let password = password.context("SQL login requires a password")?;
            Ok(AuthMethod::sql_server(username, password))
        }
        SqlAuth::Integrated => integrated_auth(),
        SqlAuth::WindowsAccount { username } => {
            let password = password.context("Windows account requires a password")?;
            windows_auth(username, password)
        }
        SqlAuth::AzureAd { .. } => anyhow::bail!("Azure AD authentication is not yet supported"),
    }
}

#[cfg(windows)]
fn integrated_auth() -> anyhow::Result<AuthMethod> {
    Ok(AuthMethod::Integrated)
}

#[cfg(not(windows))]
fn integrated_auth() -> anyhow::Result<AuthMethod> {
    anyhow::bail!("integrated authentication is only available on Windows")
}

#[cfg(windows)]
fn windows_auth(username: &str, password: &str) -> anyhow::Result<AuthMethod> {
    Ok(AuthMethod::windows(username, password))
}

#[cfg(not(windows))]
fn windows_auth(_username: &str, _password: &str) -> anyhow::Result<AuthMethod> {
    anyhow::bail!("Windows account authentication is only available on Windows")
}

/// Read a string column, treating NULL as empty.
fn string_at(row: &Row, idx: usize) -> String {
    row.get::<&str, _>(idx).unwrap_or_default().to_string()
}

/// Read an integer column, treating NULL as 0.
fn int_at(row: &Row, idx: usize) -> i32 {
    row.get::<i32, _>(idx).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use crate::sql::plan::{is_no_chain_error, Role};
    use pbsgui_ipc::SqlAuth;

    /// Against a live SQL Server (for example the `mcr.microsoft.com/mssql/server`
    /// container): `PBSGUI_TEST_SQL=host:port` and `PBSGUI_TEST_SQL_SA=<sa password>`.
    /// Walks a database through the states the Always On rules depend on.
    #[tokio::test]
    #[ignore = "needs a live SQL Server"]
    async fn db_state_tracks_the_log_chain() {
        let target = std::env::var("PBSGUI_TEST_SQL").expect("PBSGUI_TEST_SQL");
        let password = std::env::var("PBSGUI_TEST_SQL_SA").expect("PBSGUI_TEST_SQL_SA");
        let (host, port) = target.split_once(':').unwrap();
        let auth = SqlAuth::SqlLogin {
            username: "sa".into(),
        };
        let mut c = super::connect(host, Some(port.parse().unwrap()), &auth, Some(&password))
            .await
            .unwrap();
        for sql in [
            "IF DB_ID('pbsgui_chain') IS NOT NULL DROP DATABASE pbsgui_chain",
            "CREATE DATABASE pbsgui_chain",
            "ALTER DATABASE pbsgui_chain SET RECOVERY FULL",
        ] {
            c.simple_query(sql)
                .await
                .unwrap()
                .into_results()
                .await
                .unwrap();
        }

        let s = super::db_state(&mut c, "pbsgui_chain").await.unwrap();
        assert_eq!(s.role, Role::Standalone);
        assert!(s.preferred && !s.simple_recovery);
        assert!(!s.log_chain, "a new FULL database has no chain");

        // The log backup pbsgui would have issued fails with 4214, and the
        // client's error text is recognised.
        let err = match c
            .simple_query("BACKUP LOG pbsgui_chain TO DISK = 'NUL'")
            .await
        {
            Ok(stream) => stream.into_results().await.err(),
            Err(e) => Some(e),
        }
        .expect("BACKUP LOG must fail without a full");
        assert!(is_no_chain_error(&format!("{err:#}")), "{err:#}");

        // A copy-only full does not start the chain; a normal full does.
        c.simple_query("BACKUP DATABASE pbsgui_chain TO DISK = 'NUL' WITH COPY_ONLY")
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();
        assert!(
            !super::db_state(&mut c, "pbsgui_chain")
                .await
                .unwrap()
                .log_chain
        );
        c.simple_query("BACKUP DATABASE pbsgui_chain TO DISK = 'NUL'")
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();
        assert!(
            super::db_state(&mut c, "pbsgui_chain")
                .await
                .unwrap()
                .log_chain
        );

        // A read-only database is flagged (it never shows a log chain).
        c.simple_query("ALTER DATABASE pbsgui_chain SET READ_ONLY")
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();
        let s = super::db_state(&mut c, "pbsgui_chain").await.unwrap();
        assert!(s.read_only);
        c.simple_query("ALTER DATABASE pbsgui_chain SET READ_WRITE")
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();

        // A missing database is an error (reported per database), not a panic.
        assert!(super::db_state(&mut c, "pbsgui_no_such_db").await.is_err());
        c.simple_query("DROP DATABASE pbsgui_chain")
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();
    }
}
