//! Run a backup job: archive the sources, then stream them to PBS deduplicated.

use pbs_client::session::{backup_dynamic_file_with_progress, SessionParams};
use pbs_client::Repository;
use pbsgui_ipc::{Job, Reply};
use tokio::sync::mpsc::Sender;

use crate::archive;
use crate::config::unix_now;

/// Archive name for a job's filesystem backup (a tar in a dynamic index).
const ARCHIVE_NAME: &str = "files.didx";

/// Run a job's backup, streaming `Reply::Log`/`Reply::Progress` to `events`.
/// Returns a summary on success.
pub async fn run_job(job: &Job, secret: &str, events: Sender<Reply>) -> anyhow::Result<String> {
    if job.sources.is_empty() {
        anyhow::bail!("job has no sources");
    }

    let _ = events
        .send(Reply::Log {
            line: format!("archiving {} source(s)", job.sources.len()),
        })
        .await;

    // Archive the sources to a temp tar (blocking work off the async runtime).
    let tmp = std::env::temp_dir().join(format!("pbsgui-{}-{}.tar", job.id, unix_now()));
    let sources = job.sources.clone();
    let excludes = job.excludes.clone();
    let tmp_path = tmp.clone();
    tokio::task::spawn_blocking(move || archive::build_tar(&sources, &excludes, &tmp_path))
        .await
        .map_err(|e| anyhow::anyhow!("archive task failed: {e}"))??;

    let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    let _ = events
        .send(Reply::Log {
            line: format!("archive is {size} bytes; uploading to PBS (deduplicating)"),
        })
        .await;

    let repo: Repository = job.destination.repository.parse()?;
    let params = SessionParams::from_repository(
        &repo,
        secret,
        &job.destination.fingerprint,
        "host",
        &job.destination.backup_id,
        unix_now(),
    )?;

    let progress = events.clone();
    let result =
        backup_dynamic_file_with_progress(&params, ARCHIVE_NAME, &tmp, true, move |done, total| {
            let fraction = if total > 0 {
                done as f32 / total as f32
            } else {
                0.0
            };
            let _ = progress.try_send(Reply::Progress {
                fraction,
                message: format!("{done}/{total} bytes"),
            });
        })
        .await;

    let _ = std::fs::remove_file(&tmp);
    let stats = result?;

    let summary = format!(
        "backed up {} bytes: {} chunks, {} uploaded, {} reused",
        stats.bytes, stats.chunks, stats.uploaded, stats.reused
    );
    let _ = events
        .send(Reply::Log {
            line: summary.clone(),
        })
        .await;
    Ok(summary)
}
