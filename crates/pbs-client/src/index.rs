//! Fixed index (.fidx) format.
//!
//! A fixed index describes an image split into equal-size chunks. On-disk and
//! on-wire layout (little-endian), header is exactly 4096 bytes:
//!
//! ```text
//! offset  size  field
//! 0       8     magic
//! 8       16    uuid
//! 24      8     ctime (i64, unix seconds)
//! 32      32    index csum (SHA-256 of the concatenated chunk digests)
//! 64      8     size (image bytes, u64)
//! 72      8     chunk size (u64, power of two)
//! 80      4016  reserved (zero)
//! 4096    ...   flat array of 32-byte SHA-256 chunk digests, in order
//! ```
//!
//! The index csum is a plain SHA-256 over the concatenation of the raw 32-byte
//! digests (no offsets, not keyed), and is the `csum` reported to `fixed_close`
//! and recorded in the manifest.

use sha2::{Digest, Sha256};

use crate::error::{PbsError, Result};

/// Fixed index magic.
pub const FIXED_MAGIC: [u8; 8] = [47, 127, 65, 237, 145, 253, 15, 205];
/// Fixed index header length.
pub const HEADER_LEN: usize = 4096;
/// SHA-256 digest length.
pub const DIGEST_LEN: usize = 32;
/// Default fixed chunk size used for images (4 MiB), as hardcoded by the server.
pub const DEFAULT_CHUNK_SIZE: u64 = 4096 * 1024;

/// SHA-256 of a chunk's plaintext bytes. This is the digest stored in the index
/// and sent on the wire (for the unencrypted case).
pub fn chunk_digest(data: &[u8]) -> [u8; DIGEST_LEN] {
    Sha256::digest(data).into()
}

/// Builds a fixed index from a sequence of chunk digests.
#[derive(Debug, Clone)]
pub struct FixedIndexBuilder {
    /// Total image size in bytes.
    pub size: u64,
    /// Chunk size in bytes.
    pub chunk_size: u64,
    /// Creation time, unix seconds.
    pub ctime: i64,
    /// Index uuid (raw 16 bytes).
    pub uuid: [u8; 16],
    digests: Vec<[u8; DIGEST_LEN]>,
}

impl FixedIndexBuilder {
    /// Create a builder for an image of `size` bytes split into `chunk_size`
    /// chunks.
    pub fn new(size: u64, chunk_size: u64, ctime: i64, uuid: [u8; 16]) -> Self {
        Self {
            size,
            chunk_size,
            ctime,
            uuid,
            digests: Vec::new(),
        }
    }

    /// Append the digest of the next chunk.
    pub fn push_digest(&mut self, digest: [u8; DIGEST_LEN]) {
        self.digests.push(digest);
    }

    /// Number of chunks pushed so far.
    pub fn chunk_count(&self) -> usize {
        self.digests.len()
    }

    /// The digests pushed so far.
    pub fn digests(&self) -> &[[u8; DIGEST_LEN]] {
        &self.digests
    }

    /// Expected number of chunks for the declared size and chunk size.
    pub fn expected_chunk_count(&self) -> u64 {
        self.size.div_ceil(self.chunk_size)
    }

    /// SHA-256 over the concatenation of all chunk digests.
    pub fn index_csum(&self) -> [u8; DIGEST_LEN] {
        let mut hasher = Sha256::new();
        for digest in &self.digests {
            hasher.update(digest);
        }
        hasher.finalize().into()
    }

    /// Serialize the full fixed index file (header + digest array).
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = vec![0u8; HEADER_LEN + self.digests.len() * DIGEST_LEN];
        out[0..8].copy_from_slice(&FIXED_MAGIC);
        out[8..24].copy_from_slice(&self.uuid);
        out[24..32].copy_from_slice(&self.ctime.to_le_bytes());
        out[32..64].copy_from_slice(&self.index_csum());
        out[64..72].copy_from_slice(&self.size.to_le_bytes());
        out[72..80].copy_from_slice(&self.chunk_size.to_le_bytes());
        // bytes [80..4096] stay zero (reserved)
        for (i, digest) in self.digests.iter().enumerate() {
            let start = HEADER_LEN + i * DIGEST_LEN;
            out[start..start + DIGEST_LEN].copy_from_slice(digest);
        }
        out
    }
}

/// A parsed fixed index file.
#[derive(Debug, Clone)]
pub struct FixedIndex {
    pub ctime: i64,
    pub uuid: [u8; 16],
    pub size: u64,
    pub chunk_size: u64,
    pub index_csum: [u8; DIGEST_LEN],
    pub digests: Vec<[u8; DIGEST_LEN]>,
}

