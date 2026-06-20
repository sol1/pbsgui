//! Run a backup job. A job pairs a source (files or SQL Server databases) with
//! a destination (a PBS server or a folder); this dispatches the right backend,
//! resolving the saved connections and their secrets, and runs the optional
//! pre/post scripts around it.

use pbs_client::session::{backup_dynamic_file_with_progress, BackupStats, SessionParams};
use pbs_client::Repository;
use pbsgui_ipc::{FileInfo, Job, JobDestination, JobSource, Reply, SqlAuth, SqlBackupType};
use tokio::sync::mpsc::Sender;

use crate::config::unix_now;
use crate::{archive, changedet, connstore, scripts, secrets};

/// Archive name for a job's filesystem backup (a tar in a dynamic index).
const ARCHIVE_NAME: &str = "files.didx";

/// Run a job, streaming `Reply::Log`/`Reply::Progress` to `events`. The post-job
/// script (if any) always runs with the final status in its environment.
pub async fn run_job(job: &Job, events: Sender<Reply>) -> anyhow::Result<String> {
    let outcome = run_inner(job, &events).await;

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

    // Change detection (files source only): skip if nothing changed.
    let fingerprint = match &job.source {
        JobSource::Files {
            sources,
            excludes,
            change_detection: true,
        } => Some(changedet::fingerprint(sources, excludes)?),
        _ => None,
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

    let (summary, stats) = do_backup(job, events).await?;

    // Record the fingerprint only after a successful backup.
    if let Some(fp) = &fingerprint {
        let _ = changedet::save(&job.id, fp);
    }

    let _ = events
        .send(Reply::Log {
            line: summary.clone(),
        })
        .await;
    Ok((summary, stats))
}

async fn do_backup(
    job: &Job,
    events: &Sender<Reply>,
) -> anyhow::Result<(String, Option<BackupStats>)> {
    match (&job.source, &job.destination) {
        (
            JobSource::Files {
                sources, excludes, ..
            },
            JobDestination::Pbs {
                server_id,
                backup_id,
            },
        ) => {
            let stats =
                backup_files_to_pbs(job, sources, excludes, server_id, backup_id, events).await?;
            let summary = format!(
                "backed up {} bytes: {} chunks, {} uploaded, {} reused",
                stats.bytes, stats.chunks, stats.uploaded, stats.reused
            );
            Ok((summary, Some(stats)))
        }
        (
            JobSource::Sql {
                connection_id,
                databases,
                backup_type,
                ..
            },
            JobDestination::Pbs {
                server_id,
                backup_id,
            },
        ) => {
            backup_sql_to_pbs(
                connection_id,
                databases,
                *backup_type,
                server_id,
                backup_id,
                events,
            )
            .await
        }
        (
            JobSource::Sql {
                connection_id,
                databases,
                backup_type,
                ..
            },
            JobDestination::Folder { path },
        ) => backup_sql_to_folder(connection_id, databases, *backup_type, path, events).await,
        (JobSource::Files { .. }, JobDestination::Folder { .. }) => {
            anyhow::bail!("backing up files to a folder is not supported yet")
        }
    }
}

async fn backup_files_to_pbs(
    job: &Job,
    sources: &[String],
    excludes: &[String],
    server_id: &str,
    backup_id: &str,
    events: &Sender<Reply>,
) -> anyhow::Result<BackupStats> {
    if sources.is_empty() {
        anyhow::bail!("job has no sources");
    }
    let _ = events
        .send(Reply::Log {
            line: format!("archiving {} source(s)", sources.len()),
        })
        .await;

    // Archive the sources to a temp tar (blocking work off the async runtime).
    let tmp = std::env::temp_dir().join(format!("pbsgui-{}-{}.tar", job.id, unix_now()));
    let sources = sources.to_vec();
    let excludes = excludes.to_vec();
    let tmp_path = tmp.clone();
    let entries =
        tokio::task::spawn_blocking(move || archive::build_tar(&sources, &excludes, &tmp_path))
            .await
            .map_err(|e| anyhow::anyhow!("archive task failed: {e}"))??;

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

    let params = pbs_session_params(server_id, backup_id, "host", unix_now())?;
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

async fn backup_sql_to_pbs(
    connection_id: &str,
    databases: &[String],
    backup_type: SqlBackupType,
    server_id: &str,
    backup_id: &str,
    events: &Sender<Reply>,
) -> anyhow::Result<(String, Option<BackupStats>)> {
    require_full(backup_type)?;
    if databases.is_empty() {
        anyhow::bail!("no databases selected");
    }
    let (conn, password) = sql_conn_and_password(connection_id)?;

    let (mut chunks, mut uploaded, mut reused, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    for db in databases {
        let group = format!("{backup_id}-{}", sanitize(db));
        let archive = format!("{}.didx", sanitize(db));
        let params = pbs_session_params(server_id, &group, "mssql", unix_now())?;
        let _ = events
            .send(Reply::Log {
                line: format!("backing up [{db}] to PBS (group {group})"),
            })
            .await;
        let stats = crate::sql::vdi::backup_database_to_pbs(
            &conn.server,
            conn.port,
            &conn.auth,
            password.as_deref(),
            db,
            &params,
            &archive,
        )
        .await?;
        chunks += stats.chunks;
        uploaded += stats.uploaded;
        reused += stats.reused;
        bytes += stats.bytes;
    }

    let summary = format!(
        "backed up {} database(s) to PBS: {bytes} bytes, {chunks} chunks, {uploaded} uploaded, {reused} reused",
        databases.len()
    );
    Ok((summary, None))
}

async fn backup_sql_to_folder(
    connection_id: &str,
    databases: &[String],
    backup_type: SqlBackupType,
    path: &str,
    events: &Sender<Reply>,
) -> anyhow::Result<(String, Option<BackupStats>)> {
    require_full(backup_type)?;
    if databases.is_empty() {
        anyhow::bail!("no databases selected");
    }
    let (conn, password) = sql_conn_and_password(connection_id)?;

    let mut total: u64 = 0;
    for db in databases {
        let out = std::path::Path::new(path)
            .join(format!("{}-{}.bak", sanitize(db), unix_now()))
            .display()
            .to_string();
        let _ = events
            .send(Reply::Log {
                line: format!("backing up [{db}] to {out}"),
            })
            .await;
        total += crate::sql::vdi::backup_database_to_file(
            &conn.server,
            conn.port,
            &conn.auth,
            password.as_deref(),
            db,
            &out,
        )
        .await?;
    }

    let summary = format!(
        "backed up {} database(s) to {path}: {total} bytes",
        databases.len()
    );
    Ok((summary, None))
}

fn require_full(backup_type: SqlBackupType) -> anyhow::Result<()> {
    if backup_type != SqlBackupType::Full {
        anyhow::bail!("only full SQL Server backups are supported yet");
    }
    Ok(())
}

/// Resolve a saved PBS server into session parameters for one snapshot group.
fn pbs_session_params(
    server_id: &str,
    group: &str,
    backup_type: &str,
    backup_time: i64,
) -> anyhow::Result<SessionParams> {
    let server = connstore::pbs_servers()
        .get(server_id)
        .ok_or_else(|| anyhow::anyhow!("no such PBS server"))?;
    let secret = secrets::get(&connstore::pbs_secret_key(server_id))?
        .ok_or_else(|| anyhow::anyhow!("no saved secret for the PBS server"))?;
    let repo: Repository = server.repository.parse()?;
    Ok(SessionParams::from_repository(
        &repo,
        secret,
        &server.fingerprint,
        backup_type,
        group,
        backup_time,
    )?)
}

/// Resolve a saved SQL connection and its password (none for integrated auth).
fn sql_conn_and_password(
    connection_id: &str,
) -> anyhow::Result<(pbsgui_ipc::SqlConnection, Option<String>)> {
    let conn = connstore::sql_connections()
        .get(connection_id)
        .ok_or_else(|| anyhow::anyhow!("no such SQL connection"))?;
    let password = match conn.auth {
        SqlAuth::Integrated => None,
        _ => secrets::get(&connstore::sql_secret_key(connection_id))?,
    };
    Ok((conn, password))
}

/// PBS-safe slug for snapshot groups and archive names.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn script_of(script: &Option<String>) -> Option<&str> {
    script.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn base_env(job: &Job) -> Vec<(String, String)> {
    let mut env = vec![
        ("PBSGUI_JOB_ID".into(), job.id.clone()),
        ("PBSGUI_JOB_NAME".into(), job.name.clone()),
    ];
    match &job.destination {
        JobDestination::Pbs { backup_id, .. } => {
            env.push(("PBSGUI_DESTINATION".into(), "pbs".into()));
            env.push(("PBSGUI_BACKUP_ID".into(), backup_id.clone()));
        }
        JobDestination::Folder { path } => {
            env.push(("PBSGUI_DESTINATION".into(), "folder".into()));
            env.push(("PBSGUI_FOLDER".into(), path.clone()));
        }
    }
    env
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
