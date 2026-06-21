//! Client-side encryption, byte-compatible with Proxmox Backup Server.
//!
//! Matches PBS's scheme so a backup written here can be read by the official
//! `proxmox-backup-client` given the same key (and vice versa):
//!
//! - Each chunk is an AES-256-GCM encrypted `DataBlob`:
//!   `magic[8] | crc32-le[4] | iv[16] | tag[16] | ciphertext`, where the CRC is
//!   over the ciphertext, the IV is 16 random bytes, and the AAD is empty.
//! - The chunk digest used for deduplication is keyed:
//!   `SHA-256(plaintext ‖ id_key)`, with
//!   `id_key = PBKDF2-HMAC-SHA256(enc_key, "_id_key", 10 iterations, 32 bytes)`.
//!   This keeps dedup working under one key while preventing cross-key
//!   correlation.
//! - The key fingerprint is `SHA-256(FINGERPRINT_INPUT ‖ id_key)`.

use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::aes::Aes256;
use aes_gcm::{AesGcm, Nonce, Tag};
use sha2::{Digest, Sha256};

use crate::blob::{crc32, MAGIC_ENCRYPTED};
use crate::error::{PbsError, Result};

/// AES-256-GCM with PBS's 16-byte IV (the default 16-byte tag).
type Aes256Gcm16 = AesGcm<Aes256, U16>;

/// Encrypted blob header: `magic[8] | crc[4] | iv[16] | tag[16]`.
const ENC_HEADER_SIZE: usize = 8 + 4 + 16 + 16;

/// `sha256("Proxmox Backup Encryption Key Fingerprint")` (from PBS source).
const FINGERPRINT_INPUT: [u8; 32] = [
    110, 208, 239, 119, 71, 31, 255, 77, 85, 199, 168, 254, 74, 157, 182, 33, 97, 64, 127, 19, 76,
    114, 93, 223, 48, 153, 45, 37, 236, 69, 237, 38,
];

/// An encryption key and the helpers PBS derives from it.
#[derive(Clone)]
pub struct CryptConfig {
    enc_key: [u8; 32],
    id_key: [u8; 32],
}

/// Generate a fresh random 32-byte encryption key (CSPRNG via `getrandom`).
pub fn random_key() -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key)
        .map_err(|e| PbsError::Protocol(format!("failed to generate a key: {e}")))?;
    Ok(key)
}

impl CryptConfig {
    /// Build the config from a 32-byte encryption key.
    pub fn new(enc_key: [u8; 32]) -> Self {
        let mut id_key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(&enc_key, b"_id_key", 10, &mut id_key);
        Self { enc_key, id_key }
    }

    /// The raw 32-byte encryption key.
    pub fn key(&self) -> &[u8; 32] {
        &self.enc_key
    }

    /// The keyed chunk digest: `SHA-256(data ‖ id_key)`. Identical plaintext
    /// under the same key yields the same digest, so dedup is preserved.
    pub fn compute_digest(&self, data: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(data);
        // Appended last to avoid length-extension attacks (as PBS does).
        hasher.update(self.id_key);
        hasher.finalize().into()
    }

