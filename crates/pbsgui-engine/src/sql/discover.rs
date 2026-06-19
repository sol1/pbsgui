//! Discovery of SQL Server instances.
//!
//! Local enumeration reads the registry on the host the engine runs on. It is
//! the reliable backbone: it needs no credentials and finds every installed
//! Database Engine instance with its port and accepted login modes. Network
//! discovery (Browser/SSRP, host/subnet scanning, Active Directory) and the
//! per-instance probe that fills [`pbsgui_ipc::SqlProbe`] (version, topology,
//! databases) are layered on in later steps.

use pbsgui_ipc::SqlInstance;

/// Discover SQL Server instances. Local enumeration always runs; network
/// discovery is gated on `include_network`.
pub async fn discover(include_network: bool, targets: Vec<String>) -> Vec<SqlInstance> {
    let mut instances = local_instances();

    if include_network {
        // TODO(network): SQL Browser (UDP 1434), scan `targets`, AD SPN lookup,
        // and the AG-listener walk. Tracked as a follow-up step.
        tracing::info!(
            target_count = targets.len(),
            "network SQL discovery requested but not yet implemented"
        );
    }

    instances.sort_by(|a, b| a.server.cmp(&b.server));
    instances.dedup_by(|a, b| a.server.eq_ignore_ascii_case(&b.server));
    instances
}

/// Enumerate Database Engine instances from the local registry.
#[cfg(windows)]
fn local_instances() -> Vec<SqlInstance> {
    use pbsgui_ipc::{SqlAuthMode, SqlDiscoverySource};
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    const SQL_ROOT: &str = r"SOFTWARE\Microsoft\Microsoft SQL Server";

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let names_key = match hklm.open_subkey(format!(r"{SQL_ROOT}\Instance Names\SQL")) {
        Ok(key) => key,
        // No Database Engine installed on this host.
        Err(_) => return Vec::new(),
    };

    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".to_string());

    // Value name = instance name (e.g. "MSSQLSERVER", "SQLEXPRESS"); value data =
    // the internal instance id (e.g. "MSSQL16.SQLEXPRESS") keying the config hive.
    let instance_names: Vec<String> = names_key
        .enum_values()
        .filter_map(|entry| entry.ok().map(|(name, _)| name))
        .collect();

    let mut out = Vec::new();
    for instance_name in instance_names {
        let Ok(instance_id) = names_key.get_value::<String, _>(&instance_name) else {
            continue;
        };
        let inst_root = format!(r"{SQL_ROOT}\{instance_id}");
        let mssql = hklm.open_subkey(format!(r"{inst_root}\MSSQLServer")).ok();

        let auth_mode = mssql
            .as_ref()
            .and_then(|key| key.get_value::<u32, _>("LoginMode").ok())
            .map(|mode| match mode {
                1 => SqlAuthMode::WindowsOnly,
                2 => SqlAuthMode::Mixed,
                _ => SqlAuthMode::Unknown,
            })
            .unwrap_or(SqlAuthMode::Unknown);

        let tcp = mssql
            .as_ref()
            .and_then(|key| key.open_subkey(r"SuperSocketNetLib\Tcp").ok());
        let tcp_enabled = tcp
            .as_ref()
            .and_then(|key| key.get_value::<u32, _>("Enabled").ok())
            .map(|enabled| enabled == 1);
        let port = tcp
            .as_ref()
            .and_then(|key| key.open_subkey("IPAll").ok())
            .and_then(|ipall| tcp_port(&ipall));

        // The presence of a Cluster key marks a Failover Cluster Instance; the
        // probe step later refines this to FCI vs AG.
        let clustered = Some(hklm.open_subkey(format!(r"{inst_root}\Cluster")).is_ok());

        let server = if instance_name.eq_ignore_ascii_case("MSSQLSERVER") {
            host.clone()
        } else {
            format!(r"{host}\{instance_name}")
        };

        out.push(SqlInstance {
            server,
            instance_name,
            host: host.clone(),
            port,
            source: SqlDiscoverySource::LocalRegistry,
            running: None,
            service_account: None,
            auth_mode,
            clustered,
            tcp_enabled,
            probe: None,
            probe_error: None,
        });
    }
    out
}

/// Read the configured TCP port from an instance's `Tcp\IPAll` key, preferring a
/// static `TcpPort` and falling back to the first `TcpDynamicPorts` entry.
#[cfg(windows)]
fn tcp_port(tcp: &winreg::RegKey) -> Option<u16> {
    for value in ["TcpPort", "TcpDynamicPorts"] {
        if let Ok(raw) = tcp.get_value::<String, _>(value) {
            if let Some(port) = raw
                .split(',')
                .next()
                .and_then(|first| first.trim().parse::<u16>().ok())
            {
                return Some(port);
            }
        }
    }
    None
}

/// On non-Windows hosts there is no local SQL Server registry to read.
#[cfg(not(windows))]
fn local_instances() -> Vec<SqlInstance> {
    Vec::new()
}
