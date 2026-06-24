//! IPC request handler: job CRUD, runs, browse, and restore.

use std::collections::HashSet;
use std::sync::Arc;

use pbs_client::api::ApiClient;
use pbs_client::session::{ReaderClient, SessionParams};
use pbs_client::{CryptConfig, Repository};
use pbsgui_ipc::{
    FileInfo, JobSource, Reply, Request, Responder, SnapshotInfo, SqlAuth, SqlBackupType,
    SqlProtection, SqlRestorePoint, SqlRestoreWindow,
};
use tokio::sync::mpsc;

use crate::config::unix_now;
use crate::jobstore::JobStore;
use crate::{backup, connstore, enckey, metrics, notify, restore, secrets};

/// Archive name for a job's filesystem backup.
const ARCHIVE_NAME: &str = "files.didx";
/// Backup type used for filesystem backups.
const BACKUP_TYPE: &str = "host";
/// Backup type used for SQL Server backups.
// PBS only accepts the backup types vm, ct, and host, so SQL Server backups use
// "host" and are kept distinct by their snapshot group id (which carries the
// database name).
const SQL_BACKUP_TYPE: &str = "host";

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
            // Remove the job's encryption key too; nothing else references it.
            let _ = enckey::clear(&id);
            let _ = responder.send(&deleted_reply(store.delete(&id))).await;
        }

        Request::RunJob { id } => {
            run_job(store, id, responder).await;
        }

        Request::CancelJob { id } => {
            let running = backup::cancel_job(&id);
            let message = if running {
                "cancelling the running backup".to_string()
            } else {
                "no run is in progress for this job".to_string()
            };
            let _ = responder
                .send(&Reply::Finished {
                    success: running,
                    message,
                })
                .await;
        }

        Request::ListRunning => {
            let _ = responder
                .send(&Reply::Running {
                    jobs: backup::running_snapshot(),
                })
                .await;
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

        Request::ListSqlSnapshots { job_id, database } => {
            let reply = match list_sql_snapshots(&store, &job_id, &database).await {
                Ok(snapshots) => Reply::Snapshots { snapshots },
                Err(e) => Reply::Error {
                    message: format!("{e:#}"),
                },
            };
            let _ = responder.send(&reply).await;
        }

        Request::GetSqlRestoreWindow { job_id, database } => {
            let reply = match get_sql_restore_window(&store, &job_id, &database).await {
                Ok(window) => Reply::SqlRestoreWindow { window },
                Err(e) => Reply::Error {
                    message: format!("{e:#}"),
                },
            };
            let _ = responder.send(&reply).await;
        }
        Request::RestoreSql {
            job_id,
            database,
            target_database,
            point,
        } => {
            restore_sql(store, job_id, database, target_database, point, responder).await;
        }
        Request::RestoreSqlToFile {
            job_id,
            database,
            point,
            destination,
        } => {
            restore_sql_to_file(store, job_id, database, point, destination, responder).await;
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
        Request::TestPbsServer { server, secret } => {
            let (success, message) = match test_pbs_server(server, secret).await {
                Ok(msg) => (true, msg),
                Err(e) => (false, format!("{e:#}")),
            };
            let _ = responder.send(&Reply::Finished { success, message }).await;
        }

        Request::GenerateEncryptionKey { job_id } => {
            let _ = responder
                .send(&enc_key_reply(enckey::generate(&job_id)))
                .await;
        }
        Request::ImportEncryptionKey { job_id, key } => {
            let _ = responder
                .send(&enc_key_reply(enckey::import(&job_id, &key)))
                .await;
        }
        Request::GetEncryptionKey { job_id } => {
            let reply = match enckey::get(&job_id) {
                Ok(info) => Reply::EncryptionKey { info },
                Err(e) => Reply::Error {
                    message: e.to_string(),
                },
            };
            let _ = responder.send(&reply).await;
        }
        Request::ClearEncryptionKey { job_id } => {
            let reply = match enckey::clear(&job_id) {
                Ok(()) => Reply::EncryptionKey { info: None },
                Err(e) => Reply::Error {
                    message: e.to_string(),
                },
            };
            let _ = responder.send(&reply).await;
        }

        Request::GetNotifications => {
            let settings = notify::load();
            let (has_smtp_password, has_webhook_url) = notify::secret_flags();
            let _ = responder
                .send(&Reply::Notifications {
                    settings,
                    has_smtp_password,
                    has_webhook_url,
                })
                .await;
        }
        Request::SaveNotifications {
            settings,
            smtp_password,
            webhook_url,
        } => {
            let result = (|| -> anyhow::Result<()> {
                notify::set_smtp_password(smtp_password.as_deref())?;
                notify::set_webhook_url(webhook_url.as_deref())?;
                notify::save(&settings)
            })();
            let _ = responder
                .send(&saved_reply("notifications".to_string(), result))
                .await;
        }
        Request::TestNotification { channel } => {
            let reply = match notify::send_test(channel).await {
                Ok(()) => Reply::Finished {
                    success: true,
                    message: "test notification sent".to_string(),
                },
                Err(e) => Reply::Finished {
                    success: false,
                    message: format!("{e:#}"),
                },
            };
            let _ = responder.send(&reply).await;
        }

        Request::GetMetrics => {
            let _ = responder
                .send(&Reply::Metrics {
                    settings: metrics::load(),
                })
                .await;
        }
        Request::SaveMetrics { settings } => {
            let reply = match metrics::save(&settings) {
                Ok(()) => {
                    metrics::apply(store.clone());
                    Reply::Metrics { settings }
                }
                Err(e) => Reply::Error {
                    message: format!("{e:#}"),
                },
            };
            let _ = responder.send(&reply).await;
        }
    }
}

