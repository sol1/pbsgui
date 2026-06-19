//! IPC request handler: job CRUD, runs, browse, and restore.

use std::collections::HashSet;
use std::sync::Arc;

use pbs_client::api::ApiClient;
use pbs_client::session::{ReaderClient, SessionParams};
use pbs_client::Repository;
use pbsgui_ipc::{FileInfo, Job, Reply, Request, Responder, SnapshotInfo};
use tokio::sync::mpsc;

use crate::config::unix_now;
use crate::jobstore::JobStore;
use crate::{backup, restore, secrets};

/// Archive name for a job's filesystem backup.
const ARCHIVE_NAME: &str = "files.didx";
/// Backup type used for all backups for now.
const BACKUP_TYPE: &str = "host";

/// Handle one IPC request against the shared job store.
pub async fn handle(store: Arc<JobStore>, request: Request, mut responder: Responder) {
    match request {
        Request::Ping => {
            let _ = responder.send(&Reply::Pong).await;
        }

        Request::ListJobs => {
            let _ = responder.send(&Reply::Jobs { jobs: store.list() }).await;
        }

        Request::SaveJob { job, secret } => {
            let id = job.id.clone();
            let result = (|| -> anyhow::Result<()> {
                if let Some(secret) = secret {
                    secrets::set(&id, &secret)?;
                }
                store.save_job(job)
            })();
            let reply = match result {
                Ok(()) => Reply::Saved { id },
                Err(e) => Reply::Error {
                    message: e.to_string(),
                },
            };
            let _ = responder.send(&reply).await;
        }

        Request::DeleteJob { id } => {
            let _ = secrets::delete(&id);
            let reply = match store.delete(&id) {
                Ok(()) => Reply::Deleted,
                Err(e) => Reply::Error {
                    message: e.to_string(),
                },
            };
            let _ = responder.send(&reply).await;
        }

        Request::RunJob { id } => {
            run_job(store, id, responder).await;
        }

        Request::ListSnapshots { job_id } => {
            let reply = match list_snapshots(&store, &job_id).await {
                Ok(snapshots) => Reply::Snapshots { snapshots },
                Err(e) => Reply::Error {
                    message: e.to_string(),
                },
            };
            let _ = responder.send(&reply).await;
        }

        Request::ListFiles {
            job_id,
            backup_time,
        } => {
            let reply = match list_files(&store, &job_id, backup_time).await {
                Ok(files) => Reply::Files { files },
                Err(e) => Reply::Error {
                    message: e.to_string(),
                },
            };
            let _ = responder.send(&reply).await;
        }

        Request::Restore {
            job_id,
            backup_time,
            files,
            destination,
        } => {
            restore_job(store, job_id, backup_time, files, destination, responder).await;
        }
    }
}

/// Resolve a job, its secret, and its parsed repository.
fn job_context(store: &JobStore, job_id: &str) -> anyhow::Result<(Job, String, Repository)> {
    let job = store
        .get(job_id)
        .ok_or_else(|| anyhow::anyhow!("no such job: {job_id}"))?;
    let secret =
        secrets::get(job_id)?.ok_or_else(|| anyhow::anyhow!("no saved credential for this job"))?;
    let repo: Repository = job.destination.repository.parse()?;
    Ok((job, secret, repo))
}

async fn list_snapshots(store: &JobStore, job_id: &str) -> anyhow::Result<Vec<SnapshotInfo>> {
    let (job, secret, repo) = job_context(store, job_id)?;
    let api = ApiClient::from_repository(&repo, secret, &job.destination.fingerprint)?;
    let snapshots = api
        .list_snapshots(&repo.datastore, BACKUP_TYPE, &job.destination.backup_id)
        .await?;
    Ok(snapshots
        .into_iter()
        .map(|s| SnapshotInfo {
            backup_time: s.backup_time,
            size: s.size,
        })
        .collect())
}

async fn list_files(
    store: &JobStore,
    job_id: &str,
    backup_time: i64,
) -> anyhow::Result<Vec<FileInfo>> {
    let (job, secret, repo) = job_context(store, job_id)?;
    let params = SessionParams::from_repository(
        &repo,
        secret,
        &job.destination.fingerprint,
        BACKUP_TYPE,
        &job.destination.backup_id,
        backup_time,
    )?;
    let mut reader = ReaderClient::connect(&params).await?;
    let bytes = reader.restore_dynamic_archive(ARCHIVE_NAME).await?;
    restore::list_tar(&bytes)
}

