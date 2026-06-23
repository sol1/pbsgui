//! Minimal Proxmox Backup Server management REST API client.
//!
//! Used for browsing: listing the snapshots in a backup group. It reuses the
//! TLS connector with SHA-256 fingerprint pinning, issues a plain `HTTP/1.1` GET
//! with `Connection: close`, reads to end of stream, and parses the JSON body
//! (which PBS wraps as `{ "data": ... }`).

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use rustls::pki_types::ServerName;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::{PbsError, Result};
use crate::repository::Repository;
use crate::session;

const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn enc(value: &str) -> String {
    utf8_percent_encode(value, QUERY).to_string()
}

/// One archive listed within a snapshot (e.g. `files.didx`).
#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotFile {
    pub filename: String,
    #[serde(default)]
    pub size: Option<u64>,
}

/// A snapshot in a backup group.
#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    #[serde(rename = "backup-time")]
    pub backup_time: i64,
    #[serde(rename = "backup-id")]
    pub backup_id: String,
    #[serde(rename = "backup-type")]
    pub backup_type: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub files: Vec<SnapshotFile>,
    #[serde(default)]
    pub owner: Option<String>,
}

/// A client for the PBS management REST API.
pub struct ApiClient {
    pub host: String,
    pub port: u16,
    pub auth_id: String,
    pub secret: String,
    pub fingerprint: String,
}

impl ApiClient {
    /// Build from a parsed repository plus the token secret and pinned fingerprint.
    pub fn from_repository(
        repo: &Repository,
        secret: impl Into<String>,
        fingerprint: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            host: repo
                .host
                .clone()
                .ok_or_else(|| PbsError::Protocol("repository has no host".into()))?,
            port: repo.port(),
            // Trim the auth id and secret: a token secret pasted from the PBS UI
            // often carries a trailing newline or stray space, which would
            // otherwise be sent verbatim and rejected as a bad credential.
            auth_id: repo
                .auth_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| PbsError::Auth("repository has no auth id".into()))?
                .to_string(),
            secret: secret.into().trim().to_string(),
            fingerprint: fingerprint.into(),
        })
    }

    /// List the snapshots in a backup group, newest data as returned by PBS.
    /// `namespace` selects a namespace within the datastore (`None` = the root).
    pub async fn list_snapshots(
        &self,
        datastore: &str,
        namespace: Option<&str>,
        backup_type: &str,
        backup_id: &str,
    ) -> Result<Vec<Snapshot>> {
        let mut path = format!(
            "/api2/json/admin/datastore/{}/snapshots?backup-type={}&backup-id={}",
            enc(datastore),
            enc(backup_type),
            enc(backup_id)
        );
        if let Some(ns) = namespace {
            if !ns.is_empty() {
                path.push_str(&format!("&ns={}", enc(ns)));
            }
        }
        let data = self.get_data(&path).await?;
        serde_json::from_value(data)
            .map_err(|e| PbsError::Protocol(format!("parsing snapshots: {e}")))
    }

    /// Whether the token holds `Datastore.Backup` on a datastore (and namespace),
    /// read from its own effective permissions. This needs no `Datastore.Audit`
    /// (a token may always read its own permissions), so it validates a
    /// backup-only token without a false negative. A failure to reach PBS, a TLS
    /// fingerprint mismatch, or a rejected token surfaces as the returned error.
    pub async fn can_backup(&self, datastore: &str, namespace: Option<&str>) -> Result<bool> {
        let acl_path = match namespace {
            Some(ns) if !ns.is_empty() => format!("/datastore/{datastore}/{ns}"),
            _ => format!("/datastore/{datastore}"),
        };
        let path = format!("/api2/json/access/permissions?path={}", enc(&acl_path));
        let data = self.get_data(&path).await?;
        Ok(privilege_granted(&data, &acl_path, "Datastore.Backup"))
    }

    async fn get_data(&self, path: &str) -> Result<serde_json::Value> {
        let body = self.get(path).await?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| PbsError::Protocol(format!("invalid JSON response: {e}")))?;
        Ok(value
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    async fn get(&self, path_and_query: &str) -> Result<Vec<u8>> {
        let connector = session::tls_connector(&self.fingerprint)?;
        let tcp = TcpStream::connect((self.host.as_str(), self.port)).await?;
        let server_name = ServerName::try_from(self.host.clone())
            .map_err(|_| PbsError::Protocol(format!("invalid server name: {}", self.host)))?;
        let mut tls = connector.connect(server_name, tcp).await?;

        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}:{port}\r\n\
             Authorization: PBSAPIToken={auth}:{secret}\r\n\
             Accept: application/json\r\n\
             Connection: close\r\n\
             \r\n",
            path = path_and_query,
            host = self.host,
            port = self.port,
            auth = self.auth_id,
            secret = self.secret,
        );
        tls.write_all(request.as_bytes()).await?;
        tls.flush().await?;

        // Connection: close -> read until end of stream (tolerate an unclean TLS close).
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match tls.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
        }
        parse_http_response(&raw)
    }
}