/// Wrap a generate/import result (which always yields a key) into a reply.
fn enc_key_reply(result: anyhow::Result<pbsgui_ipc::EncryptionKeyInfo>) -> Reply {
    match result {
        Ok(info) => Reply::EncryptionKey { info: Some(info) },
        Err(e) => Reply::Error {
            message: e.to_string(),
        },
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

/// Validate a PBS server: reachability, the pinned TLS fingerprint, that the token
/// authenticates, and that it holds `Datastore.Backup` on the datastore/namespace.
/// One `/access/permissions` round-trip covers all four. Does not write a backup.
async fn test_pbs_server(
    server: pbsgui_ipc::PbsServer,
    secret: Option<String>,
) -> anyhow::Result<String> {
    // Use the typed secret, or fall back to the one stored for this saved server.
    let secret = match secret {
        Some(s) if !s.is_empty() => s,
        _ => secrets::get(&connstore::pbs_secret_key(&server.id))?
            .ok_or_else(|| anyhow::anyhow!("no token secret was entered or stored"))?,
    };
    let repo: Repository = server.repository.parse()?;
    let datastore = repo.datastore.clone();
    let namespace = repo.namespace.clone();
    let acl = match &namespace {
        Some(ns) if !ns.is_empty() => format!("{datastore}/{ns}"),
        _ => datastore.clone(),
    };
    let api = ApiClient::from_repository(&repo, secret, &server.fingerprint)?;
    match api.can_backup(&datastore, namespace.as_deref()).await {
        Ok(true) => Ok(format!(
            "Reached PBS; the token can back up to datastore \"{acl}\"."
        )),
        Ok(false) => {
            // The token authenticates but has no effective Datastore.Backup. The
            // usual cause is privilege separation: a token's rights are the
            // INTERSECTION of the token's and its owning user's roles, so granting
            // the role to only the token is not enough - the user needs it too.
            let auth_id = repo.auth_id.as_deref().unwrap_or_default();
            let user = auth_id.split('!').next().unwrap_or(auth_id);
            anyhow::bail!(
                "Reached PBS and the token authenticates, but it has no effective \
                 Datastore.Backup on /datastore/{acl}. PBS API tokens use privilege \
                 separation: a token's rights are the intersection of the token's and \
                 its user's roles, so both need the DatastoreBackup role on this path. \
                 If only the token has it, grant the user '{user}' too, e.g. on the PBS \
                 host:\n  proxmox-backup-manager acl update /datastore/{acl} \
                 DatastoreBackup --auth-id '{user}'"
            )
        }
        Err(e) => {
            let detail = e.to_string();
            if detail.contains("401") {
                anyhow::bail!("The PBS token id or secret was not accepted (401).")
            } else if detail.contains("certificate") || detail.contains("fingerprint") {
                anyhow::bail!("Could not verify the PBS certificate (fingerprint mismatch?): {e}")
            } else {
                anyhow::bail!("Could not reach PBS: {e}")
            }
        }
    }
}

/// A SQL backup job resolved to its source connection and PBS destination.
struct SqlJobPbs {
    conn: pbsgui_ipc::SqlConnection,
    password: Option<String>,
    repo: Repository,
    secret: String,
    fingerprint: String,
    backup_id: String,
    /// The job's encryption key, when it is an encrypted job (for transparent
    /// decryption on restore).
    crypt: Option<CryptConfig>,
}

fn sql_job_pbs(store: &JobStore, job_id: &str) -> anyhow::Result<SqlJobPbs> {
    let job = store
        .get(job_id)
        .ok_or_else(|| anyhow::anyhow!("no such job: {job_id}"))?;
    // Load the key if one is stored, regardless of the job's current `encrypted`
    // flag, so snapshots made while encrypted stay restorable even if the flag is
    // later turned off. Plaintext blobs decode fine even with a key present.
    let crypt = enckey::load_config(&job.id)?;
    let connection_id = match &job.source {
        pbsgui_ipc::JobSource::Sql { connection_id, .. } => connection_id.clone(),
        _ => anyhow::bail!("not a SQL Server backup job"),
    };
    let (server_id, backup_id) = match &job.destination {
        pbsgui_ipc::JobDestination::Pbs {
            server_id,
            backup_id,
        } => (server_id.clone(), backup_id.clone()),
        pbsgui_ipc::JobDestination::Folder { .. } => {
            anyhow::bail!("this job backs up to a folder, not PBS")
        }
    };
    let conn = connstore::sql_connections()
        .get(&connection_id)
        .ok_or_else(|| anyhow::anyhow!("the job's SQL connection no longer exists"))?;
    let password = match conn.auth {
        SqlAuth::Integrated => None,
        _ => secrets::get(&connstore::sql_secret_key(&connection_id))?,
    };
    let server = connstore::pbs_servers()
        .get(&server_id)
        .ok_or_else(|| anyhow::anyhow!("the job's PBS server no longer exists"))?;
    let secret = secrets::get(&connstore::pbs_secret_key(&server_id))?
        .ok_or_else(|| anyhow::anyhow!("no saved secret for the PBS server"))?;
    let repo: Repository = server.repository.parse()?;
    Ok(SqlJobPbs {
        conn,
        password,
        repo,
        secret,
        fingerprint: server.fingerprint,
        backup_id,
        crypt,
    })
}

async fn list_sql_snapshots(
    store: &JobStore,
    job_id: &str,
    database: &str,
) -> anyhow::Result<Vec<SnapshotInfo>> {
    let ctx = sql_job_pbs(store, job_id)?;
    // Browse/restore target the full-backup group (log restore is not wired yet).
    let (group, _archive) =
        backup::sql_group_and_archive(&ctx.backup_id, database, SqlBackupType::Full);
    let api = ApiClient::from_repository(&ctx.repo, ctx.secret, &ctx.fingerprint)?;
    let snapshots = api
        .list_snapshots(
            &ctx.repo.datastore,
            ctx.repo.namespace.as_deref(),
            SQL_BACKUP_TYPE,
            &group,
        )
        .await?;
    Ok(snapshots
        .into_iter()
        .map(|s| SnapshotInfo {
            backup_time: s.backup_time,
            size: s.size,
        })
        .collect())
}

/// A point-in-time job's per-database chain status (for stall alerts and metrics).
pub(crate) struct JobSqlStatus {
    pub job_id: String,
    pub job_name: String,
    pub databases: Vec<metrics::SqlDbStatus>,
}

/// Count and the earliest/newest snapshot time of one group.
async fn group_stats(
    api: &ApiClient,
    ctx: &SqlJobPbs,
    database: &str,
    kind: SqlBackupType,
) -> (u32, Option<i64>, Option<i64>) {
    let (group, _archive) = backup::sql_group_and_archive(&ctx.backup_id, database, kind);
    match api
        .list_snapshots(
            &ctx.repo.datastore,
            ctx.repo.namespace.as_deref(),
            SQL_BACKUP_TYPE,
            &group,
        )
        .await
    {
        Ok(snaps) => (
            snaps.len() as u32,
            snaps.iter().map(|s| s.backup_time).min(),
            snaps.iter().map(|s| s.backup_time).max(),
        ),
        Err(_) => (0, None, None),
    }
}

/// Gather the chain status of every point-in-time job's databases by reading the
/// shared PBS groups. Reading the shared store is what makes stall detection work
/// across Always On replicas with no link between the pbsgui instances. A database
/// with no snapshots yet is reported but never marked stalled.
pub(crate) async fn collect_sql_status(store: &JobStore, now: i64) -> Vec<JobSqlStatus> {
    let mut out = Vec::new();
    for job in store.list() {
        let (databases, log_interval) = match &job.source {
            JobSource::Sql {
                databases,
                protection:
                    SqlProtection::PointInTime {
                        log_interval_minutes,
                        ..
                    },
                ..
            } => (databases.clone(), *log_interval_minutes as i64),
            _ => continue,
        };
        // Allow several missed log intervals before warning (floor 30 minutes).
        let grace = (log_interval * 60 * 4).max(1800);
        let ctx = match sql_job_pbs(store, &job.id) {
            Ok(ctx) => ctx,
            Err(_) => continue, // folder destination, missing server, etc.
        };
        let api = match ApiClient::from_repository(&ctx.repo, ctx.secret.clone(), &ctx.fingerprint)
        {
            Ok(api) => api,
            Err(_) => continue,
        };
        let mut dbs = Vec::new();
        for db in &databases {
            let (full_count, full_min, full_max) =
                group_stats(&api, &ctx, db, SqlBackupType::Full).await;
            let (log_count, _log_min, log_max) =
                group_stats(&api, &ctx, db, SqlBackupType::Log).await;
            let chain_latest = [full_max, log_max].into_iter().flatten().max();
            let pit_window_secs = match (chain_latest, full_min) {
                (Some(latest), Some(earliest)) if log_count > 0 => Some(latest - earliest),
                _ => None,
            };
            let stalled = chain_latest.is_some_and(|l| now - l > grace);
            dbs.push(metrics::SqlDbStatus {
                database: db.clone(),
                chain_latest,
                stalled,
                full_count,
                log_count,
                pit_window_secs,
            });
        }
        out.push(JobSqlStatus {
            job_id: job.id.clone(),
            job_name: job.name.clone(),
            databases: dbs,
        });
    }
    out
}

/// Report the restore options for one database: the full restore points and, if
/// log backups exist, the earliest/latest point-in-time bounds.
async fn get_sql_restore_window(
    store: &JobStore,
    job_id: &str,
    database: &str,
) -> anyhow::Result<SqlRestoreWindow> {
    let ctx = sql_job_pbs(store, job_id)?;
    let (full_group, _) =
        backup::sql_group_and_archive(&ctx.backup_id, database, SqlBackupType::Full);
    let (log_group, _) =
        backup::sql_group_and_archive(&ctx.backup_id, database, SqlBackupType::Log);
    let api = ApiClient::from_repository(&ctx.repo, ctx.secret.clone(), &ctx.fingerprint)?;
    let ns = ctx.repo.namespace.as_deref();

    let mut fulls = api
        .list_snapshots(&ctx.repo.datastore, ns, SQL_BACKUP_TYPE, &full_group)
        .await?;
    fulls.sort_by_key(|s| std::cmp::Reverse(s.backup_time)); // newest first
    let logs = api
        .list_snapshots(&ctx.repo.datastore, ns, SQL_BACKUP_TYPE, &log_group)
        .await
        .unwrap_or_default();

    // Point-in-time is available only when there are logs to replay over a full.
    let (pit_earliest, pit_latest) = if !logs.is_empty() && !fulls.is_empty() {
        let earliest = fulls.iter().map(|s| s.backup_time).min();
        let latest = fulls.iter().chain(logs.iter()).map(|s| s.backup_time).max();
        (earliest, latest)
    } else {
        (None, None)
    };

    // None total if any log's size is unknown.
    let log_total_size = logs
        .iter()
        .map(|s| s.size)
        .try_fold(0u64, |acc, s| s.map(|v| acc + v));
    Ok(SqlRestoreWindow {
        full_points: fulls
            .into_iter()
            .map(|s| pbsgui_ipc::SqlFullPoint {
                backup_time: s.backup_time,
                size: s.size,
            })
            .collect(),
        pit_earliest,
        pit_latest,
        log_count: logs.len() as u32,
        log_total_size,
    })
}

async fn restore_sql(
    store: Arc<JobStore>,
    job_id: String,
    database: String,
    target_database: String,
    point: SqlRestorePoint,
    mut responder: Responder,
) {
    let _ = responder
        .send(&Reply::Accepted {
            job_id: database.clone(),
        })
        .await;
    let result = run_restore_sql(
        &store,
        &job_id,
        &database,
        &target_database,
        point,
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
            message: format!("{e:#}"),
        },
    };
    let _ = responder.send(&reply).await;
}

async fn run_restore_sql(
    store: &JobStore,
    job_id: &str,
    database: &str,
    target_database: &str,
    point: SqlRestorePoint,
    responder: &mut Responder,
) -> anyhow::Result<String> {
    let ctx = sql_job_pbs(store, job_id)?;
    match point {
        SqlRestorePoint::Full { backup_time } => {
            run_restore_full(&ctx, database, target_database, backup_time, responder).await
        }
        SqlRestorePoint::PointInTime { unix_time } => {
            run_restore_pit(&ctx, database, target_database, unix_time, responder).await
        }
    }
}

/// Restore one full snapshot (no log replay).
async fn run_restore_full(
    ctx: &SqlJobPbs,
    database: &str,
    target_database: &str,
    backup_time: i64,
    responder: &mut Responder,
) -> anyhow::Result<String> {
    let (group, archive) =
        backup::sql_group_and_archive(&ctx.backup_id, database, SqlBackupType::Full);
    let new_name = !database.eq_ignore_ascii_case(target_database);
    // A renamed restore relocates the files, which needs the file list captured at
    // backup time. When it is present (or restoring over the original name) the
    // restore streams straight into SQL Server with bounded memory; an older backup
    // without it falls back to a buffered restore.
    let files = if new_name {
        download_sql_meta(ctx, &group, backup_time)
            .await
            .map(|m| m.files)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let _ = responder
        .send(&Reply::Log {
            line: format!("restoring as [{target_database}] (overwrites it if it exists)"),
        })
        .await;
    if !new_name || !files.is_empty() {
        let _ = responder
            .send(&Reply::Log {
                line: "streaming the restore from PBS into SQL Server".to_string(),
            })
            .await;
        let pbs = SessionParams::from_repository(
            &ctx.repo,
            ctx.secret.clone(),
            &ctx.fingerprint,
            SQL_BACKUP_TYPE,
            &group,
            backup_time,
        )?;
        crate::sql::vdi::restore_database_streamed(
            &ctx.conn.server,
            ctx.conn.port,
            &ctx.conn.auth,
            ctx.password.as_deref(),
            database,
            target_database,
            &pbs,
            &archive,
            ctx.crypt.clone(),
            &files,
        )
        .await?;
    } else {
        let _ = responder
            .send(&Reply::Log {
                line: "older backup without a stored file list; using a buffered restore"
                    .to_string(),
            })
            .await;
        let image = download_sql_image(ctx, &group, &archive, backup_time).await?;
        crate::sql::vdi::restore_database_from_image(
            &ctx.conn.server,
            ctx.conn.port,
            &ctx.conn.auth,
            ctx.password.as_deref(),
            database,
            target_database,
            image,
        )
        .await?;
    }
    Ok(format!("restored {database} as {target_database}"))
}

/// Restore to a point in time: the covering full plus its log chain, trimmed to
/// `target_unix` with STOPAT.
async fn run_restore_pit(
    ctx: &SqlJobPbs,
    database: &str,
    target_database: &str,
    target_unix: i64,
    responder: &mut Responder,
) -> anyhow::Result<String> {
    use crate::sql::backupmeta::{self, ChainItem};

    let (full_group, archive) =
        backup::sql_group_and_archive(&ctx.backup_id, database, SqlBackupType::Full);
    let (log_group, _) =
        backup::sql_group_and_archive(&ctx.backup_id, database, SqlBackupType::Log);
    let api = ApiClient::from_repository(&ctx.repo, ctx.secret.clone(), &ctx.fingerprint)?;
    let ns = ctx.repo.namespace.as_deref();

    let _ = responder
        .send(&Reply::Log {
            line: "reading the backup chain".to_string(),
        })
        .await;
    let mut items: Vec<ChainItem> = Vec::new();
    for (group, _is_log) in [(&full_group, false), (&log_group, true)] {
        let snaps = api
            .list_snapshots(&ctx.repo.datastore, ns, SQL_BACKUP_TYPE, group)
            .await
            .unwrap_or_default();
        for s in snaps {
            if let Some(meta) = download_sql_meta(ctx, group, s.backup_time).await {
                items.push(ChainItem {
                    snapshot_time: s.backup_time,
                    meta,
                });
            }
        }
    }

    let chain = backupmeta::select_chain(&items, target_unix);
    if chain.is_empty() {
        anyhow::bail!("no full backup covers that time; pick a later restore point");
    }
    let _ = responder
        .send(&Reply::Log {
            line: format!(
                "restoring 1 full + {} log backup(s) to the chosen point",
                chain.len() - 1
            ),
        })
        .await;

    // A renamed restore needs the full's file list (captured at backup time) to
    // relocate files without a second read of the backup. Stream when it is present
    // or the name is unchanged; otherwise fall back to a buffered restore.
    let new_name = !database.eq_ignore_ascii_case(target_database);
    let full_files = chain
        .iter()
        .find(|c| !c.meta.is_log())
        .map(|c| c.meta.files.clone())
        .unwrap_or_default();

    if !new_name || !full_files.is_empty() {
        let _ = responder
            .send(&Reply::Log {
                line: "streaming the restore from PBS into SQL Server".to_string(),
            })
            .await;
        let mut steps: Vec<(bool, SessionParams, String)> = Vec::new();
        for item in &chain {
            let group = if item.meta.is_log() {
                &log_group
            } else {
                &full_group
            };
            let pbs = SessionParams::from_repository(
                &ctx.repo,
                ctx.secret.clone(),
                &ctx.fingerprint,
                SQL_BACKUP_TYPE,
                group,
                item.snapshot_time,
            )?;
            steps.push((item.meta.is_log(), pbs, archive.clone()));
        }
        crate::sql::vdi::restore_chain_streamed(
            &ctx.conn.server,
            ctx.conn.port,
            &ctx.conn.auth,
            ctx.password.as_deref(),
            database,
            target_database,
            steps,
            ctx.crypt.clone(),
            &full_files,
            target_unix,
        )
        .await?;
    } else {
        let _ = responder
            .send(&Reply::Log {
                line: "older backup without a stored file list; using a buffered restore"
                    .to_string(),
            })
            .await;
        // Download each image in apply order (full first); memory holds one chain.
        let mut steps: Vec<(bool, Vec<u8>)> = Vec::new();
        for item in &chain {
            let group = if item.meta.is_log() {
                &log_group
            } else {
                &full_group
            };
            let image = download_sql_image(ctx, group, &archive, item.snapshot_time).await?;
            steps.push((item.meta.is_log(), image));
        }
        crate::sql::vdi::restore_chain(
            &ctx.conn.server,
            ctx.conn.port,
            &ctx.conn.auth,
            ctx.password.as_deref(),
            database,
            target_database,
            steps,
            target_unix,
        )
        .await?;
    }
    Ok(format!(
        "restored {database} as {target_database} to the chosen point in time"
    ))
}

/// Download one SQL snapshot's archive image from PBS.
async fn download_sql_image(
    ctx: &SqlJobPbs,
    group: &str,
    archive: &str,
    backup_time: i64,
) -> anyhow::Result<Vec<u8>> {
    let params = SessionParams::from_repository(
        &ctx.repo,
        ctx.secret.clone(),
        &ctx.fingerprint,
        SQL_BACKUP_TYPE,
        group,
        backup_time,
    )?;
    let mut reader = ReaderClient::connect(&params).await?;
    Ok(reader
        .restore_dynamic_archive(archive, ctx.crypt.as_ref())
        .await?)
}

/// Stream one SQL snapshot's native backup stream from PBS straight to `final_path`
/// (decrypted and decompressed), without holding the whole image in memory. Writes
/// to a `.partial` file and renames it into place on success, so an interrupted
/// download never leaves a file that looks like a complete backup. Returns the
/// bytes written.
async fn download_sql_image_to_file(
    ctx: &SqlJobPbs,
    group: &str,
    archive: &str,
    backup_time: i64,
    final_path: &std::path::Path,
) -> anyhow::Result<u64> {
    if final_path.exists() {
        anyhow::bail!("{} already exists", final_path.display());
    }
    let params = SessionParams::from_repository(
        &ctx.repo,
        ctx.secret.clone(),
        &ctx.fingerprint,
        SQL_BACKUP_TYPE,
        group,
        backup_time,
    )?;
    let mut reader = ReaderClient::connect(&params).await?;

    let mut tmp = final_path.as_os_str().to_owned();
    tmp.push(".partial");
    let tmp_path = std::path::PathBuf::from(tmp);
    let file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", tmp_path.display()))?;
    let mut writer = tokio::io::BufWriter::new(file);

    let streamed = reader
        .restore_dynamic_archive_to_writer(archive, ctx.crypt.as_ref(), &mut writer)
        .await;
    // Close the file (flush the buffer) before renaming or removing it; Windows will
    // not rename or delete a file that is still open.
    let flushed = tokio::io::AsyncWriteExt::flush(&mut writer).await;
    drop(writer);

    let bytes = match (streamed, flushed) {
        (Ok(bytes), Ok(())) => bytes,
        (Err(e), _) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e.into());
        }
        (Ok(_), Err(e)) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(anyhow::anyhow!("writing {}: {e}", tmp_path.display()));
        }
    };
    tokio::fs::rename(&tmp_path, final_path)
        .await
        .map_err(|e| anyhow::anyhow!("finalising {}: {e}", final_path.display()))?;
    Ok(bytes)
}

