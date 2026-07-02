//! System State capture for Active Directory.
//!
//! Drives a VSS snapshot through the in-box writers (NTDS, DFS Replication,
//! Registry, System, COM+, WMI) via [`crate::vss`], then streams the extracted
//! System State file set (`ntds.dit` + `edb*.log` + SYSVOL + registry hives +
//! OS/COM+ files) straight off the shadow copy into PBS as one deduplicated tar
//! archive, with bounded memory and no disk staging. Capturing the raw files
//! (rather than a VHDX image or an IFM-defragmented copy) is what lets the ESE
//! database dedup well across daily backups and lets the browser (see
//! [`crate::dit`]) read it later.
//!
//! The VSS writers are told the outcome truthfully: they record a backup only
//! after PBS has committed the snapshot, and a failed or aborted upload reports
//! failure, so AD's backup timestamp (`repadmin /showbackup`) never claims a
//! backup that is not actually on the server. See
//! `research/notes/10-active-directory-backup.md` for the mechanics and the
//! hard constraints (USN rollback, tombstone lifetime, DSRM).

/// Options for a System State backup run (the dev CLI surface until jobs land).
/// Only the Windows implementation reads the fields; other targets bail first.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct BackupOptions {
    /// PBS repository, `user@realm!token@host:datastore`. Without it the run is
    /// a capture-only smoke test: snapshot, resolve, verify, complete.
    pub repository: Option<String>,
    /// PBS server certificate fingerprint (SHA-256, colons optional).
    pub fingerprint: String,
    /// PBS namespace, if any.
    pub namespace: Option<String>,
    /// Backup group id; defaults to `<hostname>-ad`.
    pub backup_id: Option<String>,
    /// Compress chunks with zstd.
    pub compress: bool,
}

/// Archive name of the System State tar inside a snapshot.
#[cfg(windows)]
const ARCHIVE_NAME: &str = "systemstate.didx";
/// Blob holding the backup's metadata: the file catalog and the VSS Backup
/// Components Document (which a restore-time requester initializes from).
#[cfg(windows)]
const META_BLOB_NAME: &str = "ad-meta.json.blob";

