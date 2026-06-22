//! Per-job client-side encryption keys.
//!
//! Each encrypted job owns a 32-byte AES key, stored base64-encoded in the OS
//! credential store under `enc:<job_id>` (alongside the PBS and SQL secrets, see
//! [`crate::secrets`]). The key never lives in the job config or travels over
//! IPC except when the user explicitly reveals it to copy into a password
//! manager. Backups encrypt with it and restores decrypt transparently, both by
//! loading a [`CryptConfig`] via [`load_config`].

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use pbs_client::CryptConfig;
use pbsgui_ipc::EncryptionKeyInfo;

use crate::secrets;

/// The credential-store key for a job's encryption key.
pub fn enc_secret_key(job_id: &str) -> String {
    format!("enc:{job_id}")
}

/// Generate a fresh random key for a job and store it. Fails if one already
/// exists, so a generate never silently overwrites a key still protecting old
/// backups.
pub fn generate(job_id: &str) -> anyhow::Result<EncryptionKeyInfo> {
    if secrets::get(&enc_secret_key(job_id))?.is_some() {
        anyhow::bail!("this job already has an encryption key; clear it first to replace it");
    }
    let key = pbs_client::random_key()?;
    store(job_id, &key)?;
    Ok(info_from_key(&key))
}

/// Import an existing base64 key for a job (replacing any current one), so a key
/// can be reused across jobs or machines.
pub fn import(job_id: &str, key_base64: &str) -> anyhow::Result<EncryptionKeyInfo> {
    let key = decode_key(key_base64)?;
    store(job_id, &key)?;
    Ok(info_from_key(&key))
}

/// The stored key's fingerprint, or `None` if there is none. The raw key is
/// deliberately blanked: it is handed to the GUI only once, when generated or
/// imported, never on a later status read.
pub fn get(job_id: &str) -> anyhow::Result<Option<EncryptionKeyInfo>> {
    match secrets::get(&enc_secret_key(job_id))? {
        Some(b64) => {
            let mut info = info_from_key(&decode_key(&b64)?);
            info.key = String::new();
            Ok(Some(info))
        }
        None => Ok(None),
    }
}

/// Delete a job's stored key.
pub fn clear(job_id: &str) -> anyhow::Result<()> {
    secrets::delete(&enc_secret_key(job_id))
}

/// Load a job's key as a [`CryptConfig`] for backup/restore, or `None` if the
/// job has no key stored.
pub fn load_config(job_id: &str) -> anyhow::Result<Option<CryptConfig>> {
    match secrets::get(&enc_secret_key(job_id))? {
        Some(b64) => Ok(Some(CryptConfig::new(decode_key(&b64)?))),
        None => Ok(None),
    }
}

/// The encryption config for a job, honoring its `encrypted` flag: `Ok(None)`
/// for an unencrypted job, the loaded key for an encrypted one, or an error if
/// the job is marked encrypted but its key has gone missing. Used by both backup
/// (so we never write plaintext into an "encrypted" group) and restore (so a
/// decrypt never silently fails for lack of a key).
pub fn for_job(job: &pbsgui_ipc::Job) -> anyhow::Result<Option<CryptConfig>> {
    if !job.encrypted {
        return Ok(None);
    }
    load_config(&job.id)?
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("job is marked encrypted but its key is not stored"))
}

fn store(job_id: &str, key: &[u8; 32]) -> anyhow::Result<()> {
    secrets::set(&enc_secret_key(job_id), &STANDARD.encode(key))
}

/// Decode a base64 key and confirm it is exactly 32 bytes.
fn decode_key(key_base64: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = STANDARD
        .decode(key_base64.trim())
        .map_err(|e| anyhow::anyhow!("the key is not valid base64: {e}"))?;
    let key: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("a key must be 32 bytes; got {}", bytes.len()))?;
    Ok(key)
}

/// Build the display info (base64 key + colon-grouped fingerprint) for a key.
fn info_from_key(key: &[u8; 32]) -> EncryptionKeyInfo {
    let fingerprint = CryptConfig::new(*key).fingerprint();
    EncryptionKeyInfo {
        key: STANDARD.encode(key),
        fingerprint: colon_hex(&fingerprint),
    }
}

/// Lowercase hex grouped in colon-separated byte pairs, as PBS shows key
/// fingerprints (e.g. `a1:b2:c3:...`).
fn colon_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_wrong_length() {
        assert!(decode_key(&STANDARD.encode([0u8; 16])).is_err());
        assert!(decode_key("not base64!!!").is_err());
        assert!(decode_key(&STANDARD.encode([0u8; 32])).is_ok());
    }

    #[test]
    fn info_round_trips_through_base64() {
        let key = [42u8; 32];
        let info = info_from_key(&key);
        assert_eq!(decode_key(&info.key).unwrap(), key);
        // 32 bytes => 31 colons.
        assert_eq!(info.fingerprint.matches(':').count(), 31);
    }
}