/// Download and parse one SQL snapshot's chain-metadata blob (`None` on any
/// failure, e.g. an older snapshot without it).
async fn download_sql_meta(
    ctx: &SqlJobPbs,
    group: &str,
    backup_time: i64,
) -> Option<crate::sql::backupmeta::SqlBackupMeta> {
    let params = SessionParams::from_repository(
        &ctx.repo,
        ctx.secret.clone(),
        &ctx.fingerprint,
        SQL_BACKUP_TYPE,
        group,
        backup_time,
    )
    .ok()?;
    let mut reader = ReaderClient::connect(&params).await.ok()?;
    let bytes = reader
        .download_blob(crate::sql::backupmeta::META_BLOB_NAME, ctx.crypt.as_ref())
        .await
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn restore_sql_to_file(
    store: Arc<JobStore>,
    job_id: String,
    database: String,
    point: SqlRestorePoint,
    destination: String,
    mut responder: Responder,
) {
    let _ = responder
        .send(&Reply::Accepted {
            job_id: database.clone(),
        })
        .await;
    let result = run_restore_sql_to_file(
        &store,
        &job_id,
        &database,
        point,
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
            message: format!("{e:#}"),
        },
    };
    let _ = responder.send(&reply).await;
}

/// Restore SQL snapshots from PBS to native backup files in a folder, without
/// touching SQL Server. The stored archive is the native backup stream, so this is
/// just a download (decrypted and decompressed) written to disk.
async fn run_restore_sql_to_file(
    store: &JobStore,
    job_id: &str,
    database: &str,
    point: SqlRestorePoint,
    destination: &str,
    responder: &mut Responder,
) -> anyhow::Result<String> {
    let ctx = sql_job_pbs(store, job_id)?;
    let dir = std::path::Path::new(destination);
    if !dir.is_dir() {
        anyhow::bail!("the destination folder does not exist: {destination}");
    }
    match point {
        SqlRestorePoint::Full { backup_time } => {
            export_sql_full(&ctx, database, backup_time, dir, responder).await
        }
        SqlRestorePoint::PointInTime { unix_time } => {
            export_sql_chain(&ctx, database, unix_time, dir, responder).await
        }
    }
}

