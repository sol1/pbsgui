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

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
use crate::chunker;
use crate::crypt::CryptConfig;
use crate::error::{PbsError, Result};
use crate::index::{
    self, DynamicIndex, DynamicIndexBuilder, FixedIndex, FixedIndexBuilder, DEFAULT_CHUNK_SIZE,
    DIGEST_LEN,
};
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

/// Explicit override for [`pipeline_width`]; 0 means "use the core-derived
/// default". Set once at startup via [`set_pipeline_width`].
static PIPELINE_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// Fix the backup/restore pipeline width (the number of chunks compressed and
/// uploaded, or downloaded and decompressed, at once). Pass 0 to restore the
/// default. The engine wires this to a setting so a dedicated backup window can
/// go wider; unset, the default is deliberately modest (see [`pipeline_width`]).
pub fn set_pipeline_width(n: usize) {
    PIPELINE_OVERRIDE.store(n, Ordering::Relaxed);
}

/// How many chunks to compress+upload (or download+decompress) concurrently.
///
/// Every chunk is zstd-(de)compressed and AES-(en/de)crypted on a blocking
/// thread, so this bounds CPU use. It defaults to half the machine's cores
/// (clamped to `[2, 16]`) rather than a fixed maximum, so a backup or restore
/// leaves headroom for a live workload instead of saturating the box - the
/// concurrency is the only lever, since the per-chunk crypto/compression work is
/// inherent. [`set_pipeline_width`] overrides it. Also pipelines network I/O:
/// HTTP/2 multiplexes the transfers over the one connection, overlapping the
/// network with the disk/CPU work.
fn pipeline_width() -> usize {
    match PIPELINE_OVERRIDE.load(Ordering::Relaxed) {
        0 => {
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            (cores / 2).clamp(2, 16)
        }
        n => n,
    }
}

