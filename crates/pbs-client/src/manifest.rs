//! Backup manifest (index.json).
//!
//! Every snapshot has a manifest, uploaded as an uncompressed DataBlob named
//! `index.json.blob`. It lists the archives in the snapshot with their size and
//! csum so the server and later restores can identify them.

use serde::{Deserialize, Serialize};

/// Name of the manifest blob in a snapshot.
pub const MANIFEST_BLOB_NAME: &str = "index.json.blob";

/// One archive within a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub filename: String,
    /// Encryption mode: "none" for an unencrypted backup.
    #[serde(rename = "crypt-mode")]
    pub crypt_mode: String,
    /// Total content size in bytes (the image size for a fixed index).
    pub size: u64,
    /// Lowercase hex of the archive's index csum (64 chars).
    pub csum: String,
}

impl FileEntry {
    /// An unencrypted archive entry (size and the archive's index csum).
    pub fn new(filename: impl Into<String>, size: u64, index_csum: &[u8; 32]) -> Self {
        Self {
            filename: filename.into(),
            crypt_mode: "none".to_string(),
            size,
            csum: hex::encode(index_csum),
        }
    }

    /// A fixed-index image entry for an unencrypted backup.
    pub fn fixed_image(filename: impl Into<String>, size: u64, index_csum: &[u8; 32]) -> Self {
        Self::new(filename, size, index_csum)
    }

    /// An archive entry, marking it encrypted when a key is in use.
    pub fn with_crypt(
        filename: impl Into<String>,
        size: u64,
        index_csum: &[u8; 32],
        encrypted: bool,
    ) -> Self {
        Self {
            filename: filename.into(),
            crypt_mode: if encrypted { "encrypt" } else { "none" }.to_string(),
            size,
            csum: hex::encode(index_csum),
        }
    }
}

/// A backup snapshot manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    #[serde(rename = "backup-type")]
    pub backup_type: String,
    #[serde(rename = "backup-id")]
    pub backup_id: String,
    #[serde(rename = "backup-time")]
    pub backup_time: i64,
    pub files: Vec<FileEntry>,
    /// Free-form data not covered by the signature. Always present (possibly {}).
    #[serde(default)]
    pub unprotected: serde_json::Value,
}

impl BackupManifest {
    /// Create a manifest for the given snapshot identity and files.
    pub fn new(
        backup_type: impl Into<String>,
        backup_id: impl Into<String>,
        backup_time: i64,
        files: Vec<FileEntry>,
    ) -> Self {
        Self {
            backup_type: backup_type.into(),
            backup_id: backup_id.into(),
            backup_time,
            files,
            unprotected: serde_json::json!({}),
        }
    }

    /// Serialize to the JSON bytes that go inside the manifest blob.
    pub fn to_json_bytes(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_with_kebab_case_keys() {
        let m = BackupManifest::new(
            "host",
            "myhost",
            1_700_000_000,
            vec![FileEntry::fixed_image("data.img.fidx", 4096, &[0xab; 32])],
        );
        let v: serde_json::Value = serde_json::from_slice(&m.to_json_bytes().unwrap()).unwrap();
        assert_eq!(v["backup-type"], "host");
        assert_eq!(v["backup-id"], "myhost");
        assert_eq!(v["backup-time"], 1_700_000_000);
        assert_eq!(v["files"][0]["filename"], "data.img.fidx");
        assert_eq!(v["files"][0]["crypt-mode"], "none");
        assert_eq!(v["files"][0]["size"], 4096);
        assert_eq!(v["files"][0]["csum"], hex::encode([0xab; 32]));
    }
}
