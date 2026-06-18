//! Bridge between the engine and the clean-room [`pbs_client`] protocol crate.
//!
//! This module turns engine level concepts (a backup job producing a byte
//! stream plus metadata) into PBS protocol operations: open a backup session,
//! upload the stream as a fixed index image, attach a metadata blob, and finish
//! the snapshot. One PBS snapshot is created per SQL backup operation.

use pbs_client::Repository;

/// Connection settings for a PBS datastore.
#[derive(Debug, Clone)]
pub struct PbsConnection {
    pub repository: Repository,
    /// Expected server certificate SHA-256 fingerprint (colon separated hex).
    pub fingerprint: Option<String>,
}

// TODO: open_backup_session(conn) -> Session
// TODO: upload_fixed_index_image(session, name, reader)
// TODO: put_blob(session, name, bytes)  // SQL metadata: LSNs, type, identity
// TODO: finish_snapshot(session)