/// Write one full snapshot's native backup stream to a `.bak` in `dir`.
async fn export_sql_full(
    ctx: &SqlJobPbs,
    database: &str,
    backup_time: i64,
    dir: &std::path::Path,
    responder: &mut Responder,
) -> anyhow::Result<String> {
    let (group, archive) =
        backup::sql_group_and_archive(&ctx.backup_id, database, SqlBackupType::Full);
    let name = format!(
        "{}-{}.bak",
        sanitize_filename(database),
        file_stamp(backup_time)
    );
    let path = dir.join(&name);
    let _ = responder
        .send(&Reply::Log {
            line: format!("downloading the full backup from PBS to {name}"),
        })
        .await;
    let size = download_sql_image_to_file(ctx, &group, &archive, backup_time, &path).await?;
    let _ = responder
        .send(&Reply::Log {
            line: format!("wrote {name} ({})", human_size(size)),
        })
        .await;
    Ok(format!("saved {database} to {}", path.display()))
}

/// Write the covering full plus the log chain up to `target_unix` as native files
/// (`.bak` + `.trn`), plus a steps file describing the manual `RESTORE` replay.
async fn export_sql_chain(
    ctx: &SqlJobPbs,
    database: &str,
    target_unix: i64,
    dir: &std::path::Path,
    responder: &mut Responder,
) -> anyhow::Result<String> {
    use crate::sql::backupmeta::{self, ChainItem};

    let (full_group, archive) =
        backup::sql_group_and_archive(&ctx.backup_id, database, SqlBackupType::Full);
    let (log_group, _) =
        backup::sql_group_and_archive(&ctx.backup_id, database, SqlBackupType::Log);
    let api = ApiClient::from_repository(&ctx.repo, ctx.secret.clone(), &ctx.fingerprint)?;
    let ns = ctx.repo.namespace.as_deref();

    let _ = responder
        .send(&Reply::Log {
            line: "reading the backup chain".to_string(),
        })
        .await;
    let mut items: Vec<ChainItem> = Vec::new();
    for group in [&full_group, &log_group] {
        let snaps = api
            .list_snapshots(&ctx.repo.datastore, ns, SQL_BACKUP_TYPE, group)
            .await
            .unwrap_or_default();
        for s in snaps {
            if let Some(meta) = download_sql_meta(ctx, group, s.backup_time).await {
                items.push(ChainItem {
                    snapshot_time: s.backup_time,
                    meta,
                });
            }
        }
    }

    let chain = backupmeta::select_chain(&items, target_unix);
    if chain.is_empty() {
        anyhow::bail!("no full backup covers that time; pick a later restore point");
    }
    let _ = responder
        .send(&Reply::Log {
            line: format!(
                "saving 1 full + {} log backup(s) to the chosen point",
                chain.len() - 1
            ),
        })
        .await;

    // Download and write each in apply order; the index-prefixed names sort to the
    // replay order. Each archive streams straight to its file, so only one chunk is
    // ever held in memory (a multi-hundred-GB database exports without buffering).
    let mut files: Vec<(String, bool)> = Vec::new();
    for (i, item) in chain.iter().enumerate() {
        let is_log = item.meta.is_log();
        let group = if is_log { &log_group } else { &full_group };
        let name = format!(
            "{}-{:02}-{}-{}.{}",
            sanitize_filename(database),
            i + 1,
            if is_log { "LOG" } else { "FULL" },
            file_stamp(item.snapshot_time),
            if is_log { "trn" } else { "bak" },
        );
        let size =
            download_sql_image_to_file(ctx, group, &archive, item.snapshot_time, &dir.join(&name))
                .await?;
        let _ = responder
            .send(&Reply::Log {
                line: format!("wrote {name} ({})", human_size(size)),
            })
            .await;
        files.push((name, is_log));
    }

    let steps = build_restore_steps(database, &files, target_unix);
    let steps_name = format!("{}-RESTORE-STEPS.txt", sanitize_filename(database));
    write_export_file(&dir.join(&steps_name), steps.into_bytes()).await?;
    let _ = responder
        .send(&Reply::Log {
            line: format!("wrote {steps_name}"),
        })
        .await;

    Ok(format!(
        "saved {} file(s) for {database} to {}",
        files.len() + 1,
        dir.display()
    ))
}

