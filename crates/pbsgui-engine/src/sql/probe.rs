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

type SqlClient = Client<Compat<TcpStream>>;

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
       CAST(sys.fn_hadr_backup_is_preferred_replica(d.name) AS int) \
FROM sys.databases d \
LEFT JOIN sys.dm_hadr_database_replica_states drs \
       ON drs.database_id = d.database_id AND drs.is_local = 1 \
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

async fn connect(
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
