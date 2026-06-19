//! Run a backup job: optional pre-script, change detection, archive the sources
//! and stream them to PBS deduplicated, then an optional post-script.

use pbs_client::session::{backup_dynamic_file_with_progress, BackupStats, SessionParams};
use pbs_client::Repository;
use pbsgui_ipc::{FileInfo, Job, Reply};
use tokio::sync::mpsc::Sender;

use crate::config::unix_now;
use crate::{archive, changedet, scripts};

/// Archive name for a job's filesystem backup (a tar in a dynamic index).
const ARCHIVE_NAME: &str = "files.didx";

/// Run a job, streaming `Reply::Log`/`Reply::Progress` to `events`. The post-job
/// script (if any) always runs with the final status in its environment.
pub async fn run_job(job: &Job, secret: &str, events: Sender<Reply>) -> anyhow::Result<String> {
    let outcome = run_inner(job, secret, &events).await;

    if let Some(post) = script_of(&job.post_script) {
        let mut env = base_env(job);
        match &outcome {
            Ok((message, stats)) => {
                env.push((
                    "PBSGUI_STATUS".into(),
                    (if stats.is_some() { "ok" } else { "no-change" }).into(),
                ));
                env.push(("PBSGUI_SUCCESS".into(), "1".into()));
                env.push(("PBSGUI_MESSAGE".into(), message.clone()));
                if let Some(stats) = stats {
                    push_stats_env(&mut env, stats);
                }
            }
            Err(e) => {
                env.push(("PBSGUI_STATUS".into(), "error".into()));
                env.push(("PBSGUI_SUCCESS".into(), "0".into()));
                env.push(("PBSGUI_MESSAGE".into(), e.to_string()));
            }
        }
        if let Err(e) = run_script(post, &env, "post", &events).await {
            let _ = events
                .send(Reply::Log {
                    line: format!("[post] failed to run: {e}"),
                })
                .await;
        }
    }

    outcome.map(|(message, _)| message)
}

async fn run_inner(
    job: &Job,
    secret: &str,
    events: &Sender<Reply>,
) -> anyhow::Result<(String, Option<BackupStats>)> {
    // Pre-job script: a non-zero exit aborts the job.
    if let Some(pre) = script_of(&job.pre_script) {
        let mut env = base_env(job);
        env.push(("PBSGUI_PHASE".into(), "pre".into()));
        let code = run_script(pre, &env, "pre", events).await?;
        if code != 0 {
            anyhow::bail!("pre-job script exited with code {code}");
        }
    }

    if job.sources.is_empty() {
        anyhow::bail!("job has no sources");
    }

    // Change detection: skip the run if nothing changed since last success.
    let fingerprint = if job.change_detection {
        Some(changedet::fingerprint(&job.sources, &job.excludes)?)
    } else {
        None
    };
    if let Some(fp) = &fingerprint {
        if changedet::load(&job.id).as_ref() == Some(fp) {
            let _ = events
                .send(Reply::Log {
                    line: "no source changes since last run; skipping backup".to_string(),
                })
                .await;
            return Ok(("no changes since last run; skipped".to_string(), None));
        }
    }

    let stats = do_backup(job, secret, events).await?;

    // Record the fingerprint only after a successful backup.
    if let Some(fp) = &fingerprint {
        let _ = changedet::save(&job.id, fp);
    }

    let summary = format!(
        "backed up {} bytes: {} chunks, {} uploaded, {} reused",
        stats.bytes, stats.chunks, stats.uploaded, stats.reused
    );
    let _ = events
        .send(Reply::Log {
            line: summary.clone(),
        })
        .await;
    Ok((summary, Some(stats)))
}

async fn do_backup(job: &Job, secret: &str, events: &Sender<Reply>) -> anyhow::Result<BackupStats> {
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
    let entries =
        tokio::task::spawn_blocking(move || archive::build_tar(&sources, &excludes, &tmp_path))
            .await
            .map_err(|e| anyhow::anyhow!("archive task failed: {e}"))??;

    // A catalog of the files so browsing does not need the whole archive.
    let catalog_files: Vec<FileInfo> = entries
        .into_iter()
        .map(|(path, size)| FileInfo { path, size })
        .collect();
    let catalog = serde_json::to_vec(&catalog_files)
        .map(|json| ("catalog.json.blob".to_string(), json))
        .ok();

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
    let result = backup_dynamic_file_with_progress(
        &params,
        ARCHIVE_NAME,
        &tmp,
        true,
        catalog,
        move |done, total| {
            let fraction = if total > 0 {
                done as f32 / total as f32
            } else {
                0.0
            };
            let _ = progress.try_send(Reply::Progress {
                fraction,
                message: format!("{done}/{total} bytes"),
            });
        },
    )
    .await;

    let _ = std::fs::remove_file(&tmp);
    Ok(result?)
}

fn script_of(script: &Option<String>) -> Option<&str> {
    script.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn base_env(job: &Job) -> Vec<(String, String)> {
    vec![
        ("PBSGUI_JOB_ID".into(), job.id.clone()),
        ("PBSGUI_JOB_NAME".into(), job.name.clone()),
        ("PBSGUI_BACKUP_ID".into(), job.destination.backup_id.clone()),
        (
            "PBSGUI_REPOSITORY".into(),
            job.destination.repository.clone(),
        ),
    ]
}

fn push_stats_env(env: &mut Vec<(String, String)>, stats: &BackupStats) {
    env.push(("PBSGUI_BYTES".into(), stats.bytes.to_string()));
    env.push(("PBSGUI_CHUNKS".into(), stats.chunks.to_string()));
    env.push(("PBSGUI_UPLOADED".into(), stats.uploaded.to_string()));
    env.push(("PBSGUI_REUSED".into(), stats.reused.to_string()));
}

async fn run_script(
    script: &str,
    env: &[(String, String)],
    label: &str,
    events: &Sender<Reply>,
) -> anyhow::Result<i32> {
    let _ = events
        .send(Reply::Log {
            line: format!("running {label}-job script"),
        })
        .await;
    let (code, output) = scripts::run(script, env).await?;
    for line in output.lines() {
        let _ = events
            .send(Reply::Log {
                line: format!("[{label}] {line}"),
            })
            .await;
    }
    Ok(code)
}
