//! SQL Server Resolution Protocol (SSRP) client.
//!
//! The SQL Server Browser listens on UDP 1434 and answers a single `0x02` byte
//! with the host's Database Engine instances and their TCP ports. We use it two
//! ways: a broadcast to find instances on the local subnet, and unicast queries
//! to specific hosts or an expanded subnet (the `targets` field of a discovery
//! request). No credentials are needed; an instance that answers is reachable.

use std::net::Ipv4Addr;
use std::time::Duration;

use pbsgui_ipc::{SqlAuthMode, SqlDiscoverySource, SqlInstance};
use tokio::net::UdpSocket;
use tokio::time::{timeout_at, Instant};

const BROWSER_PORT: u16 = 1434;
const REQUEST: [u8; 1] = [0x02];

/// Broadcast an SSRP request and collect the instances that answer.
pub async fn broadcast(timeout: Duration) -> Vec<SqlInstance> {
    let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)).await else {
        return Vec::new();
    };
    if sock.set_broadcast(true).is_err() {
        return Vec::new();
    }
    if sock
        .send_to(&REQUEST, (Ipv4Addr::BROADCAST, BROWSER_PORT))
        .await
        .is_err()
    {
        return Vec::new();
    }
    collect(&sock, timeout, SqlDiscoverySource::Browser).await
}

/// Unicast an SSRP request to each host (resolved names or expanded subnet IPs)
/// on one socket, then collect every answer within `timeout`.
pub async fn scan(hosts: &[String], timeout: Duration) -> Vec<SqlInstance> {
    if hosts.is_empty() {
        return Vec::new();
    }
    let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)).await else {
        return Vec::new();
    };
    for host in hosts {
        let _ = sock.send_to(&REQUEST, (host.as_str(), BROWSER_PORT)).await;
    }
    collect(&sock, timeout, SqlDiscoverySource::NetworkScan).await
}

/// Expand a discovery target into hosts to query: a single host/IP as-is, or an
/// IPv4 CIDR (`a.b.c.d/prefix`) into its host addresses. Subnets larger than /22
/// (1024 hosts) are rejected to avoid an enormous scan.
pub fn expand_target(target: &str) -> Vec<String> {
    let target = target.trim();
    let Some((net, prefix)) = target.split_once('/') else {
        return if target.is_empty() {
            Vec::new()
        } else {
            vec![target.to_string()]
        };
    };
    let (Ok(ip), Ok(prefix)) = (net.trim().parse::<Ipv4Addr>(), prefix.trim().parse::<u32>())
    else {
        return Vec::new();
    };
    if !(22..=32).contains(&prefix) {
        return Vec::new();
    }
    let bits = 32 - prefix;
    let base = u32::from(ip) & (u32::MAX.checked_shl(bits).unwrap_or(0));
    let count = 1u32 << bits;
    // Skip the network and broadcast addresses for ordinary subnets.
    let (start, end) = if prefix <= 30 {
        (1, count - 1)
    } else {
        (0, count)
    };
    (start..end)
        .map(|i| Ipv4Addr::from(base + i).to_string())
        .collect()
}

/// Receive and parse SSRP answers until `timeout` elapses.
async fn collect(
    sock: &UdpSocket,
    timeout: Duration,
    source: SqlDiscoverySource,
) -> Vec<SqlInstance> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    // Stops on the deadline (timeout_at -> Err) or a socket error.
    while let Ok(Ok((n, _src))) = timeout_at(deadline, sock.recv_from(&mut buf)).await {
        out.extend(parse_response(&buf[..n], source));
    }
    out
}

/// Parse an SSRP response packet into instances. The packet is
/// `0x05 len_lo len_hi <ASCII>`, where the ASCII is a flat `key;value;...` list
/// with `;;` separating instances, e.g.
/// `ServerName;HOST;InstanceName;SQLEXPRESS;IsClustered;No;Version;15.0;tcp;1433;;`.
pub fn parse_response(packet: &[u8], source: SqlDiscoverySource) -> Vec<SqlInstance> {
    if packet.len() < 3 || packet[0] != 0x05 {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&packet[3..]);

    let mut out = Vec::new();
    for chunk in text.split(";;") {
        let fields: Vec<&str> = chunk.split(';').collect();
        let mut server_name = "";
        let mut instance = "";
        let mut port: Option<u16> = None;
        let mut clustered: Option<bool> = None;
        let mut i = 0;
        while i + 1 < fields.len() {
            let (key, value) = (fields[i], fields[i + 1]);
            match key.to_ascii_lowercase().as_str() {
                "servername" => server_name = value,
                "instancename" => instance = value,
                "tcp" => port = value.trim().parse().ok(),
                "isclustered" => clustered = Some(value.eq_ignore_ascii_case("yes")),
                _ => {}
            }
            i += 2;
        }
        if server_name.is_empty() {
            continue;
        }
        let instance_name = if instance.is_empty() {
            "MSSQLSERVER".to_string()
        } else {
            instance.to_string()
        };
        let server = if instance_name.eq_ignore_ascii_case("MSSQLSERVER") {
            server_name.to_string()
        } else {
            format!(r"{server_name}\{instance_name}")
        };
        out.push(SqlInstance {
            server,
            instance_name,
            host: server_name.to_string(),
            port,
            source,
            running: Some(true), // it answered the browser
            service_account: None,
            auth_mode: SqlAuthMode::Unknown,
            clustered,
            // The browser advertises a TCP port only when TCP/IP is enabled.
            tcp_enabled: port.map(|_| true),
            probe: None,
            probe_error: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_instances() {
        let body = "ServerName;STANLEY;InstanceName;MSSQLSERVER;IsClustered;No;Version;16.0.1000.6;tcp;1433;;\
                    ServerName;STANLEY;InstanceName;SQLEXPRESS;IsClustered;Yes;Version;15.0;tcp;49677;;";
        let mut packet = vec![0x05, 0, 0];
        packet.extend_from_slice(body.as_bytes());
        let got = parse_response(&packet, SqlDiscoverySource::Browser);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].server, "STANLEY");
        assert_eq!(got[0].instance_name, "MSSQLSERVER");
        assert_eq!(got[0].port, Some(1433));
        assert_eq!(got[0].clustered, Some(false));
        assert_eq!(got[0].tcp_enabled, Some(true));
        assert_eq!(got[1].server, r"STANLEY\SQLEXPRESS");
        assert_eq!(got[1].port, Some(49677));
        assert_eq!(got[1].clustered, Some(true));
    }

    #[test]
    fn rejects_non_ssrp_packet() {
        assert!(parse_response(&[0x00, 1, 2, 3], SqlDiscoverySource::Browser).is_empty());
        assert!(parse_response(&[], SqlDiscoverySource::Browser).is_empty());
    }

    #[test]
    fn expands_a_host_and_a_small_subnet() {
        assert_eq!(expand_target("stanley"), vec!["stanley"]);
        assert_eq!(expand_target("10.0.0.5"), vec!["10.0.0.5"]);
        let net = expand_target("192.168.1.0/30");
        assert_eq!(net, vec!["192.168.1.1", "192.168.1.2"]); // .0 and .3 skipped
        assert_eq!(expand_target("192.168.1.10/24").len(), 254);
    }

    #[test]
    fn rejects_oversized_or_bad_subnets() {
        assert!(expand_target("10.0.0.0/8").is_empty()); // too large
        assert!(expand_target("not-an-ip/24").is_empty());
        assert!(expand_target("").is_empty());
    }
}