async fn restore_job(
    store: Arc<JobStore>,
    job_id: String,
    backup_time: i64,
    files: Option<Vec<String>>,
    destination: String,
    mut responder: Responder,
) {
    let (job, secret, repo) = match job_context(&store, &job_id) {
        Ok(ctx) => ctx,
        Err(e) => {
            let _ = responder
                .send(&Reply::Error {
                    message: e.to_string(),
                })
                .await;
            return;
        }
    };

    let _ = responder
        .send(&Reply::Accepted {
            job_id: job_id.clone(),
        })
        .await;

    let result = run_restore(
        &job,
        &secret,
        &repo,
        backup_time,
        files,
        &destination,
        &mut responder,
    )
    .await;
    let reply = match result {
        Ok(message) => Reply::Finished {
            success: true,
            message,
        },
        Err(e) => Reply::Finished {
            success: false,
            message: e.to_string(),
        },
    };
    let _ = responder.send(&reply).await;
}

#[allow(clippy::too_many_arguments)]
async fn run_restore(
    job: &Job,
    secret: &str,
    repo: &Repository,
    backup_time: i64,
    files: Option<Vec<String>>,
    destination: &str,
    responder: &mut Responder,
) -> anyhow::Result<String> {
    let params = SessionParams::from_repository(
        repo,
        secret,
        &job.destination.fingerprint,
        BACKUP_TYPE,
        &job.destination.backup_id,
        backup_time,
    )?;

    let _ = responder
        .send(&Reply::Log {
            line: "connecting to PBS".to_string(),
        })
        .await;
    let mut reader = ReaderClient::connect(&params).await?;

    let _ = responder
        .send(&Reply::Log {
            line: "downloading archive".to_string(),
        })
        .await;
    let bytes = reader.restore_dynamic_archive(ARCHIVE_NAME).await?;
    let _ = responder
        .send(&Reply::Progress {
            fraction: 0.5,
            message: format!("downloaded {} bytes", bytes.len()),
        })
        .await;

    let selected: Option<HashSet<String>> = files.map(|v| v.into_iter().collect());
    let dest = std::path::PathBuf::from(destination);
    let _ = responder
        .send(&Reply::Log {
            line: format!("extracting to {}", dest.display()),
        })
        .await;

    let count =
        tokio::task::spawn_blocking(move || restore::extract(&bytes, selected.as_ref(), &dest))
            .await
            .map_err(|e| anyhow::anyhow!("extract task failed: {e}"))??;

    let _ = responder
        .send(&Reply::Progress {
            fraction: 1.0,
            message: "done".to_string(),
        })
        .await;
    Ok(format!("restored {count} file(s)"))
}

async fn run_job(store: Arc<JobStore>, id: String, mut responder: Responder) {
    let Some(job) = store.get(&id) else {
        let _ = responder
            .send(&Reply::Error {
                message: format!("no such job: {id}"),
            })
            .await;
        return;
    };
    let secret = match secrets::get(&id) {
        Ok(Some(secret)) => secret,
        Ok(None) => {
            let _ = responder
                .send(&Reply::Error {
                    message: "no saved credential for this job".to_string(),
                })
                .await;
            return;
        }
        Err(e) => {
            let _ = responder
                .send(&Reply::Error {
                    message: e.to_string(),
                })
                .await;
            return;
        }
    };

    let _ = responder
        .send(&Reply::Accepted { job_id: id.clone() })
        .await;

    let (tx, mut rx) = mpsc::channel::<Reply>(64);
    let job_for_run = job.clone();
    let run = tokio::spawn(async move { backup::run_job(&job_for_run, &secret, tx).await });

    while let Some(reply) = rx.recv().await {
        if responder.send(&reply).await.is_err() {
            break;
        }
    }

    let (success, message) = match run.await {
        Ok(Ok(summary)) => (true, summary),
        Ok(Err(e)) => (false, e.to_string()),
        Err(e) => (false, format!("job task failed: {e}")),
    };
    let status = if success {
        "ok".to_string()
    } else {
        message.clone()
    };
    let _ = store.record_run(&id, unix_now(), status);
    let _ = responder.send(&Reply::Finished { success, message }).await;
}