    /// The key fingerprint, for the user to identify which key a backup needs.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.compute_digest(&FINGERPRINT_INPUT)
    }

    /// Encrypt a chunk/blob payload into an encrypted `DataBlob`.
    pub fn encrypt_blob(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut iv = [0u8; 16];
        getrandom::getrandom(&mut iv)
            .map_err(|e| PbsError::Protocol(format!("failed to generate an IV: {e}")))?;

        let cipher = Aes256Gcm16::new_from_slice(&self.enc_key)
            .map_err(|_| PbsError::Protocol("invalid encryption key length".into()))?;
        let mut buffer = plaintext.to_vec();
        let tag = cipher
            .encrypt_in_place_detached(Nonce::<U16>::from_slice(&iv), b"", &mut buffer)
            .map_err(|_| PbsError::Protocol("encryption failed".into()))?;

        let mut out = Vec::with_capacity(ENC_HEADER_SIZE + buffer.len());
        out.extend_from_slice(&MAGIC_ENCRYPTED);
        out.extend_from_slice(&crc32(&buffer).to_le_bytes());
        out.extend_from_slice(&iv);
        out.extend_from_slice(tag.as_slice());
        out.extend_from_slice(&buffer);
        Ok(out)
    }

    /// Decrypt an encrypted `DataBlob` back to its plaintext payload.
    pub fn decrypt_blob(&self, blob: &[u8]) -> Result<Vec<u8>> {
        if blob.len() < ENC_HEADER_SIZE {
            return Err(PbsError::Protocol(
                "encrypted blob shorter than its header".into(),
            ));
        }
        let magic: [u8; 8] = blob[0..8].try_into().unwrap();
        if magic != MAGIC_ENCRYPTED {
            return Err(PbsError::Protocol(
                "blob is not an AES-256-GCM encrypted blob (compression not supported)".into(),
            ));
        }
        let crc_stored = u32::from_le_bytes(blob[8..12].try_into().unwrap());
        let iv: [u8; 16] = blob[12..28].try_into().unwrap();
        let tag: [u8; 16] = blob[28..44].try_into().unwrap();
        let ciphertext = &blob[ENC_HEADER_SIZE..];

        let crc_actual = crc32(ciphertext);
        if crc_actual != crc_stored {
            return Err(PbsError::Protocol(format!(
                "encrypted blob crc mismatch: stored {crc_stored:#010x}, computed {crc_actual:#010x}"
            )));
        }

        let cipher = Aes256Gcm16::new_from_slice(&self.enc_key)
            .map_err(|_| PbsError::Protocol("invalid encryption key length".into()))?;
        let mut buffer = ciphertext.to_vec();
        cipher
            .decrypt_in_place_detached(
                Nonce::<U16>::from_slice(&iv),
                b"",
                &mut buffer,
                Tag::<U16>::from_slice(&tag),
            )
            .map_err(|_| {
                PbsError::Protocol("decryption failed (wrong key or corrupt data)".into())
            })?;
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_round_trips() {
        let crypt = CryptConfig::new([7u8; 32]);
        let plaintext = b"the quick brown fox jumps over the lazy dog".repeat(40);
        let blob = crypt.encrypt_blob(&plaintext).unwrap();
        // Header present and the magic is the encrypted one.
        assert_eq!(&blob[0..8], &MAGIC_ENCRYPTED);
        assert_eq!(blob.len(), ENC_HEADER_SIZE + plaintext.len());
        assert_eq!(crypt.decrypt_blob(&blob).unwrap(), plaintext);
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let a = CryptConfig::new([1u8; 32]);
        let b = CryptConfig::new([2u8; 32]);
        let blob = a.encrypt_blob(b"secret").unwrap();
        assert!(b.decrypt_blob(&blob).is_err());
    }

    #[test]
    fn corruption_is_detected() {
        let crypt = CryptConfig::new([9u8; 32]);
        let mut blob = crypt.encrypt_blob(b"payload bytes").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(crypt.decrypt_blob(&blob).is_err());
    }

    #[test]
    fn digest_is_keyed_and_stable() {
        let a = CryptConfig::new([1u8; 32]);
        let b = CryptConfig::new([2u8; 32]);
        // Same key + same data => same digest (dedup works).
        assert_eq!(a.compute_digest(b"chunk"), a.compute_digest(b"chunk"));
        // Different key => different digest (no cross-key correlation).
        assert_ne!(a.compute_digest(b"chunk"), b.compute_digest(b"chunk"));
        // A keyed digest differs from the plain SHA-256.
        assert_ne!(
            a.compute_digest(b"chunk"),
            crate::index::chunk_digest(b"chunk")
        );
    }

    #[test]
    fn iv_is_random_per_blob() {
        let crypt = CryptConfig::new([3u8; 32]);
        let one = crypt.encrypt_blob(b"same").unwrap();
        let two = crypt.encrypt_blob(b"same").unwrap();
        // Distinct IVs => distinct ciphertext for identical plaintext.
        assert_ne!(one, two);
    }
}