/// Look up one privilege in an `/access/permissions` response. PBS returns either
/// a path-scoped map (`{ "Datastore.Backup": true, ... }`, when queried with a
/// `path`) or the full tree (`{ "/datastore/x": { ... } }`); handle both, and
/// accept the privilege as a bool or a non-zero number.
fn privilege_granted(data: &serde_json::Value, acl_path: &str, name: &str) -> bool {
    let truthy =
        |v: &serde_json::Value| v.as_bool() == Some(true) || v.as_u64().is_some_and(|n| n != 0);
    if let Some(v) = data.get(name) {
        return truthy(v);
    }
    data.get(acl_path)
        .and_then(|m| m.get(name))
        .is_some_and(truthy)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parse an HTTP/1.1 response: split headers from body, check the status, and
/// de-chunk the body if it is `Transfer-Encoding: chunked`.
fn parse_http_response(raw: &[u8]) -> Result<Vec<u8>> {
    let sep = find_subsequence(raw, b"\r\n\r\n")
        .ok_or_else(|| PbsError::Protocol("malformed HTTP response (no header end)".into()))?;
    let header = String::from_utf8_lossy(&raw[..sep]);
    let body = &raw[sep + 4..];

    let status_line = header.lines().next().unwrap_or_default();
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    let chunked = header
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");

    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };

    if !(200..300).contains(&code) {
        return Err(PbsError::Protocol(format!(
            "server returned {}: {}",
            status_line.trim(),
            String::from_utf8_lossy(&body)
        )));
    }
    Ok(body)
}

/// Decode an HTTP chunked-transfer body.
fn dechunk(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = find_subsequence(body, b"\r\n")
            .ok_or_else(|| PbsError::Protocol("malformed chunked body".into()))?;
        let size_str = String::from_utf8_lossy(&body[..line_end]);
        let size =
            usize::from_str_radix(size_str.trim().split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| PbsError::Protocol("invalid chunk size".into()))?;
        body = &body[line_end + 2..];
        if size == 0 {
            break;
        }
        if body.len() < size {
            return Err(PbsError::Protocol("truncated chunk".into()));
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..]; // skip chunk data + trailing CRLF
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"data\":[1,2]}";
        let body = parse_http_response(raw).unwrap();
        assert_eq!(body, b"{\"data\":[1,2]}");
    }

    #[test]
    fn parses_chunked_response() {
        // Two chunks: "{\"dat" (5) + "a\":[1]}" (7) = {"data":[1]}.
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"dat\r\n7\r\na\":[1]}\r\n0\r\n\r\n";
        let body = parse_http_response(raw).unwrap();
        assert_eq!(body, b"{\"data\":[1]}");
    }

    #[test]
    fn errors_on_non_2xx() {
        let raw = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\n\r\nno access";
        assert!(parse_http_response(raw).is_err());
    }

    #[test]
    fn reads_privilege_from_either_shape() {
        let path = "/datastore/store";
        // Path-scoped map (queried with ?path=...).
        let scoped = serde_json::json!({"Datastore.Backup": true, "Datastore.Read": false});
        assert!(privilege_granted(&scoped, path, "Datastore.Backup"));
        assert!(!privilege_granted(&scoped, path, "Datastore.Read"));
        // Full tree, privilege as a number.
        let tree = serde_json::json!({"/datastore/store": {"Datastore.Backup": 1}});
        assert!(privilege_granted(&tree, path, "Datastore.Backup"));
        // Absent.
        assert!(!privilege_granted(
            &serde_json::json!({}),
            path,
            "Datastore.Backup"
        ));
    }
}