/// Cap the up-front buffer a buffered restore reserves from an index's declared
/// size. The size comes from the (possibly corrupt or hostile) index, so reserving
/// it directly would let a bogus index trigger a huge allocation before a single
/// chunk is fetched. The buffer still grows to fit the actual chunk data, which is
/// verified against its digest as it arrives.
const MAX_RESTORE_PREALLOC: usize = 128 * 1024 * 1024;

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
        // Trim the auth id and secret: a token secret pasted from the PBS UI often
        // carries a trailing newline or stray space, which would otherwise be sent
        // verbatim in the Authorization header and rejected as a bad credential.
        let auth_id = repo
            .auth_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| PbsError::Auth("repository has no auth id".into()))?
            .to_string();
        Ok(Self {
            host,
            port: repo.port(),
            datastore: repo.datastore.clone(),
            auth_id,
            secret: secret.into().trim().to_string(),
            fingerprint: fingerprint.into(),
            backup_type: backup_type.into(),
            backup_id: backup_id.into(),
            backup_time,
            namespace: repo.namespace.clone(),
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

/// A TLS connector that accepts exactly the certificate with the given SHA-256
/// fingerprint (colon-separated hex, any case), bypassing CA validation - the
/// PBS trust model. Public so the engine's relay agent pins its proxy the same
/// way this client pins PBS.
pub fn tls_connector(fingerprint: &str) -> Result<TlsConnector> {
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
            // A 403 means PBS authenticated the token but denied it; a 401 means the
            // credentials themselves were not accepted. Spell that out, since the raw
            // status is otherwise easy to misread as a protocol fault.
            let hint = if status_line.contains("403") {
                " (the PBS token lacks permission on this datastore/namespace, \
                 or another owner holds this backup group)"
            } else if status_line.contains("401") {
                " (the PBS token id or secret was not accepted)"
            } else {
                ""
            };
            return Err(PbsError::Protocol(format!(
                "expected 101 Switching Protocols, got: {status_line}{hint}"
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

    /// Issue one request and read the full response body (strict: a connection
    /// close mid-body is an error). Use for requests whose body we consume.
    async fn request(
        &self,
        method: Method,
        path_and_query: &str,
        content_type: Option<&str>,
        body: Option<Bytes>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        h2_request(
            &self.send,
            &self.authority,
            method,
            path_and_query,
            content_type,
            body,
            false,
        )
        .await
    }

    /// Issue a status-only request that commits an operation on the server (chunk
    /// upload, index append/close, blob, `/finish`). Tolerates the server closing
    /// the connection after a success status, since we only read the status.
    async fn request_committing(
        &self,
        method: Method,
        path_and_query: &str,
        content_type: Option<&str>,
        body: Option<Bytes>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        h2_request(
            &self.send,
            &self.authority,
            method,
            path_and_query,
            content_type,
            body,
            true,
        )
        .await
    }
}

/// Issue one HTTP/2 request on a (cloned) send handle and read the full response.
/// Cloning `send` per request is what lets many requests be in flight concurrently
/// over the one connection (h2 multiplexes streams).
///
/// `tolerate_close`: for a status-only request (the server commits the operation
/// and we only read its status, not its body), a connection close while reading
/// the ignored body is treated as a clean end once a success status is in hand.
/// PBS tears down the HTTP/2 connection right after a successful `/finish`, so
/// without this a committed backup would be misreported as failed. It must stay
/// `false` for requests whose body we actually use (GET downloads, the wid from
/// creating an index), where a truncated read is a real error.
async fn h2_request(
    send: &SendRequest<Bytes>,
    authority: &str,
    method: Method,
    path_and_query: &str,
    content_type: Option<&str>,
    body: Option<Bytes>,
    tolerate_close: bool,
) -> Result<(StatusCode, Vec<u8>)> {
    let uri = format!("https://{authority}{path_and_query}");
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(ct) = content_type {
        builder = builder.header(http::header::CONTENT_TYPE, ct);
    }
    let request = builder
        .body(())
        .map_err(|e| PbsError::Protocol(format!("building request: {e}")))?;

    let end_of_stream = body.is_none();
    let mut send = send.clone().ready().await.map_err(h2err)?;
    let (response_fut, mut stream) = send.send_request(request, end_of_stream).map_err(h2err)?;
    if let Some(bytes) = body {
        stream.send_data(bytes, true).map_err(h2err)?;
    }

    let response = response_fut.await.map_err(h2err)?;
    let status = response.status();
    let mut body_stream = response.into_body();
    let mut out = Vec::new();
    while let Some(chunk) = body_stream.data().await {
        match chunk {
            Ok(chunk) => {
                out.extend_from_slice(&chunk);
                let _ = body_stream.flow_control().release_capacity(chunk.len());
            }
            // The operation is already committed once the server returned a success
            // status; a connection close while draining the ignored body is not a
            // failure for a status-only request.
            Err(_) if tolerate_close && status.is_success() => break,
            Err(e) => return Err(h2err(e)),
        }
    }
    Ok((status, out))
}

/// A cloneable handle for uploading chunk data concurrently on a backup
/// connection. Each clone carries its own h2 `SendRequest`, so uploads spawned
/// from it run as separate multiplexed HTTP/2 streams.
#[derive(Clone)]
pub struct ChunkUploader {
    send: SendRequest<Bytes>,
    authority: String,
}

impl ChunkUploader {
    /// Upload one dynamic-index chunk (its encoded DataBlob bytes). Takes owned
    /// data so it can run in a spawned task.
    pub async fn upload_dynamic_chunk(
        &self,
        wid: u64,
        digest: [u8; DIGEST_LEN],
        plaintext_len: u64,
        encoded: Vec<u8>,
    ) -> Result<()> {
        let q = format!(
            "/dynamic_chunk?wid={}&digest={}&size={}&encoded-size={}",
            wid,
            hex::encode(digest),
            plaintext_len,
            encoded.len()
        );
        let (status, body) = h2_request(
            &self.send,
            &self.authority,
            Method::POST,
            &q,
            Some("application/octet-stream"),
            Some(Bytes::from(encoded)),
            true,
        )
        .await?;
        ensure_ok(status, &body)
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

    /// A cloneable handle for uploading chunks concurrently on this connection.
    pub fn chunk_uploader(&self) -> ChunkUploader {
        ChunkUploader {
            send: self.conn.send.clone(),
            authority: self.conn.authority.clone(),
        }
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
            .request_committing(
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
            .request_committing(
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
        let (status, body) = self
            .conn
            .request_committing(Method::POST, &q, None, None)
            .await?;
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
            .request_committing(
                Method::POST,
                &q,
                Some("application/octet-stream"),
                Some(Bytes::copy_from_slice(blob)),
            )
            .await?;
        ensure_ok(status, &body)
    }

    /// Create a dynamic-index writer; returns the writer id (wid).
    pub async fn create_dynamic_index(&mut self, archive_name: &str) -> Result<u64> {
        let q = format!("/dynamic_index?archive-name={}", enc(archive_name));
        let (status, body) = self.conn.request(Method::POST, &q, None, None).await?;
        ensure_ok(status, &body)?;
        unwrap_data(&body)?
            .as_u64()
            .ok_or_else(|| PbsError::Protocol("dynamic_index did not return a wid".into()))
    }

    // Dynamic-index chunk upload now goes through `ChunkUploader` (see
    // `chunk_uploader`), so it can run concurrently.

    /// Append a batch of chunk references to a dynamic index. `offsets` are the
    /// chunk START offsets (the first chunk is 0); the server derives each end
    /// offset from the chunk size, the same convention as the fixed index.
    pub async fn append_dynamic_index(
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
            .request_committing(
                Method::PUT,
                "/dynamic_index",
                Some("application/json"),
                Some(Bytes::from(bytes)),
            )
            .await?;
        ensure_ok(status, &rbody)
    }

    /// Close (commit) a dynamic index.
    pub async fn close_dynamic_index(
        &mut self,
        wid: u64,
        chunk_count: u64,
        size: u64,
        csum: &[u8; DIGEST_LEN],
    ) -> Result<()> {
        let q = format!(
            "/dynamic_close?wid={}&chunk-count={}&size={}&csum={}",
            wid,
            chunk_count,
            size,
            hex::encode(csum)
        );
        let (status, body) = self
            .conn
            .request_committing(Method::POST, &q, None, None)
            .await?;
        ensure_ok(status, &body)
    }

    /// Download the previous snapshot's index for an archive, if any. Used to
    /// build the known-chunk set so unchanged chunks are not re-uploaded.
    pub async fn download_previous(&mut self, archive_name: &str) -> Result<Option<Vec<u8>>> {
        let q = format!("/previous?archive-name={}", enc(archive_name));
        let (status, body) = self.conn.request(Method::GET, &q, None, None).await?;
        if status.is_success() {
            Ok(Some(body))
        } else {
            Ok(None)
        }
    }

    /// Commit the snapshot and end the session. PBS tears down the HTTP/2
    /// connection immediately after committing, so this is a status-only request
    /// that tolerates the post-success close (otherwise a finished backup would be
    /// misreported as failed).
    pub async fn finish(&mut self) -> Result<()> {
        let (status, body) = self
            .conn
            .request_committing(Method::POST, "/finish", None, None)
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
        let encoded = blob::encode_auto(chunk);
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

/// Outcome of a deduplicated dynamic backup.
#[derive(Debug, Clone, Default)]
pub struct BackupStats {
    /// Total chunks in the archive.
    pub chunks: u64,
    /// Chunks already present on the server (re-referenced, not uploaded).
    pub reused: u64,
    /// Chunks uploaded this run.
    pub uploaded: u64,
    /// Total archive bytes (plaintext).
    pub bytes: u64,
    /// Plaintext bytes of the chunks uploaded this run (i.e. not deduplicated).
    pub uploaded_bytes: u64,
    /// Encoded bytes actually stored for the uploaded chunks (after compression
    /// and/or encryption). `uploaded_bytes / stored_bytes` is the compression
    /// ratio; `bytes / stored_bytes` is the overall reduction including dedup.
    pub stored_bytes: u64,
    /// Archive index csum.
    pub csum: [u8; DIGEST_LEN],
}

/// A live snapshot of a backup in progress, delivered to the progress callback
/// after each chunk. All byte figures are cumulative for the current archive.
#[derive(Debug, Clone, Copy, Default)]
pub struct BackupProgress {
    /// Plaintext bytes processed so far (the chunk-stream offset).
    pub bytes_done: u64,
    /// Estimated total plaintext bytes, for a percentage (0 if unknown).
    pub total_bytes: u64,
    /// Chunks seen so far.
    pub chunks: u64,
    /// Chunks already on the server (deduplicated, not uploaded).
    pub reused: u64,
    /// Chunks uploaded so far.
    pub uploaded: u64,
    /// Plaintext bytes of the chunks uploaded so far.
    pub uploaded_bytes: u64,
    /// Plaintext bytes of the chunks deduplicated away so far.
    pub reused_bytes: u64,
    /// Encoded bytes stored so far for uploaded chunks (lags slightly: chunks
    /// still encoding are not yet counted).
    pub stored_bytes: u64,
}

/// Back up a file as a deduplicated dynamic-index archive, then finish the
/// snapshot. With `dedup_with_previous`, the previous snapshot's chunk list is
/// fetched and unchanged chunks are skipped (only changed chunks are uploaded).
pub async fn backup_dynamic_file(
    params: &SessionParams,
    archive_name: &str,
    path: &Path,
    dedup_with_previous: bool,
) -> Result<BackupStats> {
    backup_dynamic_file_with_progress(
        params,
        archive_name,
        path,
        dedup_with_previous,
        true,
        None,
        None,
        |_| {},
    )
    .await
}

/// Like [`backup_dynamic_file`] but reports progress as `(bytes_done, total_bytes)`
/// after each chunk, optionally compresses chunks with zstd (`compress`), and
/// optionally encrypts with `crypt`. Chunking and hashing run on a blocking
/// thread; compression and uploads run concurrently.
#[allow(clippy::too_many_arguments)]
pub async fn backup_dynamic_file_with_progress(
    params: &SessionParams,
    archive_name: &str,
    path: &Path,
    dedup_with_previous: bool,
    compress: bool,
    catalog: Option<(String, Vec<u8>)>,
    crypt: Option<CryptConfig>,
    on_progress: impl FnMut(&BackupProgress),
) -> Result<BackupStats> {
    let total_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let file = std::fs::File::open(path)
        .map_err(|e| PbsError::Protocol(format!("opening {}: {e}", path.display())))?;
    // The file catalog is known up front; hand it to the reader through the same
    // deferred channel a streamed source (SQL) uses, sending it immediately.
    let catalog_rx = catalog.map(|c| {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(c);
        rx
    });
    backup_dynamic_reader(
        params,
        archive_name,
        dedup_with_previous,
        compress,
        file,
        total_bytes,
        catalog_rx,
        crypt,
        on_progress,
    )
    .await
}

/// Back up an arbitrary byte stream as a deduplicated dynamic-index archive,
/// then finish the snapshot. `total_bytes` is used only for progress reporting
/// (pass 0 if unknown). With `compress`, each new chunk is zstd-compressed (only
/// when that shrinks it); with `crypt`, chunks (and the extra blob) are
/// AES-256-GCM encrypted and the chunk digests are keyed, matching the PBS scheme.
/// The reader is consumed on a blocking thread while chunking, compression, and
/// upload proceed concurrently, so a streamed source (e.g. a SQL VDI backup) is
/// never staged.
///
/// `catalog` is an optional extra named blob (a file listing, or SQL chain
/// metadata) delivered through a channel so it can be computed *after* the stream
/// drains, when the source's metadata is known. It is uploaded just before the
/// snapshot is finalised; if the sender drops without sending, no blob is added.
#[allow(clippy::too_many_arguments)]
pub async fn backup_dynamic_reader<R: std::io::Read + Send + 'static>(
    params: &SessionParams,
    archive_name: &str,
    dedup_with_previous: bool,
    compress: bool,
    reader: R,
    total_bytes: u64,
    catalog: Option<tokio::sync::oneshot::Receiver<(String, Vec<u8>)>>,
    crypt: Option<CryptConfig>,
    mut on_progress: impl FnMut(&BackupProgress),
) -> Result<BackupStats> {
    let mut writer = BackupWriter::connect(params).await?;

    let mut known: HashSet<[u8; DIGEST_LEN]> = HashSet::new();
    if dedup_with_previous {
        if let Some(prev) = writer.download_previous(archive_name).await? {
            if let Ok(index) = DynamicIndex::parse(&prev) {
                known.extend(index.digests().copied());
            }
        }
    }

    let wid = writer.create_dynamic_index(archive_name).await?;

    // Digests are computed on the chunker thread; keyed when encrypting so that
    // dedup still works under one key.
    let digest_crypt = crypt.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<([u8; DIGEST_LEN], Vec<u8>)>(8);
    let chunker_task = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        for chunk in chunker::chunk_reader(reader) {
            let data = chunk?;
            let digest = match &digest_crypt {
                Some(c) => c.compute_digest(&data),
                None => index::chunk_digest(&data),
            };
            if tx.blocking_send((digest, data)).is_err() {
                break;
            }
        }
        Ok(())
    });

    let mut builder = DynamicIndexBuilder::new(params.backup_time, random_uuid());
    // The full ordered index (digest + start offset). The index is built in order
    // as chunks arrive, but appended only after every chunk it references has been
    // uploaded, since chunk DATA is uploaded concurrently below.
    let mut all_digests: Vec<[u8; DIGEST_LEN]> = Vec::new();
    let mut all_offsets: Vec<u64> = Vec::new();
    let mut end_offset = 0u64;
    let mut chunks = 0u64;
    let mut reused = 0u64;
    let mut uploaded = 0u64;
    let mut uploaded_bytes = 0u64;
    let mut reused_bytes = 0u64;
    // Encoded bytes are summed by the upload tasks (the encoded size is only known
    // after compression, which runs off this loop), so progress reads it atomically.
    let stored_bytes = Arc::new(AtomicU64::new(0));

    // Pipeline chunk-data uploads: keep up to `pipeline_width()` in flight on the
    // one HTTP/2 connection instead of awaiting each in turn.
    let uploader = writer.chunk_uploader();
    let permits = Arc::new(tokio::sync::Semaphore::new(pipeline_width()));
    let mut inflight = tokio::task::JoinSet::new();

    while let Some((digest, data)) = rx.recv().await {
        let start_offset = end_offset;
        let len = data.len() as u64;
        end_offset += len;
        chunks += 1;
        if known.contains(&digest) {
            reused += 1;
            reused_bytes += len;
        } else {
            uploaded_bytes += len;
            // Acquiring a permit bounds in-flight work (back-pressuring the
            // chunker once the window is full). Acquired before spawning so at
            // most `pipeline_width()` chunks encode/upload concurrently.
            let permit = permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| PbsError::Protocol("upload semaphore closed".into()))?;
            let up = uploader.clone();
            let crypt = crypt.clone();
            let stored = stored_bytes.clone();
            inflight.spawn(async move {
                let _permit = permit; // released when this upload finishes
                                      // Compression and encryption are CPU-bound; run them on a blocking
                                      // thread so chunks encode in parallel without stalling the async
                                      // workers or the h2 connection driver.
                let encoded = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                    match (&crypt, compress) {
                        (Some(c), true) => c.encrypt_compressed_blob(&data),
                        (Some(c), false) => c.encrypt_blob(&data),
                        (None, true) => Ok(blob::encode_auto(&data)),
                        (None, false) => Ok(blob::encode_uncompressed(&data)),
                    }
                })
                .await
                .map_err(|e| PbsError::Protocol(format!("encode task failed: {e}")))??;
                stored.fetch_add(encoded.len() as u64, Ordering::Relaxed);
                up.upload_dynamic_chunk(wid, digest, len, encoded).await
            });
            uploaded += 1;
            // Surface a failed upload promptly and reap finished tasks.
            while let Some(joined) = inflight.try_join_next() {
                joined.map_err(|e| PbsError::Protocol(format!("upload task failed: {e}")))??;
            }
        }
        builder.push(end_offset, digest);
        on_progress(&BackupProgress {
            bytes_done: end_offset,
            total_bytes,
            chunks,
            reused,
            uploaded,
            uploaded_bytes,
            reused_bytes,
            stored_bytes: stored_bytes.load(Ordering::Relaxed),
        });
        all_digests.push(digest);
        all_offsets.push(start_offset);
    }

    // Every chunk's data must be uploaded before the index references it.
    while let Some(joined) = inflight.join_next().await {
        joined.map_err(|e| PbsError::Protocol(format!("upload task failed: {e}")))??;
    }
    for (digests, offsets) in all_digests
        .chunks(APPEND_BATCH)
        .zip(all_offsets.chunks(APPEND_BATCH))
    {
        writer.append_dynamic_index(wid, digests, offsets).await?;
    }

    chunker_task
        .await
        .map_err(|e| PbsError::Protocol(format!("chunker task failed: {e}")))??;

    let size = builder.total_size();
    let csum = builder.index_csum();
    writer
        .close_dynamic_index(wid, builder.entry_count() as u64, size, &csum)
        .await?;

    // Optional catalog blob (e.g. a file listing) so browsing does not require
    // downloading the whole archive. Stored alongside the archive; not in the
    // manifest (the reader serves it by name regardless). Encrypted with the
    // chunks, since it contains file paths.
    if let Some(rx) = catalog {
        // The blob may be produced after the stream (e.g. SQL metadata known only
        // once BACKUP yields); a dropped sender just means no extra blob.
        if let Ok((name, content)) = rx.await {
            let catalog_blob = match (&crypt, compress) {
                (Some(c), true) => c.encrypt_compressed_blob(&content)?,
                (Some(c), false) => c.encrypt_blob(&content)?,
                (None, true) => blob::encode_auto(&content),
                (None, false) => blob::encode_uncompressed(&content),
            };
            writer.upload_blob(&name, &catalog_blob).await?;
        }
    }

    let entry = FileEntry::with_crypt(archive_name, size, &csum, crypt.is_some());
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

    Ok(BackupStats {
        chunks,
        reused,
        uploaded,
        bytes: size,
        uploaded_bytes,
        stored_bytes: stored_bytes.load(Ordering::Relaxed),
        csum,
    })
}

/// A reader session for restoring from a snapshot.
/// A cloneable handle for downloading chunk data concurrently on a reader
/// connection. Each clone carries its own h2 `SendRequest`, so downloads spawned
/// from it run as separate multiplexed HTTP/2 streams (the read-side mirror of
/// [`ChunkUploader`]), which is what lets a restore keep many chunk GETs in flight
/// instead of waiting a full round-trip per chunk.
#[derive(Clone)]
pub struct ChunkDownloader {
    send: SendRequest<Bytes>,
    authority: String,
}

impl ChunkDownloader {
    /// Download one chunk's DataBlob by digest. Takes `&self` so it can run from a
    /// spawned task.
    pub async fn download_chunk(&self, digest: &[u8; DIGEST_LEN]) -> Result<Vec<u8>> {
        let q = format!("/chunk?digest={}", hex::encode(digest));
        let (status, body) = h2_request(
            &self.send,
            &self.authority,
            Method::GET,
            &q,
            None,
            None,
            false,
        )
        .await?;
        ensure_ok(status, &body)?;
        Ok(body)
    }
}

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

    /// A cloneable handle for downloading chunks concurrently on this connection.
    pub fn chunk_downloader(&self) -> ChunkDownloader {
        ChunkDownloader {
            send: self.conn.send.clone(),
            authority: self.conn.authority.clone(),
        }
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

    /// Download a named blob and return its decoded (and, if encrypted,
    /// decrypted) contents. `crypt` is needed only for encrypted blobs.
    pub async fn download_blob(
        &mut self,
        file_name: &str,
        crypt: Option<&CryptConfig>,
    ) -> Result<Vec<u8>> {
        let raw = self.download(file_name).await?;
        decode_blob(&raw, crypt)
    }

    /// Download a fixed-index archive and reassemble the original image bytes.
    pub async fn restore_fixed_image(
        &mut self,
        archive_name: &str,
        crypt: Option<&CryptConfig>,
    ) -> Result<Vec<u8>> {
        let index_bytes = self.download(archive_name).await?;
        let fixed = FixedIndex::parse(&index_bytes)?;
        if !fixed.verify_csum() {
            return Err(PbsError::Protocol(
                "downloaded fixed index failed its csum check".into(),
            ));
        }
        let mut image = Vec::with_capacity((fixed.size as usize).min(MAX_RESTORE_PREALLOC));
        for digest in &fixed.digests {
            let chunk_blob = self.download_chunk(digest).await?;
            let chunk = decode_and_verify_chunk(&chunk_blob, crypt, digest)?;
            image.extend_from_slice(&chunk);
        }
        image.truncate(fixed.size as usize);
        Ok(image)
    }

    /// Download a dynamic-index archive and reassemble the original bytes.
    pub async fn restore_dynamic_archive(
        &mut self,
        archive_name: &str,
        crypt: Option<&CryptConfig>,
    ) -> Result<Vec<u8>> {
        let index_bytes = self.download(archive_name).await?;
        let index = DynamicIndex::parse(&index_bytes)?;
        if !index.verify_csum() {
            return Err(PbsError::Protocol(
                "downloaded dynamic index failed its csum check".into(),
            ));
        }
        let mut out = Vec::with_capacity((index.total_size() as usize).min(MAX_RESTORE_PREALLOC));
        for digest in index.digests() {
            let chunk_blob = self.download_chunk(digest).await?;
            out.extend_from_slice(&decode_and_verify_chunk(&chunk_blob, crypt, digest)?);
        }
        Ok(out)
    }

    /// Download and verify an archive's dynamic index, returning its chunk digests
    /// in archive order (the input to the restore pipeline).
    async fn archive_digests(&mut self, archive_name: &str) -> Result<Vec<[u8; DIGEST_LEN]>> {
        let index_bytes = self.download(archive_name).await?;
        let index = DynamicIndex::parse(&index_bytes)?;
        if !index.verify_csum() {
            return Err(PbsError::Protocol(
                "downloaded dynamic index failed its csum check".into(),
            ));
        }
        Ok(index.digests().copied().collect())
    }

    /// Stream a dynamic-index archive to an async writer, decoding (and decrypting)
    /// each chunk as it arrives. Unlike [`Self::restore_dynamic_archive`], the whole
    /// archive is never held in memory, so a very large archive (a multi-hundred-GB
    /// SQL backup) can be written straight to a file. Returns the bytes written.
    ///
    /// Chunks download and decode through a pipeline of up to [`pipeline_width`]
    /// concurrent tasks (see [`spawn_chunk_pipeline`]) and are written in order, so
    /// the network read overlaps the disk write instead of taking turns.
    pub async fn restore_dynamic_archive_to_writer<W>(
        &mut self,
        archive_name: &str,
        crypt: Option<&CryptConfig>,
        out: &mut W,
    ) -> Result<u64>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let digests = self.archive_digests(archive_name).await?;
        let (mut rx, producer) =
            spawn_chunk_pipeline(self.chunk_downloader(), digests, crypt.cloned());
        let mut written = 0u64;
        while let Some(chunk) = rx.recv().await {
            out.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        join_pipeline(producer).await?;
        out.flush().await?;
        Ok(written)
    }

    /// Stream a dynamic-index archive, handing each decoded (and decrypted) chunk to
    /// `on_chunk` in order. Like [`Self::restore_dynamic_archive_to_writer`] but the
    /// sink is an async callback, so the bytes can be fed to a consumer that is not a
    /// writer (e.g. a channel driving a SQL Server VDI restore). It uses the same
    /// concurrent download pipeline, so the restore prefetches chunks ahead of the
    /// consumer instead of waiting a round-trip per chunk. Returns the bytes produced;
    /// a callback error ends the stream early.
    pub async fn restore_dynamic_archive_streamed<F, Fut>(
        &mut self,
        archive_name: &str,
        crypt: Option<&CryptConfig>,
        mut on_chunk: F,
    ) -> Result<u64>
    where
        F: FnMut(Vec<u8>) -> Fut,
        Fut: std::future::Future<Output = std::io::Result<()>>,
    {
        let digests = self.archive_digests(archive_name).await?;
        let (mut rx, producer) =
            spawn_chunk_pipeline(self.chunk_downloader(), digests, crypt.cloned());
        let mut produced = 0u64;
        while let Some(chunk) = rx.recv().await {
            produced += chunk.len() as u64;
            on_chunk(chunk).await?;
        }
        join_pipeline(producer).await?;
        Ok(produced)
    }
}

/// Spawn a pipeline that downloads and decodes an archive's chunks through a window
/// of up to [`pipeline_width`] concurrent tasks and emits them IN ORDER on the
/// returned channel: chunks are downloaded out of order but delivered sequentially,
/// so both a file write and a VDI stream (which must be in order) can consume it.
/// Decoding runs on the blocking pool so it does not stall the connection driver.
/// The producer handle resolves once every chunk is sent (or a download/decode error
/// is hit); the caller drains the receiver, then [`join_pipeline`]s the handle to
/// surface any error. Shared by the to-file and in-place (VDI) restore paths.
fn spawn_chunk_pipeline(
    downloader: ChunkDownloader,
    digests: Vec<[u8; DIGEST_LEN]>,
    crypt: Option<CryptConfig>,
) -> (
    tokio::sync::mpsc::Receiver<Vec<u8>>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let width = pipeline_width();
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(width);
    let producer = tokio::spawn(async move {
        // Download one chunk and decode it (decompress/decrypt) off the runtime.
        let spawn_one = |digest: [u8; DIGEST_LEN]| {
            let downloader = downloader.clone();
            let crypt = crypt.clone();
            tokio::spawn(async move {
                let blob = downloader.download_chunk(&digest).await?;
                tokio::task::spawn_blocking(move || {
                    decode_and_verify_chunk(&blob, crypt.as_ref(), &digest)
                })
                .await
                .map_err(|e| PbsError::Protocol(format!("decode task failed: {e}")))?
            })
        };

        // Keep a window of in-flight downloads; await them in order and forward each
        // downstream while later chunks are still downloading.
        let mut digest_iter = digests.into_iter();
        let mut inflight: std::collections::VecDeque<_> = std::collections::VecDeque::new();
        for _ in 0..width {
            match digest_iter.next() {
                Some(d) => inflight.push_back(spawn_one(d)),
                None => break,
            }
        }
        while let Some(handle) = inflight.pop_front() {
            let chunk = handle
                .await
                .map_err(|e| PbsError::Protocol(format!("download task failed: {e}")))??;
            // Stop if the consumer is gone (the writer or the SQL restore failed).
            if tx.send(chunk).await.is_err() {
                break;
            }
            if let Some(d) = digest_iter.next() {
                inflight.push_back(spawn_one(d));
            }
        }
        Ok(())
    });
    (rx, producer)
}

/// Await a [`spawn_chunk_pipeline`] producer, surfacing a join failure or the
/// pipeline's own download/decode error.
async fn join_pipeline(producer: tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    producer
        .await
        .map_err(|e| PbsError::Protocol(format!("download pipeline task failed: {e}")))?
}

/// Decode a downloaded blob, decrypting it when it is an encrypted blob and
/// decompressing zstd blobs. Encrypted blobs (compressed or not) are recognized
/// by their magic, so plaintext blobs (e.g. the manifest) still decode without a
/// key; `blob::decode` handles the uncompressed and zstd variants.
fn decode_blob(raw: &[u8], crypt: Option<&CryptConfig>) -> Result<Vec<u8>> {
    let is_encrypted = raw.len() >= 8
        && (raw[0..8] == blob::MAGIC_ENCRYPTED || raw[0..8] == blob::MAGIC_ENCRYPTED_ZSTD);
    if is_encrypted {
        match crypt {
            Some(c) => c.decrypt_blob(raw),
            None => Err(PbsError::Protocol(
                "this snapshot is encrypted but no encryption key was provided".into(),
            )),
        }
    } else {
        blob::decode(raw)
    }
}

/// Decode a downloaded chunk and verify it against the digest it was fetched by.
///
/// PBS chunks are content addressed: the digest is SHA-256 of the chunk's
/// plaintext, keyed with the encryption key's `id_key` when encrypted (the same
/// way the backup side computes it). Recomputing it and comparing detects a
/// datastore that returned corrupt or substituted chunk data, which the index's
/// own csum check cannot (that only proves the index is internally consistent, not
/// that the chunks behind the digests are intact). A mismatch fails the restore
/// rather than reassembling silently wrong data.
fn decode_and_verify_chunk(
    raw: &[u8],
    crypt: Option<&CryptConfig>,
    expected: &[u8; DIGEST_LEN],
) -> Result<Vec<u8>> {
    let data = decode_blob(raw, crypt)?;
    let actual = match crypt {
        Some(c) => c.compute_digest(&data),
        None => index::chunk_digest(&data),
    };
    if &actual != expected {
        return Err(PbsError::Protocol(format!(
            "chunk {} failed verification: its data does not match its digest \
             (corrupt or tampered datastore)",
            hex::encode(expected)
        )));
    }
    Ok(data)
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
    fn verifies_unencrypted_chunk_against_its_digest() {
        let data = b"the quick brown fox".repeat(40);
        let raw = blob::encode_auto(&data);
        let digest = index::chunk_digest(&data);
        // The right digest decodes; a wrong one is rejected as corrupt.
        assert_eq!(decode_and_verify_chunk(&raw, None, &digest).unwrap(), data);
        let mut wrong = digest;
        wrong[0] ^= 0xff;
        assert!(decode_and_verify_chunk(&raw, None, &wrong).is_err());
    }

    #[test]
    fn verifies_encrypted_chunk_against_its_keyed_digest() {
        let crypt = CryptConfig::new([7u8; 32]);
        let data = b"secret chunk bytes".repeat(40);
        let raw = crypt.encrypt_blob(&data).unwrap();
        let digest = crypt.compute_digest(&data);
        assert_eq!(
            decode_and_verify_chunk(&raw, Some(&crypt), &digest).unwrap(),
            data
        );
        let mut wrong = digest;
        wrong[0] ^= 0xff;
        assert!(decode_and_verify_chunk(&raw, Some(&crypt), &wrong).is_err());
    }

    #[test]
    fn rejects_a_chunk_whose_payload_was_swapped() {
        // A datastore that returns a valid blob for the wrong digest (substituted
        // or corrupted content) must be caught, not reassembled.
        let real = blob::encode_auto(b"real chunk contents");
        let other_digest = index::chunk_digest(b"a different chunk entirely");
        assert!(decode_and_verify_chunk(&real, None, &other_digest).is_err());
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

    #[test]
    fn trims_whitespace_from_auth_id_and_secret() {
        // A repository whose auth id has stray spaces, and a secret pasted with a
        // trailing newline: both must be normalized so the Authorization header is
        // not sent with whitespace that PBS would reject.
        let repo: Repository = "  tok@pbs!t @pbs.example.com:8007:store"
            .trim()
            .parse()
            .unwrap();
        let p =
            SessionParams::from_repository(&repo, "  s3cret\n", "ab".repeat(32), "host", "h", 1)
                .unwrap();
        assert_eq!(p.auth_id, "tok@pbs!t");
        assert_eq!(p.secret, "s3cret");
    }
}
