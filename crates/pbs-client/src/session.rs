//! PBS backup and reader session over HTTP/2.
//!
//! The data path is a single HTTP/2 connection obtained by an HTTP/1.1 -> HTTP/2
//! upgrade: one `GET /api2/json/backup` (or `/api2/json/reader`) returns
//! `101 Switching Protocols`, after which the same TLS socket carries a raw
//! HTTP/2 connection. Per-chunk, per-index, and per-blob operations are short
//! HTTP/2 requests on that connection.
//!
//! TLS uses SHA-256 certificate fingerprint pinning (the model PBS uses for
//! self-signed servers): the leaf certificate's SHA-256 must equal the pinned
//! fingerprint; normal CA validation is bypassed.
//!
//! This module implements the unencrypted, uncompressed fixed-index image path
//! (backup and restore). Dedup against a previous snapshot, dynamic indexes, and
//! client side encryption are future work.

use std::sync::Arc;

use bytes::Bytes;
use h2::client::SendRequest;
use http::{Method, Request, StatusCode};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::blob;
use crate::error::{PbsError, Result};
use crate::index::{self, FixedIndex, FixedIndexBuilder, DEFAULT_CHUNK_SIZE, DIGEST_LEN};
use crate::manifest::{self, BackupManifest, FileEntry};
use crate::repository::Repository;

/// Upgrade protocol id for the backup (writer) session.
pub const BACKUP_PROTOCOL_ID_V1: &str = "proxmox-backup-protocol-v1";
/// Upgrade protocol id for the reader session.
pub const READER_PROTOCOL_ID_V1: &str = "proxmox-backup-reader-protocol-v1";

/// HTTP/2 max frame size used by the official client (4 MiB).
const H2_MAX_FRAME_SIZE: u32 = 4 * 1024 * 1024;
/// HTTP/2 initial window size used by the official client.
const H2_WINDOW_SIZE: u32 = (1 << 31) - 2;
/// Append at most this many index entries per PUT.
const APPEND_BATCH: usize = 64;

/// Percent-encoding set for query parameter values.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn enc(value: &str) -> String {
    utf8_percent_encode(value, QUERY).to_string()
}

fn h2err(e: h2::Error) -> PbsError {
    PbsError::Protocol(format!("http/2 error: {e}"))
}

/// Parameters identifying a backup or reader session.
#[derive(Debug, Clone)]
pub struct SessionParams {
    pub host: String,
    pub port: u16,
    pub datastore: String,
    /// Auth id, e.g. `user@realm!tokenid`.
    pub auth_id: String,
    /// API token secret.
    pub secret: String,
    /// Expected server certificate SHA-256 fingerprint (colon separated hex, any case).
    pub fingerprint: String,
    /// Backup type: "host", "vm", or "ct".
    pub backup_type: String,
    pub backup_id: String,
    /// Backup time, unix seconds.
    pub backup_time: i64,
    /// Optional namespace (path-like, e.g. "team/proj").
    pub namespace: Option<String>,
}

impl SessionParams {
    /// Build session parameters from a parsed repository plus the token secret,
    /// pinned fingerprint, and snapshot identity.
    pub fn from_repository(
        repo: &Repository,
        secret: impl Into<String>,
        fingerprint: impl Into<String>,
        backup_type: impl Into<String>,
        backup_id: impl Into<String>,
        backup_time: i64,
    ) -> Result<Self> {
        let host = repo
            .host
            .clone()
            .ok_or_else(|| PbsError::Protocol("repository has no host".into()))?;
        let auth_id = repo
            .auth_id
            .clone()
            .ok_or_else(|| PbsError::Auth("repository has no auth id".into()))?;
        Ok(Self {
            host,
            port: repo.port(),
            datastore: repo.datastore.clone(),
            auth_id,
            secret: secret.into(),
            fingerprint: fingerprint.into(),
            backup_type: backup_type.into(),
            backup_id: backup_id.into(),
            backup_time,
            namespace: None,
        })
    }

    fn snapshot_query(&self) -> String {
        let mut q = format!(
            "store={}&backup-type={}&backup-id={}&backup-time={}",
            enc(&self.datastore),
            enc(&self.backup_type),
            enc(&self.backup_id),
            self.backup_time
        );
        if let Some(ns) = &self.namespace {
            if !ns.is_empty() {
                q.push_str(&format!("&ns={}", enc(ns)));
            }
        }
        q
    }
}