impl FixedIndex {
    /// Parse a fixed index file (as returned by the reader `/download`).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(PbsError::Protocol(
                "fixed index shorter than its header".into(),
            ));
        }
        if bytes[0..8] != FIXED_MAGIC {
            return Err(PbsError::Protocol("not a fixed index (bad magic)".into()));
        }
        let uuid: [u8; 16] = bytes[8..24].try_into().unwrap();
        let ctime = i64::from_le_bytes(bytes[24..32].try_into().unwrap());
        let index_csum: [u8; DIGEST_LEN] = bytes[32..64].try_into().unwrap();
        let size = u64::from_le_bytes(bytes[64..72].try_into().unwrap());
        let chunk_size = u64::from_le_bytes(bytes[72..80].try_into().unwrap());

        let body = &bytes[HEADER_LEN..];
        if body.len() % DIGEST_LEN != 0 {
            return Err(PbsError::Protocol(
                "fixed index body is not a whole number of digests".into(),
            ));
        }
        let digests = body
            .chunks_exact(DIGEST_LEN)
            .map(|c| {
                let d: [u8; DIGEST_LEN] = c.try_into().unwrap();
                d
            })
            .collect();

        Ok(Self {
            ctime,
            uuid,
            size,
            chunk_size,
            index_csum,
            digests,
        })
    }

    /// Recompute the index csum from the digests and check it matches the header.
    pub fn verify_csum(&self) -> bool {
        let mut hasher = Sha256::new();
        for digest in &self.digests {
            hasher.update(digest);
        }
        let computed: [u8; DIGEST_LEN] = hasher.finalize().into();
        computed == self.index_csum
    }
}

/// Dynamic index magic.
pub const DYNAMIC_MAGIC: [u8; 8] = [28, 145, 78, 165, 25, 186, 179, 205];

/// On-disk size of one dynamic index entry: end-offset (u64) + digest.
const DYNAMIC_ENTRY_LEN: usize = 8 + DIGEST_LEN;

/// Builds a dynamic index from variable-size (content-defined) chunks.
///
/// Each entry records a chunk's cumulative end offset and its digest. The csum
/// is a SHA-256 over each (end-offset little-endian, digest) pair.
#[derive(Debug, Clone)]
pub struct DynamicIndexBuilder {
    pub ctime: i64,
    pub uuid: [u8; 16],
    entries: Vec<(u64, [u8; DIGEST_LEN])>,
}

impl DynamicIndexBuilder {
    pub fn new(ctime: i64, uuid: [u8; 16]) -> Self {
        Self {
            ctime,
            uuid,
            entries: Vec::new(),
        }
    }

    /// Append a chunk by its cumulative end offset and digest.
    pub fn push(&mut self, end_offset: u64, digest: [u8; DIGEST_LEN]) {
        self.entries.push((end_offset, digest));
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Total archive size: the last chunk's end offset.
    pub fn total_size(&self) -> u64 {
        self.entries.last().map(|(end, _)| *end).unwrap_or(0)
    }

    pub fn index_csum(&self) -> [u8; DIGEST_LEN] {
        let mut hasher = Sha256::new();
        for (end, digest) in &self.entries {
            hasher.update(end.to_le_bytes());
            hasher.update(digest);
        }
        hasher.finalize().into()
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = vec![0u8; HEADER_LEN + self.entries.len() * DYNAMIC_ENTRY_LEN];
        out[0..8].copy_from_slice(&DYNAMIC_MAGIC);
        out[8..24].copy_from_slice(&self.uuid);
        out[24..32].copy_from_slice(&self.ctime.to_le_bytes());
        out[32..64].copy_from_slice(&self.index_csum());
        let mut pos = HEADER_LEN;
        for (end, digest) in &self.entries {
            out[pos..pos + 8].copy_from_slice(&end.to_le_bytes());
            out[pos + 8..pos + DYNAMIC_ENTRY_LEN].copy_from_slice(digest);
            pos += DYNAMIC_ENTRY_LEN;
        }
        out
    }
}

/// A parsed dynamic index (as returned by the reader `/download`).
#[derive(Debug, Clone)]
pub struct DynamicIndex {
    pub ctime: i64,
    pub uuid: [u8; 16],
    pub index_csum: [u8; DIGEST_LEN],
    /// (cumulative end offset, digest) per chunk, in order.
    pub entries: Vec<(u64, [u8; DIGEST_LEN])>,
}

impl DynamicIndex {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(PbsError::Protocol(
                "dynamic index shorter than its header".into(),
            ));
        }
        if bytes[0..8] != DYNAMIC_MAGIC {
            return Err(PbsError::Protocol("not a dynamic index (bad magic)".into()));
        }
        let uuid: [u8; 16] = bytes[8..24].try_into().unwrap();
        let ctime = i64::from_le_bytes(bytes[24..32].try_into().unwrap());
        let index_csum: [u8; DIGEST_LEN] = bytes[32..64].try_into().unwrap();

