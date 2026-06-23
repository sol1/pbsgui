//! DataBlob encoding and decoding.
//!
//! A DataBlob is how PBS frames every chunk and stored object (the manifest, our
//! metadata). Layout of the variants this client uses:
//!
//! ```text
//! uncompressed:  magic[8] | crc32-le[4] | payload
//! zstd:          magic[8] | crc32-le[4] | zstd_frame(payload)
//! ```
//!
//! The CRC is CRC-32/IEEE (the zlib polynomial) computed over the payload bytes
//! only (the compressed bytes, for a zstd blob), not the header. Multi-byte
//! integers are little-endian. This module encodes and decodes the unencrypted
//! uncompressed and zstd variants; the AES-256-GCM encrypted variants (including
//! encrypted+zstd) live in [`crate::crypt`], which holds the key.

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

/// CRC-32/IEEE over the given bytes (the zlib polynomial).
pub(crate) fn crc32(payload: &[u8]) -> u32 {
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

/// Compress a payload into a standalone zstd frame. PBS compresses at a fast,
/// low-ratio setting (its level 1); `Fastest` is the closest match and the right
/// trade-off here, where the source is slow and CPU is plentiful. Infallible:
/// the encoder always produces a valid frame (raw blocks when incompressible).
pub(crate) fn zstd_compress(payload: &[u8]) -> Vec<u8> {
    use ruzstd::encoding::{compress_to_vec, CompressionLevel};
    compress_to_vec(payload, CompressionLevel::Fastest)
}

/// Decompress a standalone zstd frame back to its plaintext.
pub(crate) fn zstd_decompress(frame: &[u8]) -> Result<Vec<u8>> {
    use ruzstd::io::Read;
    let mut src = frame;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut src)
        .map_err(|e| PbsError::Protocol(format!("zstd frame error: {e}")))?;
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| PbsError::Protocol(format!("zstd decompression failed: {e}")))?;
    Ok(out)
}

/// Frame an already-compressed zstd payload as a zstd DataBlob
/// (`magic | crc | frame`). The CRC is over the compressed bytes, matching PBS.
fn frame_zstd(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_SIZE + frame.len());
    out.extend_from_slice(&MAGIC_ZSTD);
    out.extend_from_slice(&crc32(frame).to_le_bytes());
    out.extend_from_slice(frame);
    out
}

/// Encode a payload as a zstd-compressed DataBlob (no size check).
pub fn encode_compressed(payload: &[u8]) -> Vec<u8> {
    frame_zstd(&zstd_compress(payload))
}

/// Encode a payload, compressing it only when that actually shrinks it (PBS's
/// rule). Incompressible data - already-compressed media, an encrypted source, or
/// a deliberately high-entropy database - falls back to an uncompressed blob, so
/// compression never inflates a chunk.
pub fn encode_auto(payload: &[u8]) -> Vec<u8> {
    let compressed = zstd_compress(payload);
    if compressed.len() < payload.len() {
        frame_zstd(&compressed)
    } else {
        encode_uncompressed(payload)
    }
}

/// Decode a DataBlob and return its plaintext payload.
///
/// Handles the unencrypted uncompressed and zstd variants; the CRC is verified
/// first. Encrypted variants need a key and are decoded via [`crate::crypt`].
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
        MAGIC_ZSTD => zstd_decompress(payload),
        MAGIC_ENCRYPTED | MAGIC_ENCRYPTED_ZSTD => Err(PbsError::Protocol(
            "encrypted blob: a CryptConfig is required to decode it".into(),
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

    #[test]
    fn compressed_blob_round_trips() {
        // Highly compressible payload -> a zstd blob that decodes back exactly.
        let payload = b"the quick brown fox ".repeat(4096);
        let blob = encode_compressed(&payload);
        assert_eq!(&blob[0..8], &MAGIC_ZSTD);
        assert!(blob.len() < payload.len(), "expected the zstd blob to shrink");
        assert_eq!(decode(&blob).unwrap(), payload);
    }

    #[test]
    fn encode_auto_compresses_then_falls_back() {
        // Compressible data takes the zstd path.
        let text = b"AAAAAAAAAAAAAAAA".repeat(4096);
        let blob = encode_auto(&text);
        assert_eq!(&blob[0..8], &MAGIC_ZSTD);
        assert_eq!(decode(&blob).unwrap(), text);

        // Incompressible data (would not shrink) falls back to an uncompressed
        // blob rather than inflating the chunk. A simple LCG gives high-entropy
        // bytes without a rand dependency (as the chunker tests do).
        let mut state = 0x1234_5678u32;
        let incompressible: Vec<u8> = (0..64 * 1024)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        let blob = encode_auto(&incompressible);
        assert_eq!(&blob[0..8], &MAGIC_UNCOMPRESSED);
        assert_eq!(decode(&blob).unwrap(), incompressible);
    }
}
