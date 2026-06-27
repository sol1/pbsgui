//! Tamper-evident persistence for the engine's on-disk JSON stores.
//!
//! Each store is written as a small envelope: the exact JSON text plus an
//! HMAC-SHA256 over it, keyed by a 32-byte secret kept in the OS credential
//! store (so only the service account can read it). On load the MAC is
//! re-checked; a mismatch means the file was corrupted or tampered with and the
//! caller refuses to trust it.
//!
//! The directory ACL (see [`crate::config::ensure_dirs`]) is the primary control
//! that stops an untrusted user writing these files at all; the signature is
//! defense in depth and detects corruption or any write that bypasses the ACL.
//! For a smooth upgrade, a legacy unsigned file (a bare JSON value, as written
//! before signing existed) is still accepted and re-signed on the next write.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::secrets;

type HmacSha256 = Hmac<Sha256>;

/// Credential-store key for the per-machine store-signing secret.
const HMAC_KEY_NAME: &str = "store-hmac";

#[derive(Serialize, Deserialize)]
struct Envelope {
    /// HMAC-SHA256 of `data`, base64.
    mac: String,
    /// The exact JSON text the MAC covers.
    data: String,
}

/// Fetch the signing key, generating and storing one on first use. Cached after
/// the first read, and the get-or-create is serialized so two threads never each
/// generate (and store) a different key.
fn signing_key() -> anyhow::Result<Vec<u8>> {
    static CACHE: OnceLock<Vec<u8>> = OnceLock::new();
    if let Some(key) = CACHE.get() {
        return Ok(key.clone());
    }
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    if let Some(key) = CACHE.get() {
        return Ok(key.clone());
    }
    let key = match secrets::get(HMAC_KEY_NAME)? {
        Some(b64) => STANDARD.decode(b64)?,
        None => {
            let key = pbs_client::random_key()?;
            secrets::set(HMAC_KEY_NAME, &STANDARD.encode(key))?;
            key.to_vec()
        }
    };
    let _ = CACHE.set(key.clone());
    Ok(key)
}

fn tag(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Serialize `value` to a signed envelope (pretty JSON).
pub fn serialize<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    let data = serde_json::to_string_pretty(value)?;
    let key = signing_key()?;
    let mac = STANDARD.encode(tag(&key, data.as_bytes()));
    Ok(serde_json::to_vec_pretty(&Envelope { mac, data })?)
}

/// Parse a store file. A signed envelope has its MAC verified (an error on
/// mismatch). A legacy unsigned value (written before signing existed) is
/// accepted, to be re-signed on the next write.
pub fn deserialize<T: DeserializeOwned>(bytes: &[u8]) -> anyhow::Result<T> {
    if let Ok(env) = serde_json::from_slice::<Envelope>(bytes) {
        let key = signing_key()?;
        let expected = STANDARD
            .decode(&env.mac)
            .map_err(|e| anyhow::anyhow!("store MAC is not valid base64: {e}"))?;
        // Constant-time verification via the MAC type.
        let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts a key of any length");
        mac.update(env.data.as_bytes());
        mac.verify_slice(&expected)
            .map_err(|_| anyhow::anyhow!("store signature mismatch: corrupt or tampered with"))?;
        return Ok(serde_json::from_str(&env.data)?);
    }
    Ok(serde_json::from_slice(bytes)?)
}

/// Write `bytes` to `path` atomically: a uniquely-named temp file in the same
/// directory, then a rename over the target. Two concurrent writers never
/// interleave into a half-written file (each renames its own temp; the last
/// wins, and the file is always a complete, valid snapshot).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), n));
    std::fs::write(&tmp, bytes)?;
    // std::fs::rename replaces an existing file on both Windows and Unix.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Item {
        id: String,
        n: u32,
    }

    fn sample() -> Vec<Item> {
        vec![
            Item {
                id: "a".into(),
                n: 1,
            },
            Item {
                id: "b".into(),
                n: 2,
            },
        ]
    }

    #[test]
    fn round_trips_signed() {
        let bytes = serialize(&sample()).unwrap();
        let back: Vec<Item> = deserialize(&bytes).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn rejects_tampered_payload() {
        let bytes = serialize(&sample()).unwrap();
        let mut env: Envelope = serde_json::from_slice(&bytes).unwrap();
        // Flip a value in the signed text without updating the MAC.
        env.data = env.data.replace("\"n\": 1", "\"n\": 9");
        let tampered = serde_json::to_vec(&env).unwrap();
        assert!(deserialize::<Vec<Item>>(&tampered).is_err());
    }

    #[test]
    fn accepts_legacy_unsigned() {
        // A bare JSON array, as written before signing existed.
        let legacy = serde_json::to_vec_pretty(&sample()).unwrap();
        let back: Vec<Item> = deserialize(&legacy).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn write_atomic_replaces_existing() {
        let dir = std::env::temp_dir().join(format!("pbsgui-signed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
