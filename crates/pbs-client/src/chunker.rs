//! Content-defined chunking (FastCDC).
//!
//! Chunk boundaries are chosen by content, not by fixed offsets, so an insertion
//! or deletion only changes the chunks around it - the rest reproduce identical
//! chunks (and identical SHA-256 digests) on the next backup. That shift
//! resistance is what makes incremental dedup effective: re-backing up a SQL
//! database that changed a little re-uploads only the changed chunks.

use std::io::Read;

use fastcdc::v2020::{FastCDC, StreamCDC};

/// Minimum chunk size (1 MiB).
pub const MIN_CHUNK: u32 = 1024 * 1024;
/// Target average chunk size (4 MiB).
pub const AVG_CHUNK: u32 = 4 * 1024 * 1024;
/// Maximum chunk size (8 MiB), kept under the 16 MiB PBS chunk cap.
pub const MAX_CHUNK: u32 = 8 * 1024 * 1024;

/// Split an in-memory buffer into content-defined chunks (zero-copy slices).
pub fn chunk_all(data: &[u8]) -> Vec<&[u8]> {
    chunk_all_with(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
}

/// Like [`chunk_all`] but with explicit sizes (used by tests).
pub fn chunk_all_with(data: &[u8], min: u32, avg: u32, max: u32) -> Vec<&[u8]> {
    FastCDC::new(data, min, avg, max)
        .map(|c| &data[c.offset..c.offset + c.length])
        .collect()
}

/// Stream content-defined chunks from a reader, yielding each chunk's bytes.
pub fn chunk_reader<R: Read>(reader: R) -> impl Iterator<Item = std::io::Result<Vec<u8>>> {
    StreamCDC::new(reader, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
        .map(|result| result.map(|cd| cd.data).map_err(std::io::Error::other))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    // Deterministic pseudo-random bytes (no rand dependency).
    fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
        let mut state = seed.wrapping_add(1);
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    fn digests(chunks: &[&[u8]]) -> Vec<[u8; 32]> {
        chunks.iter().map(|c| Sha256::digest(c).into()).collect()
    }

    #[test]
    fn chunking_is_deterministic() {
        let data = pseudo_random(12 * 1024 * 1024, 1);
        assert_eq!(digests(&chunk_all(&data)), digests(&chunk_all(&data)));
    }

    #[test]
    fn reassembles_to_original() {
        let data = pseudo_random(10 * 1024 * 1024, 2);
        let joined: Vec<u8> = chunk_all(&data).concat();
        assert_eq!(joined, data);
    }

    #[test]
    fn resists_shift_so_most_chunks_are_reused() {
        // Small sizes so a moderate buffer yields many chunks.
        let (min, avg, max) = (2048, 8192, 32768);
        let base = pseudo_random(2 * 1024 * 1024, 3);

        // Insert one byte near the front; a fixed-size chunker would change every
        // following chunk, a content-defined one re-syncs quickly.
        let mut shifted = base.clone();
        shifted.insert(1000, 0x42);

        let base_digests: std::collections::HashSet<_> =
            digests(&chunk_all_with(&base, min, avg, max))
                .into_iter()
                .collect();
        let shifted_digests = digests(&chunk_all_with(&shifted, min, avg, max));

        let reused = shifted_digests
            .iter()
            .filter(|d| base_digests.contains(*d))
            .count();
        let total = shifted_digests.len();
        assert!(total > 10, "expected many chunks, got {total}");
        // The overwhelming majority of chunks should be unchanged.
        assert!(
            reused * 10 >= total * 8,
            "expected >=80% chunk reuse after a 1-byte insert, got {reused}/{total}"
        );
    }
}