/// Capture this Domain Controller's System State and, when a repository is
/// given, stream it to PBS (dev entry point).
pub fn run_system_state_backup(options: BackupOptions) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        let _ = options;
        anyhow::bail!("Active Directory System State capture is only available on Windows");
    }
    #[cfg(windows)]
    {
        // COM wants a plain thread it owns end to end, not a tokio worker. The
        // runtime handle lets that thread drive the async PBS upload.
        let runtime = tokio::runtime::Handle::current();
        std::thread::spawn(move || windows_impl::run(options, runtime))
            .join()
            .map_err(|_| anyhow::anyhow!("the capture thread panicked"))?
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::io::Write as _;

    use pbs_client::session::SessionParams;
    use pbs_client::{backup_dynamic_reader, Repository};
    use serde::Serialize;

    use super::{BackupOptions, ARCHIVE_NAME, META_BLOB_NAME};
    use crate::stream::{ChannelReader, ChannelWriter};
    use crate::vss;

    /// The `ad-meta.json.blob` contents: enough to list the backup, drive a
    /// restore, and browse without downloading the archive.
    #[derive(Serialize)]
    struct AdMeta {
        host: String,
        captured_unix: i64,
        writers: Vec<MetaWriter>,
        files: Vec<MetaFile>,
        /// The VSS Backup Components Document (post-snapshot, so it carries the
        /// writers' backup metadata such as the NTDS expiration time).
        vss_backup_components_xml: String,
    }

    #[derive(Serialize)]
    struct MetaWriter {
        name: String,
        components: usize,
        files: usize,
        bytes: u64,
    }

    #[derive(Serialize)]
    struct MetaFile {
        /// Path inside the tar (original path, drive colon dropped).
        name: String,
        size: u64,
    }

    pub fn run(options: BackupOptions, runtime: tokio::runtime::Handle) -> anyhow::Result<()> {
        vss::with_com(|| {
            let mut log = |line: String| tracing::info!("{line}");
            let capture = vss::SystemStateCapture::create(&mut log)?;
            summarize(&capture);

            let Some(repo) = options.repository.as_deref() else {
                return smoke_test(capture);
            };

            let total: u64 = capture.files.iter().map(|f| f.size).sum();
            let repo: Repository = repo.parse()?;
            let secret = std::env::var("PBSGUI_AD_PBS_SECRET")
                .or_else(|_| std::env::var("PBS_PASSWORD"))
                .map_err(|_| {
                    anyhow::anyhow!(
                        "set PBSGUI_AD_PBS_SECRET (or PBS_PASSWORD) to the PBS API token secret"
                    )
                })?;
            let backup_id = options
                .backup_id
                .clone()
                .unwrap_or_else(|| format!("{}-ad", hostname().to_lowercase()));
            let mut params = SessionParams::from_repository(
                &repo,
                secret,
                &options.fingerprint,
                "host",
                &backup_id,
                pbsgui_core::config::unix_now(),
            )?;
            params.namespace = options.namespace.clone();

            // The metadata blob: catalog + the post-snapshot components document.
            let meta = AdMeta {
                host: hostname(),
                captured_unix: pbsgui_core::config::unix_now(),
                writers: capture
                    .writers
                    .iter()
                    .map(|w| MetaWriter {
                        name: w.name.clone(),
                        components: w.components,
                        files: w.files,
                        bytes: w.bytes,
                    })
                    .collect(),
                files: capture
                    .files
                    .iter()
                    .map(|f| MetaFile {
                        name: f.archive_name.clone(),
                        size: f.size,
                    })
                    .collect(),
                vss_backup_components_xml: capture.components_xml()?,
            };
            let meta_json = serde_json::to_vec(&meta)?;
            let (meta_tx, meta_rx) = tokio::sync::oneshot::channel();
            let _ = meta_tx.send((META_BLOB_NAME.to_string(), meta_json));

            // Tar the shadow-copy files on a producer thread; the uploader
            // consumes the stream with bounded memory. The reader treats EOF as
            // valid only after a success verdict, so a mid-stream tar failure
            // fails the upload instead of committing a truncated archive.
            let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
            let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();
            let entries: Vec<(std::path::PathBuf, String)> = capture
                .files
                .iter()
                .map(|f| (f.shadow_path.clone(), f.archive_name.clone()))
                .collect();
            let tar_thread = std::thread::spawn(move || {
                let result = (|| -> anyhow::Result<()> {
                    let mut builder = tar::Builder::new(ChannelWriter::new(tx));
                    for (path, name) in &entries {
                        builder
                            .append_path_with_name(path, name)
                            .map_err(|e| anyhow::anyhow!("archiving {name}: {e}"))?;
                    }
                    builder.into_inner()?.flush()?;
                    Ok(())
                })();
                let _ = done_tx.send(result.is_ok());
                result
            });

            tracing::info!(
                "uploading the System State to {}:{} as {backup_id}/{ARCHIVE_NAME} \
                 ({} across {} files)",
                repo.host.as_deref().unwrap_or("localhost"),
                repo.datastore,
                human_bytes(total),
                capture.files.len(),
            );
            let mut last_pct = 0u64;
            let upload = runtime.block_on(backup_dynamic_reader(
                &params,
                ARCHIVE_NAME,
                true,
                options.compress,
                ChannelReader::new(rx, done_rx),
                total,
                Some(meta_rx),
                None, // encryption arrives with jobs (M4)
                |p| {
                    let pct = (p.bytes_done * 100).checked_div(p.total_bytes).unwrap_or(0);
                    if pct >= last_pct + 10 {
                        last_pct = pct;
                        tracing::info!(
                            "  {pct}% ({} read, {} uploaded, {} reused chunks)",
                            human_bytes(p.bytes_done),
                            human_bytes(p.uploaded_bytes),
                            p.reused
                        );
                    }
                },
            ));
            let tar_result = tar_thread
                .join()
                .map_err(|_| anyhow::anyhow!("the archive thread panicked"))?;

            match (&tar_result, &upload) {
                (Ok(()), Ok(stats)) => {
                    // Only now, with the snapshot committed on PBS, do the
                    // writers record a successful backup.
                    let _ = capture.complete(true)?;
                    let dedup = if stats.chunks > 0 {
                        stats.reused as f64 / stats.chunks as f64 * 100.0
                    } else {
                        0.0
                    };
                    tracing::info!(
                        "backup complete: {} backed up, {} stored ({} chunks, {:.0}% dedup); \
                         AD recorded the backup (check `repadmin /showbackup`)",
                        human_bytes(stats.bytes),
                        human_bytes(stats.stored_bytes),
                        stats.chunks,
                        dedup
                    );
                    Ok(())
                }
                _ => {
                    let _ = capture.complete(false);
                    if let Err(e) = tar_result {
                        anyhow::bail!("building the archive failed: {e:#}");
                    }
                    upload
                        .map(|_| ())
                        .map_err(|e| anyhow::anyhow!("PBS upload failed: {e:#}"))
                }
            }
        })
    }

    /// Capture-only run: verify readability and complete, uploading nothing.
    fn smoke_test(capture: vss::SystemStateCapture) -> anyhow::Result<()> {
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
            "capture-only run completed and recorded by the writers (Backup Components \
             Document: {}); pass --repo to upload. Check `repadmin /showbackup`.",
            human_bytes(bcd_xml.len() as u64)
        );
        Ok(())
    }

    fn summarize(capture: &vss::SystemStateCapture) {
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
        for f in capture.files.iter().take(5) {
            tracing::info!("  e.g. {} ({})", f.archive_name, human_bytes(f.size));
        }
    }

    fn hostname() -> String {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "dc".to_string())
    }

    /// Format a byte count with a binary unit.
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
}