/// Write `bytes` to `path`, refusing to overwrite an existing file (an export
/// never clobbers). Blocking file I/O runs on the blocking pool.
async fn write_export_file(path: &std::path::Path, bytes: Vec<u8>) -> anyhow::Result<()> {
    use std::io::Write as _;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("creating {}: {e}", path.display()))?;
        f.write_all(&bytes)
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("file write task failed: {e}"))?
}

/// A UTC timestamp for a backup file name, e.g. `20260624T020000Z`.
fn file_stamp(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|d| d.format("%Y%m%dT%H%M%SZ").to_string())
        .unwrap_or_else(|| unix.to_string())
}

/// Keep only filename-safe characters (so a database name is a valid file stem).
fn sanitize_filename(value: &str) -> String {
    let s: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "database".to_string()
    } else {
        s
    }
}

/// A short human-readable byte size for the progress log.
fn human_size(n: u64) -> String {
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
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Build the manual-restore steps text (CRLF, for Windows) accompanying an
/// exported chain: the ordered `RESTORE DATABASE`/`RESTORE LOG` statements.
fn build_restore_steps(database: &str, files: &[(String, bool)], target_unix: i64) -> String {
    let target = chrono::DateTime::from_timestamp(target_unix, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| target_unix.to_string());
    let full = files
        .iter()
        .find(|(_, is_log)| !*is_log)
        .map(|(n, _)| n.as_str())
        .unwrap_or("<full>.bak");
    let logs: Vec<&str> = files
        .iter()
        .filter(|(_, is_log)| *is_log)
        .map(|(n, _)| n.as_str())
        .collect();

    let mut lines: Vec<String> = vec![
        format!("SQL Server restore steps for \"{database}\""),
        "Exported from Proxmox Backup Server by pbsgui. These are native SQL Server".to_string(),
        "backup files; restore them on any SQL Server with the statements below, run".to_string(),
        "in order (SQL Server Management Studio or sqlcmd).".to_string(),
        String::new(),
        "Before running:".to_string(),
        "  - Change [RestoredDB] to the database name you want.".to_string(),
        "  - Prefix each file name with the full path to this folder.".to_string(),
    ];
    if !logs.is_empty() {
        lines.push(format!(
            "  - Recovering to {target} (UTC). STOPAT below uses the SERVER's local"
        ));
        lines.push("    time, so adjust it to your server's timezone.".to_string());
    }
    lines.push(String::new());

    let mut n = 1;
    if logs.is_empty() {
        lines.push(format!(
            "{n}) RESTORE DATABASE [RestoredDB] FROM DISK = N'{full}'"
        ));
        lines.push("       WITH REPLACE, RECOVERY;".to_string());
    } else {
        lines.push(format!(
            "{n}) RESTORE DATABASE [RestoredDB] FROM DISK = N'{full}'"
        ));
        lines.push("       WITH REPLACE, NORECOVERY;".to_string());
        for (i, log) in logs.iter().enumerate() {
            n += 1;
            lines.push(format!(
                "{n}) RESTORE LOG [RestoredDB] FROM DISK = N'{log}'"
            ));
            if i + 1 == logs.len() {
                lines.push(format!("       WITH STOPAT = N'{target}', RECOVERY;"));
            } else {
                lines.push("       WITH NORECOVERY;".to_string());
            }
        }
    }
    lines.push(String::new());
    lines.join("\r\n")
}

/// The PBS connection details for browsing/restoring a job's snapshots.
/// Browse and restore currently support file backups to a PBS server.
struct PbsContext {
    repo: Repository,
    secret: String,
    fingerprint: String,
    backup_id: String,
    /// The job's encryption key, when it is an encrypted job (for transparent
    /// decryption on browse/restore).
    crypt: Option<CryptConfig>,
}

fn job_pbs_context(store: &JobStore, job_id: &str) -> anyhow::Result<PbsContext> {
    let job = store
        .get(job_id)
        .ok_or_else(|| anyhow::anyhow!("no such job: {job_id}"))?;
    if !matches!(job.source, pbsgui_ipc::JobSource::Files { .. }) {
        anyhow::bail!("browsing and restore are only available for file backups for now");
    }
    // Load any stored key regardless of the current `encrypted` flag (see
    // `sql_job_pbs`); plaintext blobs still decode without using it.
    let crypt = enckey::load_config(&job.id)?;
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
        crypt,
    })
}

