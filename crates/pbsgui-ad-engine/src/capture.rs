//! System State capture for Active Directory.
//!
//! Drives a VSS snapshot through the in-box writers (NTDS, DFS Replication,
//! Registry, System, COM+) and streams the extracted System State file set
//! (`ntds.dit` + `edb*.log` + SYSVOL + registry hives + boot/COM+) to PBS as a
//! pxar dynamic index. Capturing the raw `ntds.dit` (rather than a VHDX image or
//! an IFM-defragmented copy) is what lets the ESE database dedup well across daily
//! backups and lets the browser (see [`crate::dit`]) read it later. Windows-only.
//! See `research/notes/10-active-directory-backup.md` for the mechanics and the
//! hard constraints (USN rollback, tombstone lifetime, DSRM).

/// Capture this Domain Controller's System State and stream it to PBS.
pub fn run_system_state_backup() -> anyhow::Result<()> {
    #[cfg(not(windows))]
    anyhow::bail!("Active Directory System State capture is only available on Windows");
    #[cfg(windows)]
    // TODO(M3): IVssBackupComponents: InitializeForBackup ->
    // SetBackupState(select components, bootableSystemState=true, VSS_BT_FULL) ->
    // GatherWriterMetadata -> select the NTDS/DFSR/Registry/System/COM+ writer
    // components -> StartSnapshotSet + AddToSnapshotSet(critical volumes) ->
    // DoSnapshotSet -> read the extracted files off \\?\GLOBALROOT\... and stream
    // them to PBS -> BackupComplete. Also read the NTDS writer's backup-expiration
    // metadata so a stale-at-restore backup can be flagged.
    anyhow::bail!("System State capture is not implemented yet");
}