        let body = &bytes[HEADER_LEN..];
        if body.len() % DYNAMIC_ENTRY_LEN != 0 {
            return Err(PbsError::Protocol(
                "dynamic index body is not a whole number of entries".into(),
            ));
        }
        let entries = body
            .chunks_exact(DYNAMIC_ENTRY_LEN)
            .map(|e| {
                let end = u64::from_le_bytes(e[0..8].try_into().unwrap());
                let digest: [u8; DIGEST_LEN] = e[8..DYNAMIC_ENTRY_LEN].try_into().unwrap();
                (end, digest)
            })
            .collect();
        Ok(Self {
            ctime,
            uuid,
            index_csum,
            entries,
        })
    }

    /// The chunk digests in order.
    pub fn digests(&self) -> impl Iterator<Item = &[u8; DIGEST_LEN]> {
        self.entries.iter().map(|(_, digest)| digest)
    }

    pub fn total_size(&self) -> u64 {
        self.entries.last().map(|(end, _)| *end).unwrap_or(0)
    }

    pub fn verify_csum(&self) -> bool {
        let mut hasher = Sha256::new();
        for (end, digest) in &self.entries {
            hasher.update(end.to_le_bytes());
            hasher.update(digest);
        }
        let computed: [u8; DIGEST_LEN] = hasher.finalize().into();
        computed == self.index_csum
    }
}

#[cfg(test)]
mod dynamic_tests {
    use super::*;

    #[test]
    fn build_serialize_parse_round_trip() {
        let mut b = DynamicIndexBuilder::new(1_700_000_000, [9u8; 16]);
        b.push(100, [1u8; DIGEST_LEN]);
        b.push(250, [2u8; DIGEST_LEN]);
        b.push(400, [3u8; DIGEST_LEN]);
        assert_eq!(b.entry_count(), 3);
        assert_eq!(b.total_size(), 400);

        let bytes = b.serialize();
        assert_eq!(bytes.len(), HEADER_LEN + 3 * DYNAMIC_ENTRY_LEN);

        let parsed = DynamicIndex::parse(&bytes).unwrap();
        assert_eq!(parsed.ctime, 1_700_000_000);
        assert_eq!(parsed.uuid, [9u8; 16]);
        assert_eq!(parsed.total_size(), 400);
        assert_eq!(parsed.entries, b.entries);
        assert_eq!(parsed.index_csum, b.index_csum());
        assert!(parsed.verify_csum());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[0] = 1;
        assert!(DynamicIndex::parse(&bytes).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(b: &[u8]) -> [u8; DIGEST_LEN] {
        chunk_digest(b)
    }

    #[test]
    fn build_serialize_parse_round_trip() {
        let mut b = FixedIndexBuilder::new(10, 4, 1_700_000_000, [7u8; 16]);
        b.push_digest(digest_of(b"aaaa"));
        b.push_digest(digest_of(b"bbbb"));
        b.push_digest(digest_of(b"cc"));
        assert_eq!(b.chunk_count(), 3);
        assert_eq!(b.expected_chunk_count(), 3); // ceil(10/4)

        let bytes = b.serialize();
        assert_eq!(bytes.len(), HEADER_LEN + 3 * DIGEST_LEN);

        let parsed = FixedIndex::parse(&bytes).unwrap();
        assert_eq!(parsed.ctime, 1_700_000_000);
        assert_eq!(parsed.uuid, [7u8; 16]);
        assert_eq!(parsed.size, 10);
        assert_eq!(parsed.chunk_size, 4);
        assert_eq!(parsed.digests, b.digests());
        assert_eq!(parsed.index_csum, b.index_csum());
        assert!(parsed.verify_csum());
    }

    #[test]
    fn index_csum_is_sha256_of_concatenated_digests() {
        let mut b = FixedIndexBuilder::new(8, 4, 0, [0u8; 16]);
        let d0 = digest_of(b"0000");
        let d1 = digest_of(b"1111");
        b.push_digest(d0);
        b.push_digest(d1);

        let mut expected = Sha256::new();
        expected.update(d0);
        expected.update(d1);
        let expected: [u8; DIGEST_LEN] = expected.finalize().into();
        assert_eq!(b.index_csum(), expected);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[0] = 1;
        assert!(FixedIndex::parse(&bytes).is_err());
    }
}
