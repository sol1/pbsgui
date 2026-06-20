//! IPC request handler: job CRUD, runs, browse, and restore.

use std::collections::HashSet;
use std::sync::Arc;

use pbs_client::api::ApiClient;
use pbs_client::session::{ReaderClient, SessionParams};
use pbs_client::Repository;
use pbsgui_ipc::{FileInfo, Reply, Request, Responder, SnapshotInfo, SqlAuth};
use tokio::sync::mpsc;

use crate::config::unix_now;
use crate::jobstore::JobStore;
use crate::{backup, connstore, restore, secrets};

/// Archive name for a job's filesystem backup.
const ARCHIVE_NAME: &str = "files.didx";
/// Backup type used for filesystem backups.
const BACKUP_TYPE: &str = "host";
/// Backup type used for SQL Server backups.
const SQL_BACKUP_TYPE: &str = "mssql";

/// Handle one IPC request against the shared job store.
pub async fn handle(store: Arc<JobStore>, request: Request, mut responder: Responder) {
    match request {
        Request::Ping => {
            let _ = responder.send(&Reply::Pong).await;
        }

        Request::ListJobs => {
            let _ = responder.send(&Reply::Jobs { jobs: store.list() }).await;
        }

        Request::SaveJob { job } => {
            let id = job.id.clone();
            let _ = responder.send(&saved_reply(id, store.save_job(job))).await;
        }

        Request::DeleteJob { id } => {
            let _ = responder.send(&deleted_reply(store.delete(&id))).await;
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

        Request::DiscoverSql {
            include_network,
            targets,
        } => {
            let instances = crate::sql::discover::discover(include_network, targets).await;
            let _ = responder.send(&Reply::SqlInstances { instances }).await;
        }

        Request::ProbeSql {
            server,
            port,
            auth,
            password,
        } => {
            let reply =
                match crate::sql::probe::probe(&server, port, &auth, password.as_deref()).await {
                    Ok(probe) => Reply::SqlProbe { probe },
                    Err(e) => Reply::Error {
                        message: e.to_string(),
                    },
                };
            let _ = responder.send(&reply).await;
        }

        Request::CheckSql {
            server,
            port,
            auth,
            password,
        } => {
            let checks = crate::sql::check::check(&server, port, &auth, password.as_deref()).await;
            let _ = responder.send(&Reply::SqlChecks { checks }).await;
        }

        Request::BackupSqlToFile {
            server,
            port,
            auth,
            password,
            database,
            output_path,
        } => {
            backup_sql_to_file(
                server,
                port,
                auth,
                password,
                database,
                output_path,
                responder,
            )
            .await;
        }

        Request::BackupSqlToPbs {
            server,
            port,
            auth,
            password,
            database,
            pbs_server_id,
            backup_id,
        } => {
            backup_sql_to_pbs(
                server,
                port,
                auth,
                password,
                database,
                pbs_server_id,
                backup_id,
                responder,
            )
            .await;
        }

        Request::ListSqlConnections => {
            let connections = connstore::sql_connections().list();
            let _ = responder.send(&Reply::SqlConnections { connections }).await;
        }
        Request::SaveSqlConnection { connection, secret } => {
            let id = connection.id.clone();
            let result = (|| -> anyhow::Result<()> {
                if let Some(secret) = secret {
                    secrets::set(&connstore::sql_secret_key(&id), &secret)?;
                }
                connstore::sql_connections().save(connection)
            })();
            let _ = responder.send(&saved_reply(id, result)).await;
        }
        Request::DeleteSqlConnection { id } => {
            let _ = secrets::delete(&connstore::sql_secret_key(&id));
            let _ = responder
                .send(&deleted_reply(connstore::sql_connections().delete(&id)))
                .await;
        }

        Request::ListPbsServers => {
            let servers = connstore::pbs_servers().list();
            let _ = responder.send(&Reply::PbsServers { servers }).await;
        }
        Request::SavePbsServer { server, secret } => {
            let id = server.id.clone();
            let result = (|| -> anyhow::Result<()> {
                if let Some(secret) = secret {
                    secrets::set(&connstore::pbs_secret_key(&id), &secret)?;
                }
                connstore::pbs_servers().save(server)
            })();
            let _ = responder.send(&saved_reply(id, result)).await;
        }
        Request::DeletePbsServer { id } => {
            let _ = secrets::delete(&connstore::pbs_secret_key(&id));
            let _ = responder
                .send(&deleted_reply(connstore::pbs_servers().delete(&id)))
                .await;
        }
    }
}

fn saved_reply(id: String, result: anyhow::Result<()>) -> Reply {
    match result {
        Ok(()) => Reply::Saved { id },
        Err(e) => Reply::Error {
            message: e.to_string(),
        },
    }
}

fn deleted_reply(result: anyhow::Result<()>) -> Reply {
    match result {
        Ok(()) => Reply::Deleted,
        Err(e) => Reply::Error {
            message: e.to_string(),
        },
    }
}

/// A PBS-safe archive name derived from a database name.
fn sql_archive_name(database: &str) -> String {
    let safe: String = database
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.didx")
}

#[allow(clippy::too_many_arguments)]
async fn backup_sql_to_pbs(
    server: String,
    port: Option<u16>,
    auth: SqlAuth,
    password: Option<String>,
    database: String,
    pbs_server_id: String,
    backup_id: String,
    mut responder: Responder,
) {
    let _ = responder
        .send(&Reply::Accepted {
            job_id: database.clone(),
        })
        .await;
    let _ = responder
        .send(&Reply::Log {
            line: format!("backing up [{database}] over VDI to PBS (group {backup_id})"),
        })
        .await;

    let result = run_backup_sql_to_pbs(
        &server,
        port,
        &auth,
        password.as_deref(),
        &database,
        &pbs_server_id,
        &backup_id,
    )
    .await;

    let reply = match result {
        Ok(message) => Reply::Finished {
            success: true,
            message,
        },
        Err(e) => Reply::Finished {
            success: false,
            message: format!("{e:#}"),
        },
    };
    let _ = responder.send(&reply).await;
}

#[allow(clippy::too_many_arguments)]
async fn run_backup_sql_to_pbs(
    server: &str,
    port: Option<u16>,
    auth: &SqlAuth,
    password: Option<&str>,
    database: &str,
    pbs_server_id: &str,
    backup_id: &str,
) -> anyhow::Result<String> {
    let pbs = connstore::pbs_servers()
        .get(pbs_server_id)
        .ok_or_else(|| anyhow::anyhow!("no such PBS server"))?;
    let secret = secrets::get(&connstore::pbs_secret_key(pbs_server_id))?
        .ok_or_else(|| anyhow::anyhow!("no saved secret for this PBS server"))?;
    let repo: Repository = pbs.repository.parse()?;
    let backup_time = unix_now();
    let archive = sql_archive_name(database);
    let params = SessionParams::from_repository(
        &repo,
        secret,
        &pbs.fingerprint,
        SQL_BACKUP_TYPE,
        backup_id,
        backup_time,
    )?;
    let stats = crate::sql::vdi::backup_database_to_pbs(
        server, port, auth, password, database, &params, &archive,
    )
    .await?;
    Ok(format!(
        "backed up {database}: {} chunks, {} uploaded, {} reused, {} bytes",
        stats.chunks, stats.uploaded, stats.reused, stats.bytes
    ))
}

#[allow(clippy::too_many_arguments)]
async fn backup_sql_to_file(
    server: String,
    port: Option<u16>,
    auth: SqlAuth,
    password: Option<String>,
    database: String,
    output_path: String,
    mut responder: Responder,
) {
    let _ = responder
        .send(&Reply::Accepted {
            job_id: database.clone(),
        })
        .await;
    let _ = responder
        .send(&Reply::Log {
            line: format!("backing up [{database}] over VDI to {output_path}"),
        })
        .await;

    let result = crate::sql::vdi::backup_database_to_file(
        &server,
        port,
        &auth,
        password.as_deref(),
        &database,
        &output_path,
    )
    .await;

    let reply = match result {
        Ok(bytes) => Reply::Finished {
            success: true,
            message: format!("backed up {bytes} bytes to {output_path}"),
        },
        // `{:#}` includes the full error chain (the SQL Server message lives there).
        Err(e) => Reply::Finished {
            success: false,
            message: format!("{e:#}"),
        },
    };
    let _ = responder.send(&reply).await;
}

/// The PBS connection details for browsing/restoring a job's snapshots.
/// Browse and restore currently support file backups to a PBS server.
struct PbsContext {
    repo: Repository,
    secret: String,
    fingerprint: String,
    backup_id: String,
}

fn job_pbs_context(store: &JobStore, job_id: &str) -> anyhow::Result<PbsContext> {
    let job = store
        .get(job_id)
        .ok_or_else(|| anyhow::anyhow!("no such job: {job_id}"))?;
    if !matches!(job.source, pbsgui_ipc::JobSource::Files { .. }) {
        anyhow::bail!("browsing and restore are only available for file backups for now");
    }
    let (server_id, backup_id) = match &job.destination {
        pbsgui_ipc::JobDestination::Pbs {
            server_id,
            backup_id,
        } => (server_id.clone(), backup_id.clone()),
        pbsgui_ipc::JobDestination::Folder { .. } => {
            anyhow::bail!("this job backs up to a folder, not PBS")
        }
    };
    let server = connstore::pbs_servers()
        .get(&server_id)
        .ok_or_else(|| anyhow::anyhow!("the job's PBS server no longer exists"))?;
    let secret = secrets::get(&connstore::pbs_secret_key(&server_id))?
        .ok_or_else(|| anyhow::anyhow!("no saved secret for the PBS server"))?;
    let repo: Repository = server.repository.parse()?;
    Ok(PbsContext {
        repo,
        secret,
        fingerprint: server.fingerprint,
        backup_id,
    })
}

async fn list_snapshots(store: &JobStore, job_id: &str) -> anyhow::Result<Vec<SnapshotInfo>> {
    let ctx = job_pbs_context(store, job_id)?;
    let api = ApiClient::from_repository(&ctx.repo, ctx.secret, &ctx.fingerprint)?;
    let snapshots = api
        .list_snapshots(&ctx.repo.datastore, BACKUP_TYPE, &ctx.backup_id)
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
    let ctx = job_pbs_context(store, job_id)?;
    let params = SessionParams::from_repository(
        &ctx.repo,
        ctx.secret,
        &ctx.fingerprint,
        BACKUP_TYPE,
        &ctx.backup_id,
        backup_time,
    )?;
    let mut reader = ReaderClient::connect(&params).await?;
    // Prefer the small catalog blob; fall back to listing the full archive for
    // snapshots made before the catalog existed.
    match reader.download_blob("catalog.json.blob").await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(_) => {
            let archive = reader.restore_dynamic_archive(ARCHIVE_NAME).await?;
            restore::list_tar(&archive)
        }
    }
}

async fn restore_job(
    store: Arc<JobStore>,
    job_id: String,
    backup_time: i64,
    files: Option<Vec<String>>,
    destination: String,
    mut responder: Responder,
) {
    let ctx = match job_pbs_context(&store, &job_id) {
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

    let result = run_restore(&ctx, backup_time, files, &destination, &mut responder).await;
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

async fn run_restore(
    ctx: &PbsContext,
    backup_time: i64,
    files: Option<Vec<String>>,
    destination: &str,
    responder: &mut Responder,
) -> anyhow::Result<String> {
    let params = SessionParams::from_repository(
        &ctx.repo,
        ctx.secret.clone(),
        &ctx.fingerprint,
        BACKUP_TYPE,
        &ctx.backup_id,
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

    let _ = responder
        .send(&Reply::Accepted { job_id: id.clone() })
        .await;

    let (tx, mut rx) = mpsc::channel::<Reply>(64);
    let job_for_run = job.clone();
    let run = tokio::spawn(async move { backup::run_job(&job_for_run, tx).await });

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
