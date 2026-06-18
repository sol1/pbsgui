//! Windows filesystem backup via VSS.
//!
//! For generic file and volume backups the engine takes a VSS snapshot of the
//! relevant volumes (so open files are captured consistently), walks the
//! snapshot, and streams the data to PBS. Windows file metadata (ACLs, alternate
//! data streams, attributes) is preserved in our own archive format rather than
//! pxar, which only models the Unix file model.

use crate::jobs::Target;

/// Back up the given filesystem target. Not yet implemented.
pub fn back_up(_target: &Target) -> anyhow::Result<()> {
    // TODO: create a VSS snapshot set, resolve shadow copy device paths,
    // walk the tree, and stream into a PBS dynamic index archive.
    anyhow::bail!("filesystem backup not yet implemented");
}