/// A rustls verifier that pins the server's leaf certificate by SHA-256.
#[derive(Debug)]
struct PinnedVerifier {
    expected: [u8; 32],
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let actual: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "certificate fingerprint mismatch: expected {}, got {}",
                hex::encode(self.expected),
                hex::encode(actual)
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

fn parse_fingerprint(fp: &str) -> Result<[u8; 32]> {
    let cleaned: String = fp
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect();
    let bytes = hex::decode(&cleaned)
        .map_err(|e| PbsError::Protocol(format!("invalid fingerprint hex: {e}")))?;
    let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        PbsError::Protocol(format!("fingerprint must be 32 bytes, got {}", bytes.len()))
    })?;
    Ok(arr)
}

fn tls_connector(fingerprint: &str) -> Result<TlsConnector> {
    let provider = rustls::crypto::ring::default_provider();
    let algs = provider.signature_verification_algorithms;
    let verifier = PinnedVerifier {
        expected: parse_fingerprint(fingerprint)?,
        algs,
    };
    let config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| PbsError::Protocol(format!("tls config: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Read from the stream until the end of the HTTP header block (CRLF CRLF).
async fn read_headers<S>(stream: &mut S) -> Result<String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err(PbsError::Protocol(
                "connection closed during upgrade".into(),
            ));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16 * 1024 {
            return Err(PbsError::Protocol(
                "upgrade response headers too large".into(),
            ));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// An established HTTP/2 connection over the upgraded socket.
struct H2Conn {
    send: SendRequest<Bytes>,
    authority: String,
}

impl H2Conn {
    /// Perform the upgrade against `path` and bring up HTTP/2.
    async fn upgrade(params: &SessionParams, path: &str, protocol_id: &str) -> Result<Self> {
        let connector = tls_connector(&params.fingerprint)?;
        let tcp = TcpStream::connect((params.host.as_str(), params.port)).await?;
        let server_name = ServerName::try_from(params.host.clone())
            .map_err(|_| PbsError::Protocol(format!("invalid server name: {}", params.host)))?;
        let mut tls = connector.connect(server_name, tcp).await?;

        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}:{port}\r\n\
             Authorization: PBSAPIToken={auth}:{secret}\r\n\
             Upgrade: {protocol_id}\r\n\
             Connection: upgrade\r\n\
             \r\n",
            host = params.host,
            port = params.port,
            auth = params.auth_id,
            secret = params.secret,
        );
        tls.write_all(request.as_bytes()).await?;
        tls.flush().await?;

        let headers = read_headers(&mut tls).await?;
        let status_line = headers.lines().next().unwrap_or_default();
        if !status_line.contains("101") {
            return Err(PbsError::Protocol(format!(
                "expected 101 Switching Protocols, got: {status_line}"
            )));
        }

        let mut builder = h2::client::Builder::new();
        builder
            .max_frame_size(H2_MAX_FRAME_SIZE)
            .initial_window_size(H2_WINDOW_SIZE)
            .initial_connection_window_size(H2_WINDOW_SIZE);
        let (send, connection) = builder.handshake::<_, Bytes>(tls).await.map_err(h2err)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        Ok(Self {
            send,
            authority: format!("{}:{}", params.host, params.port),
        })
    }

    /// Issue one request and read the full response body.
    async fn request(
        &mut self,
        method: Method,
        path_and_query: &str,
        content_type: Option<&str>,
        body: Option<Bytes>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let uri = format!("https://{}{}", self.authority, path_and_query);
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(ct) = content_type {
            builder = builder.header(http::header::CONTENT_TYPE, ct);
        }
        let request = builder
            .body(())
            .map_err(|e| PbsError::Protocol(format!("building request: {e}")))?;

        let end_of_stream = body.is_none();
        let mut send = self.send.clone().ready().await.map_err(h2err)?;
        let (response_fut, mut stream) =
            send.send_request(request, end_of_stream).map_err(h2err)?;
        if let Some(bytes) = body {
            stream.send_data(bytes, true).map_err(h2err)?;
        }

        let response = response_fut.await.map_err(h2err)?;
        let status = response.status();
        let mut body_stream = response.into_body();
        let mut out = Vec::new();
        while let Some(chunk) = body_stream.data().await {
            let chunk = chunk.map_err(h2err)?;
            out.extend_from_slice(&chunk);
            let _ = body_stream.flow_control().release_capacity(chunk.len());
        }
        Ok((status, out))
    }
}

fn ensure_ok(status: StatusCode, body: &[u8]) -> Result<()> {
    if status.is_success() {
        Ok(())
    } else {
        Err(PbsError::Protocol(format!(
            "server returned {}: {}",
            status,
            String::from_utf8_lossy(body)
        )))
    }
}

fn unwrap_data(body: &[u8]) -> Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| PbsError::Protocol(format!("invalid JSON response: {e}")))?;
    Ok(value
        .get("data")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

fn random_uuid() -> [u8; 16] {
    *uuid::Uuid::new_v4().as_bytes()
}

/// A writer session for creating a backup snapshot.
pub struct BackupWriter {
    conn: H2Conn,
}

impl BackupWriter {
    /// Open a backup session (HTTP/2 upgrade against `/api2/json/backup`).
    pub async fn connect(params: &SessionParams) -> Result<Self> {
        let path = format!("/api2/json/backup?{}", params.snapshot_query());
        let conn = H2Conn::upgrade(params, &path, BACKUP_PROTOCOL_ID_V1).await?;
        Ok(Self { conn })
    }

    /// Create a fixed-index writer; returns the writer id (wid).
    pub async fn create_fixed_index(&mut self, archive_name: &str, size: u64) -> Result<u64> {
        let q = format!(
            "/fixed_index?archive-name={}&size={}",
            enc(archive_name),
            size
        );
        let (status, body) = self.conn.request(Method::POST, &q, None, None).await?;
        ensure_ok(status, &body)?;
        unwrap_data(&body)?
            .as_u64()
            .ok_or_else(|| PbsError::Protocol("fixed_index did not return a wid".into()))
    }

    /// Upload one chunk (its DataBlob bytes) for the given writer.
    pub async fn upload_chunk(
        &mut self,
        wid: u64,
        digest: &[u8; DIGEST_LEN],
        plaintext_len: u64,
        encoded: &[u8],
    ) -> Result<()> {
        let q = format!(
            "/fixed_chunk?wid={}&digest={}&size={}&encoded-size={}",
            wid,
            hex::encode(digest),
            plaintext_len,
            encoded.len()
        );
        let (status, body) = self
            .conn
            .request(
                Method::POST,
                &q,
                Some("application/octet-stream"),
                Some(Bytes::copy_from_slice(encoded)),
            )
            .await?;
        ensure_ok(status, &body)
    }

    /// Append a batch of chunk references to the index.
    pub async fn append_fixed_index(
        &mut self,
        wid: u64,
        digests: &[[u8; DIGEST_LEN]],
        offsets: &[u64],
    ) -> Result<()> {
        let body = serde_json::json!({
            "wid": wid,
            "digest-list": digests.iter().map(hex::encode).collect::<Vec<_>>(),
            "offset-list": offsets,
        });
        let bytes = serde_json::to_vec(&body)
            .map_err(|e| PbsError::Protocol(format!("encoding append body: {e}")))?;
        let (status, rbody) = self
            .conn
            .request(
                Method::PUT,
                "/fixed_index",
                Some("application/json"),
                Some(Bytes::from(bytes)),
            )
            .await?;
        ensure_ok(status, &rbody)
    }

    /// Close (commit) the fixed index.
    pub async fn close_fixed_index(
        &mut self,
        wid: u64,
        chunk_count: u64,
        size: u64,
        csum: &[u8; DIGEST_LEN],
    ) -> Result<()> {
        let q = format!(
            "/fixed_close?wid={}&chunk-count={}&size={}&csum={}",
            wid,
            chunk_count,
            size,
            hex::encode(csum)
        );
        let (status, body) = self.conn.request(Method::POST, &q, None, None).await?;
        ensure_ok(status, &body)
    }

    /// Upload a complete DataBlob as a named file (e.g. the manifest).
    pub async fn upload_blob(&mut self, file_name: &str, blob: &[u8]) -> Result<()> {
        let q = format!(
            "/blob?file-name={}&encoded-size={}",
            enc(file_name),
            blob.len()
        );
        let (status, body) = self
            .conn
            .request(
                Method::POST,
                &q,
                Some("application/octet-stream"),
                Some(Bytes::copy_from_slice(blob)),
            )
            .await?;
        ensure_ok(status, &body)
    }

    /// Commit the snapshot and end the session.
    pub async fn finish(&mut self) -> Result<()> {
        let (status, body) = self
            .conn
            .request(Method::POST, "/finish", None, None)
            .await?;
        ensure_ok(status, &body)
    }
}

/// Back up an in-memory image as a single fixed-index archive, then finish the
/// snapshot. This is the simplest end-to-end backup (no dedup against a previous
/// snapshot yet).
pub async fn backup_fixed_image(
    params: &SessionParams,
    archive_name: &str,
    image: &[u8],
) -> Result<[u8; DIGEST_LEN]> {
    let size = image.len() as u64;
    let chunk_size = DEFAULT_CHUNK_SIZE;

    let mut writer = BackupWriter::connect(params).await?;
    let wid = writer.create_fixed_index(archive_name, size).await?;

    let mut builder = FixedIndexBuilder::new(size, chunk_size, params.backup_time, random_uuid());
    let mut batch_digests: Vec<[u8; DIGEST_LEN]> = Vec::new();
    let mut batch_offsets: Vec<u64> = Vec::new();
    let mut offset = 0u64;

    for chunk in image.chunks(chunk_size as usize) {
        let digest = index::chunk_digest(chunk);
        let encoded = blob::encode_uncompressed(chunk);
        writer
            .upload_chunk(wid, &digest, chunk.len() as u64, &encoded)
            .await?;
        builder.push_digest(digest);
        batch_digests.push(digest);
        batch_offsets.push(offset);
        offset += chunk.len() as u64;

        if batch_digests.len() >= APPEND_BATCH {
            writer
                .append_fixed_index(wid, &batch_digests, &batch_offsets)
                .await?;
            batch_digests.clear();
            batch_offsets.clear();
        }
    }
    if !batch_digests.is_empty() {
        writer
            .append_fixed_index(wid, &batch_digests, &batch_offsets)
            .await?;
    }

    let csum = builder.index_csum();
    writer
        .close_fixed_index(wid, builder.chunk_count() as u64, size, &csum)
        .await?;

    let entry = FileEntry::fixed_image(archive_name, size, &csum);
    let backup_manifest = BackupManifest::new(
        &params.backup_type,
        &params.backup_id,
        params.backup_time,
        vec![entry],
    );
    let manifest_json = backup_manifest
        .to_json_bytes()
        .map_err(|e| PbsError::Protocol(format!("encoding manifest: {e}")))?;
    let manifest_blob = blob::encode_uncompressed(&manifest_json);
    writer
        .upload_blob(manifest::MANIFEST_BLOB_NAME, &manifest_blob)
        .await?;

    writer.finish().await?;
    Ok(csum)
}

/// A reader session for restoring from a snapshot.
pub struct ReaderClient {
    conn: H2Conn,
}

impl ReaderClient {
    /// Open a reader session (HTTP/2 upgrade against `/api2/json/reader`).
    pub async fn connect(params: &SessionParams) -> Result<Self> {
        let path = format!("/api2/json/reader?{}", params.snapshot_query());
        let conn = H2Conn::upgrade(params, &path, READER_PROTOCOL_ID_V1).await?;
        Ok(Self { conn })
    }

    /// Download a named file (index or blob) as raw bytes.
    pub async fn download(&mut self, file_name: &str) -> Result<Vec<u8>> {
        let q = format!("/download?file-name={}", enc(file_name));
        let (status, body) = self.conn.request(Method::GET, &q, None, None).await?;
        ensure_ok(status, &body)?;
        Ok(body)
    }

    /// Download one chunk's DataBlob by digest.
    pub async fn download_chunk(&mut self, digest: &[u8; DIGEST_LEN]) -> Result<Vec<u8>> {
        let q = format!("/chunk?digest={}", hex::encode(digest));
        let (status, body) = self.conn.request(Method::GET, &q, None, None).await?;
        ensure_ok(status, &body)?;
        Ok(body)
    }

    /// Download a fixed-index archive and reassemble the original image bytes.
    pub async fn restore_fixed_image(&mut self, archive_name: &str) -> Result<Vec<u8>> {
        let index_bytes = self.download(archive_name).await?;
        let fixed = FixedIndex::parse(&index_bytes)?;
        if !fixed.verify_csum() {
            return Err(PbsError::Protocol(
                "downloaded fixed index failed its csum check".into(),
            ));
        }
        let mut image = Vec::with_capacity(fixed.size as usize);
        for digest in &fixed.digests {
            let chunk_blob = self.download_chunk(digest).await?;
            let chunk = blob::decode(&chunk_blob)?;
            image.extend_from_slice(&chunk);
        }
        image.truncate(fixed.size as usize);
        Ok(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fingerprint_with_and_without_colons() {
        let hexstr = "ab".repeat(32);
        let colon = hexstr
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(parse_fingerprint(&hexstr).unwrap(), [0xab; 32]);
        assert_eq!(parse_fingerprint(&colon).unwrap(), [0xab; 32]);
        assert_eq!(
            parse_fingerprint(&colon.to_uppercase()).unwrap(),
            [0xab; 32]
        );
    }

    #[test]
    fn rejects_wrong_length_fingerprint() {
        assert!(parse_fingerprint("abcd").is_err());
    }

    #[test]
    fn builds_snapshot_query() {
        let repo: Repository = "tok@pbs!t@pbs.example.com:8007:store".parse().unwrap();
        let mut p =
            SessionParams::from_repository(&repo, "secret", "ab".repeat(32), "host", "myhost", 42)
                .unwrap();
        assert_eq!(
            p.snapshot_query(),
            "store=store&backup-type=host&backup-id=myhost&backup-time=42"
        );
        p.namespace = Some("team/proj".to_string());
        assert!(p.snapshot_query().contains("&ns=team%2Fproj"));
    }
}
