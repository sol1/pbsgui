//! Error types for the PBS client.

use thiserror::Error;

/// Convenience result type for the crate.
pub type Result<T> = std::result::Result<T, PbsError>;

/// Errors produced while talking to a Proxmox Backup Server.
#[derive(Debug, Error)]
pub enum PbsError {
    /// A repository string could not be parsed.
    #[error("invalid repository string: {0}")]
    InvalidRepository(String),

    /// Authentication with the server failed.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// The server certificate fingerprint did not match the pinned value.
    #[error("server fingerprint mismatch: expected {expected}, got {actual}")]
    FingerprintMismatch { expected: String, actual: String },

    /// The backup protocol was violated or the server returned an error.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// An underlying I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