async fn list_snapshots(store: &JobStore, job_id: &str) -> anyhow::Result<Vec<SnapshotInfo>> {
    let ctx = job_pbs_context(store, job_id)?;
    let api = ApiClient::from_repository(&ctx.repo, ctx.secret, &ctx.fingerprint)?;
    let snapshots = api
        .list_snapshots(
            &ctx.repo.datastore,
            ctx.repo.namespace.as_deref(),
            BACKUP_TYPE,
            &ctx.backup_id,
        )
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
    match reader
        .download_blob("catalog.json.blob", ctx.crypt.as_ref())
        .await
    {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(_) => {
            let archive = reader
                .restore_dynamic_archive(ARCHIVE_NAME, ctx.crypt.as_ref())
                .await?;
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
    let bytes = reader
        .restore_dynamic_archive(ARCHIVE_NAME, ctx.crypt.as_ref())
        .await?;
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
        // `{:#}` includes the full error chain (the SQL Server message lives there).
        Ok(Err(e)) => (false, format!("{e:#}")),
        Err(e) => (false, format!("job task failed: {e}")),
    };
    let status = if success {
        "ok".to_string()
    } else {
        message.clone()
    };
    let _ = store.record_run(&id, unix_now(), status);
    // Refresh the metrics textfile (no-op unless textfile mode is on).
    metrics::write_textfile(&store);
    let _ = responder.send(&Reply::Finished { success, message }).await;
}
