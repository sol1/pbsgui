//! System State capture for Active Directory.
//!
//! Drives a VSS snapshot through the in-box writers (NTDS, DFS Replication,
//! Registry, System, COM+, WMI) via [`crate::vss`] and resolves the extracted
//! System State file set (`ntds.dit` + `edb*.log` + SYSVOL + registry hives +
//! OS/COM+ files) off the shadow copy. Capturing the raw files (rather than a
//! VHDX image or an IFM-defragmented copy) is what lets the ESE database dedup
//! well across daily backups and lets the browser (see [`crate::dit`]) read it
//! later. Streaming to PBS lands next (M3.2); today's dev command proves the
//! snapshot, the file resolution, and the writer completion on a real DC.
//! See `research/notes/10-active-directory-backup.md` for the mechanics and the
//! hard constraints (USN rollback, tombstone lifetime, DSRM).

/// Capture this Domain Controller's System State and report what a backup would
/// contain (dev entry point; the PBS upload is wired in the next milestone).
pub fn run_system_state_backup() -> anyhow::Result<()> {
    #[cfg(not(windows))]
    anyhow::bail!("Active Directory System State capture is only available on Windows");
    #[cfg(windows)]
    {
        // COM wants a plain thread it owns end to end, not a tokio worker.
        std::thread::spawn(smoke_test)
            .join()
            .map_err(|_| anyhow::anyhow!("the capture thread panicked"))?
    }
}

/// Snapshot, resolve, verify readability, and complete the session so the
/// writers (and therefore AD) record a real backup. Prints a per-writer summary;
/// `repadmin /showbackup` should show the run afterwards.
#[cfg(windows)]
fn smoke_test() -> anyhow::Result<()> {
    use crate::vss;

    vss::with_com(|| {
        let mut log = |line: String| tracing::info!("{line}");
        let capture = vss::SystemStateCapture::create(&mut log)?;

        let total: u64 = capture.files.iter().map(|f| f.size).sum();
        tracing::info!(
            "snapshot set {:?}: {} file(s), {} total",
            capture.snapshot_set,
            capture.files.len(),
            human_bytes(total)
        );
        for w in &capture.writers {
            tracing::info!(
                "  {}: {} component(s), {} file(s), {}",
                w.name,
                w.components,
                w.files,
                human_bytes(w.bytes)
            );
        }

        // Show how the archive will be laid out (M3.2 tars by these names).
        for f in capture.files.iter().take(5) {
            tracing::info!("  e.g. {} ({})", f.archive_name, human_bytes(f.size));
        }

        // Prove the frozen image is actually readable where it matters most:
        // the largest file (on a DC that is ntds.dit) must open and read.
        if let Some(biggest) = capture.files.iter().max_by_key(|f| f.size) {
            use std::io::Read;
            let mut head = [0u8; 64 * 1024];
            let mut file = std::fs::File::open(&biggest.shadow_path)?;
            let n = file.read(&mut head)?;
            tracing::info!(
                "read {} from the shadow copy of {} ({})",
                human_bytes(n as u64),
                biggest.original_path,
                human_bytes(biggest.size)
            );
        }

        let bcd_xml = capture.complete(true)?;
        tracing::info!(
            "backup completed and recorded by the writers (Backup Components Document: {}); \
             check `repadmin /showbackup` for the new backup time",
            human_bytes(bcd_xml.len() as u64)
        );
        Ok(())
    })
}

/// Format a byte count with a binary unit.
#[cfg(windows)]
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}
