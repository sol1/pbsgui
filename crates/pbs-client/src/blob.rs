//! DataBlob encoding and decoding.
//!
//! A DataBlob is how PBS frames every chunk and stored object (the manifest, our
//! metadata). Layout of the variants this client uses:
//!
//! ```text
//! uncompressed:  magic[8] | crc32-le[4] | payload
//! ```
//!
//! The CRC is CRC-32/IEEE (the zlib polynomial) computed over the payload bytes
//! only, not the header. Multi-byte integers are little-endian. This client only
//! writes uncompressed blobs, so it only needs to decode those (plus a clear
//! error for the compressed and encrypted variants, which arrive once those
//! features are implemented).

use crate::error::{PbsError, Result};

/// Magic for an unencrypted, uncompressed blob.
pub const MAGIC_UNCOMPRESSED: [u8; 8] = [66, 171, 56, 7, 190, 131, 112, 161];
/// Magic for a zstd-compressed blob.
pub const MAGIC_ZSTD: [u8; 8] = [49, 185, 88, 66, 111, 182, 163, 127];
/// Magic for an AES-256-GCM encrypted blob.
pub const MAGIC_ENCRYPTED: [u8; 8] = [123, 103, 133, 190, 34, 45, 76, 240];
/// Magic for an encrypted and compressed blob.
pub const MAGIC_ENCRYPTED_ZSTD: [u8; 8] = [230, 89, 27, 191, 11, 191, 216, 11];

/// Size of the unencrypted blob header (magic + crc).
const HEADER_SIZE: usize = 12;

/// Maximum blob payload size accepted by PBS.
pub const MAX_BLOB_SIZE: usize = 128 * 1024 * 1024;

fn crc32(payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(payload);
    hasher.finalize()
}

/// Encode a payload as an unencrypted, uncompressed DataBlob.
pub fn encode_uncompressed(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    out.extend_from_slice(&MAGIC_UNCOMPRESSED);
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Decode a DataBlob and return its plaintext payload.
///
/// Only unencrypted, uncompressed blobs are supported for now (what this client
/// writes); the CRC is verified.
pub fn decode(blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < HEADER_SIZE {
        return Err(PbsError::Protocol(format!(
            "blob is shorter than the {HEADER_SIZE} byte header"
        )));
    }
    let magic: [u8; 8] = blob[0..8].try_into().unwrap();
    let crc_stored = u32::from_le_bytes(blob[8..12].try_into().unwrap());
    let payload = &blob[HEADER_SIZE..];

    let crc_actual = crc32(payload);
    if crc_actual != crc_stored {
        return Err(PbsError::Protocol(format!(
            "blob crc mismatch: stored {crc_stored:#010x}, computed {crc_actual:#010x}"
        )));
    }

    match magic {
        MAGIC_UNCOMPRESSED => Ok(payload.to_vec()),
        MAGIC_ZSTD => Err(PbsError::Protocol(
            "zstd-compressed blob: decoding not implemented yet".into(),
        )),
        MAGIC_ENCRYPTED | MAGIC_ENCRYPTED_ZSTD => Err(PbsError::Protocol(
            "encrypted blob: decoding not implemented yet".into(),
        )),
        other => Err(PbsError::Protocol(format!("unknown blob magic {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let payload = b"the quick brown fox".to_vec();
        let blob = encode_uncompressed(&payload);
        assert_eq!(decode(&blob).unwrap(), payload);
    }

    #[test]
    fn empty_payload_round_trips() {
        let blob = encode_uncompressed(b"");
        assert_eq!(blob.len(), HEADER_SIZE);
        assert_eq!(decode(&blob).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn known_layout_for_hello() {
        // CRC-32/IEEE of "hello" is 0x3610a686. This pins the exact byte layout
        // independently of the implementation.
        let blob = encode_uncompressed(b"hello");
        assert_eq!(&blob[0..8], &MAGIC_UNCOMPRESSED);
        assert_eq!(&blob[8..12], &0x3610a686u32.to_le_bytes());
        assert_eq!(&blob[12..], b"hello");
    }

    #[test]
    fn detects_corruption() {
        let mut blob = encode_uncompressed(b"payload");
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(decode(&blob).is_err());
    }

    #[test]
    fn rejects_short_blob() {
        assert!(decode(&[0u8; 4]).is_err());
    }
}
