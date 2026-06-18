//! IPC request handler: job CRUD and runs, backed by the shared job store.

use std::sync::Arc;

use pbsgui_ipc::{Reply, Request, Responder};
use tokio::sync::mpsc;

use crate::config::unix_now;
use crate::jobstore::JobStore;
use crate::{backup, secrets};

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
    }
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
