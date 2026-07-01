//! Offline reader for a backed-up `ntds.dit` (the AD ESE database).
//!
//! Opens the database read-only via the ESE API (`esent.dll`), running log
//! recovery if needed, resolves the database's own schema so the cryptic
//! `ATTm*/ATTk*` columns become named attributes, and walks `datatable` +
//! `link_table` + `sd_table` to reconstruct the directory (DN) tree. This is the
//! foundation of the browser: diffing a restore point against live AD and against
//! other restore points, and driving partial restores over LDAP. Windows-only
//! (`esent.dll`). See M6.

/// Browse a backed-up `ntds.dit` (dev entry point).
pub fn browse() -> anyhow::Result<()> {
    #[cfg(not(windows))]
    anyhow::bail!("browsing a backed-up ntds.dit requires the Windows ESE API (esent.dll)");
    #[cfg(windows)]
    // TODO(M6): JetInit/JetAttachDatabase(read-only) -> resolve the schema NC ->
    // enumerate datatable rows -> map attribute columns via the schema ->
    // reconstruct DNs from the parent/RDN chain -> expose a tree for diff/restore.
    anyhow::bail!("the ntds.dit browser is not implemented yet");
}
