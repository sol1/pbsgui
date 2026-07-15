//! TLS for the relay: the proxy listens with a self-signed certificate that
//! agents pin by SHA-256 fingerprint - the same trust model pbsgui already
//! uses toward PBS, so nothing new for an admin to reason about.
//!
//! The certificate and key persist in DER form under the proxy's config
//! directory; the fingerprint is what the admin copies to each agent.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use sha2::{Digest, Sha256};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::{rustls, TlsAcceptor, TlsConnector};

const CERT_FILE: &str = "relay-cert.der";
const KEY_FILE: &str = "relay-key.der";

/// The proxy's TLS acceptor and the certificate fingerprint agents must pin.
pub struct ServerTls {
    pub acceptor: TlsAcceptor,
    pub fingerprint: String,
}

/// Load the relay certificate from `dir`, generating and persisting a fresh
/// self-signed one (valid for the host name plus "pbsgui-relay") on first run.
pub fn server_tls(dir: &Path) -> anyhow::Result<ServerTls> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating the relay config dir {}", dir.display()))?;
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);

    let (cert_der, key_der) = if cert_path.exists() && key_path.exists() {
        (
            std::fs::read(&cert_path).context("reading the relay certificate")?,
            std::fs::read(&key_path).context("reading the relay key")?,
        )
    } else {
        let mut names = vec!["pbsgui-relay".to_string()];
        if let Ok(host) = hostname() {
            names.push(host);
        }
        let generated = rcgen::generate_simple_self_signed(names)
            .context("generating the relay certificate")?;
        let cert = generated.cert.der().to_vec();
        let key = generated.key_pair.serialize_der();
        std::fs::write(&cert_path, &cert).context("persisting the relay certificate")?;
        std::fs::write(&key_path, &key).context("persisting the relay key")?;
        (cert, key)
    };

    let fingerprint = fingerprint_hex(&cert_der);
    let provider = rustls::crypto::ring::default_provider();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .context("relay TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert_der)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
        )
        .context("relay TLS certificate")?;
    Ok(ServerTls {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        fingerprint,
    })
}

/// A connector for the agent side: accepts exactly the proxy certificate with
/// this fingerprint (pbs-client's pinned verifier, the PBS trust model).
pub fn connector(fingerprint: &str) -> anyhow::Result<TlsConnector> {
    pbs_client::session::tls_connector(fingerprint)
        .map_err(|e| anyhow::anyhow!("relay TLS connector: {e}"))
}

/// Colon-separated upper-case hex SHA-256 of the DER certificate, the format
/// PBS shows fingerprints in (and what `connector` accepts).
pub fn fingerprint_hex(cert_der: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(cert_der).into();
    digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn hostname() -> anyhow::Result<String> {
    // Windows sets COMPUTERNAME; elsewhere HOSTNAME is best-effort (the name is
    // only a convenience SAN, trust comes from the fingerprint).
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .context("no host name in the environment")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Generate a certificate, accept one pinned TLS connection with it, and
    /// confirm the wrong fingerprint is refused. Also proves the persisted
    /// cert reloads with the same fingerprint.
    #[tokio::test]
    async fn pinned_handshake_works_and_wrong_fingerprint_fails() {
        let dir = std::env::temp_dir().join(format!("pbsgui-relay-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let server = server_tls(&dir).unwrap();
        let again = server_tls(&dir).unwrap();
        assert_eq!(
            server.fingerprint, again.fingerprint,
            "the persisted certificate must reload identically"
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = server.acceptor.clone();
        let serve = tokio::spawn(async move {
            // Two connection attempts: the good one echoes a byte, the bad one
            // fails its handshake.
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    let mut b = [0u8; 1];
                    let _ = tls.read_exact(&mut b).await;
                    let _ = tls.write_all(&b).await;
                    let _ = tls.shutdown().await;
                }
            }
        });

        let name = tokio_rustls::rustls::pki_types::ServerName::try_from("pbsgui-relay").unwrap();
        let good = connector(&server.fingerprint).unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut tls = good.connect(name.clone(), stream).await.unwrap();
        tls.write_all(&[42]).await.unwrap();
        let mut b = [0u8; 1];
        tls.read_exact(&mut b).await.unwrap();
        assert_eq!(b, [42]);
        drop(tls);

        let bad = connector(&"00".repeat(32)).unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert!(
            bad.connect(name, stream).await.is_err(),
            "a mismatched fingerprint must fail the handshake"
        );

        serve.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
