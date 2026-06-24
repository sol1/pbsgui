//! The pbsgui desktop application.
//!
//! Unprivileged control/monitor UI. It connects to `pbsgui-engine` over a local
//! socket (a named pipe on Windows), exposes job CRUD/run commands plus a native
//! folder picker, and forwards run progress to the frontend over a channel. It
//! connects to the engine, which runs as a Windows Service (or `pbsgui-engine
//! serve` in development) - it does not start the engine itself, so closing the
//! GUI never stops backups.

use pbsgui_ipc::{
    EncryptionKeyInfo, FileInfo, Job, MetricsSettings, NotificationSettings, NotifyChannel,
    PbsServer, Reply, Request, RunningJob, SnapshotInfo, SqlAuth, SqlCheck, SqlConnection,
    SqlInstance, SqlProbe, SqlRestorePoint, SqlRestoreWindow, DEFAULT_SOCKET,
};
use tauri::ipc::Channel;

/// Check the engine/service is reachable (used by the Test button).
#[tauri::command]
async fn engine_ping() -> Result<String, String> {
    ensure_engine().await?;
    Ok("connected".to_string())
}

/// Whether the backup service is currently reachable (for the status indicator).
#[tauri::command]
async fn engine_status() -> bool {
    ping_once().await.unwrap_or(false)
}

/// Version + build id (commit) of this GUI, baked at build time, for display.
#[tauri::command]
fn build_info() -> String {
    option_env!("PBSGUI_BUILD")
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} (dev)", env!("CARGO_PKG_VERSION")))
}

/// List saved jobs.
#[tauri::command]
async fn list_jobs() -> Result<Vec<Job>, String> {
    let replies = request_all(Request::ListJobs).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Jobs { jobs } => Some(jobs),
            _ => None,
        })
        .ok_or_else(|| "engine did not return a job list".to_string())
}

/// Create or update a job. Secrets live with the referenced connections.
#[tauri::command]
async fn save_job(job: Job) -> Result<String, String> {
    let replies = request_all(Request::SaveJob { job }).await?;
    saved_id(replies)
}

/// Delete a job.
#[tauri::command]
async fn delete_job(id: String) -> Result<(), String> {
    let replies = request_all(Request::DeleteJob { id }).await?;
    match first_error(&replies) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Run a job, streaming engine replies to the frontend channel.
#[tauri::command]
async fn run_job(id: String, on_event: Channel<Reply>) -> Result<(), String> {
    ensure_engine().await?;
    let name = pbsgui_ipc::socket_name(DEFAULT_SOCKET).map_err(|e| e.to_string())?;
    pbsgui_ipc::send_request(name, &Request::RunJob { id }, move |reply| {
        let _ = on_event.send(reply);
    })
    .await
    .map_err(|e| e.to_string())
}

/// Cancel the in-flight run for a job, if any (best-effort).
#[tauri::command]
async fn cancel_job(id: String) -> Result<(), String> {
    let replies = request_all(Request::CancelJob { id }).await?;
    match first_error(&replies) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// List jobs with a run currently in progress in the engine, started manually or
/// by the scheduler. Polled by the GUI so a background backup is visible on open.
#[tauri::command]
async fn list_running() -> Result<Vec<RunningJob>, String> {
    let replies = request_all(Request::ListRunning).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Running { jobs } => Some(jobs),
            _ => None,
        })
        .ok_or_else(|| "engine did not return the running list".to_string())
}

/// List snapshots for a job's backup group (by date/time).
#[tauri::command]
async fn list_snapshots(job_id: String) -> Result<Vec<SnapshotInfo>, String> {
    let replies = request_all(Request::ListSnapshots { job_id }).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Snapshots { snapshots } => Some(snapshots),
            _ => None,
        })
        .ok_or_else(|| "engine did not return snapshots".to_string())
}

/// List the files inside a snapshot's archive.
#[tauri::command]
async fn list_files(job_id: String, backup_time: i64) -> Result<Vec<FileInfo>, String> {
    let replies = request_all(Request::ListFiles {
        job_id,
        backup_time,
    })
    .await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Files { files } => Some(files),
            _ => None,
        })
        .ok_or_else(|| "engine did not return a file list".to_string())
}

/// Restore a snapshot (full if `files` is None, else the selected paths),
/// streaming progress to the frontend channel.
#[tauri::command]
async fn restore(
    job_id: String,
    backup_time: i64,
    files: Option<Vec<String>>,
    destination: String,
    on_event: Channel<Reply>,
) -> Result<(), String> {
    ensure_engine().await?;
    let name = pbsgui_ipc::socket_name(DEFAULT_SOCKET).map_err(|e| e.to_string())?;
    let request = Request::Restore {
        job_id,
        backup_time,
        files,
        destination,
    };
    pbsgui_ipc::send_request(name, &request, move |reply| {
        let _ = on_event.send(reply);
    })
    .await
    .map_err(|e| e.to_string())
}

/// Discover SQL Server instances (local always; network when requested).
#[tauri::command]
async fn discover_sql(
    include_network: bool,
    targets: Vec<String>,
) -> Result<Vec<SqlInstance>, String> {
    let replies = request_all(Request::DiscoverSql {
        include_network,
        targets,
    })
    .await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::SqlInstances { instances } => Some(instances),
            _ => None,
        })
        .ok_or_else(|| "engine did not return SQL instances".to_string())
}

/// List saved SQL Server connections.
#[tauri::command]
async fn list_sql_connections() -> Result<Vec<SqlConnection>, String> {
    let replies = request_all(Request::ListSqlConnections).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::SqlConnections { connections } => Some(connections),
            _ => None,
        })
        .ok_or_else(|| "engine did not return SQL connections".to_string())
}

/// Create or update a SQL connection; `secret` is stored only when present.
#[tauri::command]
async fn save_sql_connection(
    connection: SqlConnection,
    secret: Option<String>,
) -> Result<String, String> {
    let replies = request_all(Request::SaveSqlConnection { connection, secret }).await?;
    saved_id(replies)
}

/// Delete a SQL connection and its stored secret.
#[tauri::command]
async fn delete_sql_connection(id: String) -> Result<(), String> {
    let replies = request_all(Request::DeleteSqlConnection { id }).await?;
    match first_error(&replies) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// List saved PBS servers.
#[tauri::command]
async fn list_pbs_servers() -> Result<Vec<PbsServer>, String> {
    let replies = request_all(Request::ListPbsServers).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::PbsServers { servers } => Some(servers),
            _ => None,
        })
        .ok_or_else(|| "engine did not return PBS servers".to_string())
}

/// Create or update a PBS server; `secret` is stored only when present.
#[tauri::command]
async fn save_pbs_server(server: PbsServer, secret: Option<String>) -> Result<String, String> {
    let replies = request_all(Request::SavePbsServer { server, secret }).await?;
    saved_id(replies)
}

/// Delete a PBS server and its stored secret.
#[tauri::command]
async fn delete_pbs_server(id: String) -> Result<(), String> {
    let replies = request_all(Request::DeletePbsServer { id }).await?;
    match first_error(&replies) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Validate a PBS server (reachability, fingerprint, token auth, and
/// DatastoreBackup). Returns the engine's pass message, or an error message.
#[tauri::command]
async fn test_pbs_server(server: PbsServer, secret: Option<String>) -> Result<String, String> {
    let replies = request_all(Request::TestPbsServer { server, secret }).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Finished { success, message } => {
                Some(if success { Ok(message) } else { Err(message) })
            }
            _ => None,
        })
        .unwrap_or_else(|| Err("engine did not return a test result".to_string()))
}

/// Generate a fresh encryption key for a job; returns it (to copy) + fingerprint.
#[tauri::command]
async fn generate_encryption_key(job_id: String) -> Result<EncryptionKeyInfo, String> {
    let replies = request_all(Request::GenerateEncryptionKey { job_id }).await?;
    enc_key_info(replies)?.ok_or_else(|| "engine did not return a key".to_string())
}

/// Import an existing base64 key for a job; returns it + fingerprint.
#[tauri::command]
async fn import_encryption_key(job_id: String, key: String) -> Result<EncryptionKeyInfo, String> {
    let replies = request_all(Request::ImportEncryptionKey { job_id, key }).await?;
    enc_key_info(replies)?.ok_or_else(|| "engine did not return a key".to_string())
}

/// The stored key for a job (to copy/reveal), or `None` if it has none.
#[tauri::command]
async fn get_encryption_key(job_id: String) -> Result<Option<EncryptionKeyInfo>, String> {
    let replies = request_all(Request::GetEncryptionKey { job_id }).await?;
    enc_key_info(replies)
}

/// Delete a job's stored encryption key.
#[tauri::command]
async fn clear_encryption_key(job_id: String) -> Result<(), String> {
    let replies = request_all(Request::ClearEncryptionKey { job_id }).await?;
    match first_error(&replies) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Notification settings plus which secrets are stored (for the settings form).
#[derive(serde::Serialize)]
struct NotificationsView {
    settings: NotificationSettings,
    has_smtp_password: bool,
    has_webhook_url: bool,
}

/// Get the global notification settings.
#[tauri::command]
async fn get_notifications() -> Result<NotificationsView, String> {
    let replies = request_all(Request::GetNotifications).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Notifications {
                settings,
                has_smtp_password,
                has_webhook_url,
            } => Some(NotificationsView {
                settings,
                has_smtp_password,
                has_webhook_url,
            }),
            _ => None,
        })
        .ok_or_else(|| "engine did not return notification settings".to_string())
}

/// Save the global notification settings; secrets are stored only when present.
#[tauri::command]
async fn save_notifications(
    settings: NotificationSettings,
    smtp_password: Option<String>,
    webhook_url: Option<String>,
) -> Result<String, String> {
    let replies = request_all(Request::SaveNotifications {
        settings,
        smtp_password,
        webhook_url,
    })
    .await?;
    saved_id(replies)
}

/// Send a test notification through one channel; returns the engine's message.
#[tauri::command]
async fn test_notification(channel: NotifyChannel) -> Result<String, String> {
    let replies = request_all(Request::TestNotification { channel }).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Finished { success, message } => {
                Some(if success { Ok(message) } else { Err(message) })
            }
            _ => None,
        })
        .unwrap_or_else(|| Err("engine did not return a test result".to_string()))
}

/// Get the Prometheus metrics exporter settings.
#[tauri::command]
async fn get_metrics() -> Result<MetricsSettings, String> {
    let replies = request_all(Request::GetMetrics).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Metrics { settings } => Some(settings),
            _ => None,
        })
        .ok_or_else(|| "engine did not return metrics settings".to_string())
}

/// Save the metrics settings; the engine (re)starts or stops the exporter.
#[tauri::command]
async fn save_metrics(settings: MetricsSettings) -> Result<MetricsSettings, String> {
    let replies = request_all(Request::SaveMetrics { settings }).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Metrics { settings } => Some(settings),
            _ => None,
        })
        .ok_or_else(|| "engine did not return metrics settings".to_string())
}

/// Connect to one instance and report its version, topology, and databases.
#[tauri::command]
async fn probe_sql(
    server: String,
    port: Option<u16>,
    auth: SqlAuth,
    password: Option<String>,
) -> Result<SqlProbe, String> {
    let replies = request_all(Request::ProbeSql {
        server,
        port,
        auth,
        password,
    })
    .await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::SqlProbe { probe } => Some(probe),
            _ => None,
        })
        .ok_or_else(|| "engine did not return a probe result".to_string())
}

/// List a SQL database's snapshots for a backup job, by date/time.
#[tauri::command]
async fn list_sql_snapshots(job_id: String, database: String) -> Result<Vec<SnapshotInfo>, String> {
    let replies = request_all(Request::ListSqlSnapshots { job_id, database }).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Snapshots { snapshots } => Some(snapshots),
            _ => None,
        })
        .ok_or_else(|| "engine did not return snapshots".to_string())
}

/// The restore options for one database of a SQL job (full points + PIT window).
#[tauri::command]
async fn get_sql_restore_window(
    job_id: String,
    database: String,
) -> Result<SqlRestoreWindow, String> {
    let replies = request_all(Request::GetSqlRestoreWindow { job_id, database }).await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::SqlRestoreWindow { window } => Some(window),
            _ => None,
        })
        .ok_or_else(|| "engine did not return a restore window".to_string())
}

/// Restore a SQL database via VDI to a point (a full snapshot or a moment in
/// time), streaming progress.
#[tauri::command]
async fn restore_sql(
    job_id: String,
    database: String,
    target_database: String,
    point: SqlRestorePoint,
    on_event: Channel<Reply>,
) -> Result<(), String> {
    ensure_engine().await?;
    let name = pbsgui_ipc::socket_name(DEFAULT_SOCKET).map_err(|e| e.to_string())?;
    let request = Request::RestoreSql {
        job_id,
        database,
        target_database,
        point,
    };
    pbsgui_ipc::send_request(name, &request, move |reply| {
        let _ = on_event.send(reply);
    })
    .await
    .map_err(|e| e.to_string())
}

/// Run readiness checks against a SQL Server instance.
#[tauri::command]
async fn check_sql(
    server: String,
    port: Option<u16>,
    auth: SqlAuth,
    password: Option<String>,
) -> Result<Vec<SqlCheck>, String> {
    let replies = request_all(Request::CheckSql {
        server,
        port,
        auth,
        password,
    })
    .await?;
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::SqlChecks { checks } => Some(checks),
            _ => None,
        })
        .ok_or_else(|| "engine did not return checks".to_string())
}

/// Native single-folder picker for a restore destination.
#[tauri::command]
async fn pick_destination() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        rfd::FileDialog::new()
            .pick_folder()
            .map(|p| p.display().to_string())
    })
    .await
    .ok()
    .flatten()
}

/// Native folder picker; returns selected paths.
#[tauri::command]
async fn pick_folders() -> Vec<String> {
    pick(true).await
}

/// Native file picker; returns selected paths.
#[tauri::command]
async fn pick_files() -> Vec<String> {
    pick(false).await
}

async fn pick(folders: bool) -> Vec<String> {
    tokio::task::spawn_blocking(move || {
        let dialog = rfd::FileDialog::new();
        let picked = if folders {
            dialog.pick_folders()
        } else {
            dialog.pick_files()
        };
        picked
            .map(|paths| paths.iter().map(|p| p.display().to_string()).collect())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

async fn request_all(request: Request) -> Result<Vec<Reply>, String> {
    ensure_engine().await?;
    let name = pbsgui_ipc::socket_name(DEFAULT_SOCKET).map_err(|e| e.to_string())?;
    let mut replies = Vec::new();
    pbsgui_ipc::send_request(name, &request, |reply| replies.push(reply))
        .await
        .map_err(|e| e.to_string())?;
    Ok(replies)
}

/// Extract the key info from an encryption-key reply (`None` when no key), or
/// surface the engine error.
fn enc_key_info(replies: Vec<Reply>) -> Result<Option<EncryptionKeyInfo>, String> {
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::EncryptionKey { info } => Some(info),
            _ => None,
        })
        .ok_or_else(|| "engine did not return a key result".to_string())
}

fn first_error(replies: &[Reply]) -> Option<String> {
    replies.iter().find_map(|r| match r {
        Reply::Error { message } => Some(message.clone()),
        _ => None,
    })
}

/// Extract the saved id from a save reply, or the error.
fn saved_id(replies: Vec<Reply>) -> Result<String, String> {
    if let Some(err) = first_error(&replies) {
        return Err(err);
    }
    replies
        .into_iter()
        .find_map(|r| match r {
            Reply::Saved { id } => Some(id),
            _ => None,
        })
        .ok_or_else(|| "engine did not confirm the save".to_string())
}

/// Require the engine/service to be reachable on the IPC socket.
async fn ensure_engine() -> Result<(), String> {
    if ping_once().await.unwrap_or(false) {
        Ok(())
    } else {
        Err("backup service is not reachable (is the pbsgui-engine service running?)".to_string())
    }
}

async fn ping_once() -> Result<bool, String> {
    let name = pbsgui_ipc::socket_name(DEFAULT_SOCKET).map_err(|e| e.to_string())?;
    let mut got_pong = false;
    pbsgui_ipc::send_request(name, &Request::Ping, |reply| {
        if matches!(reply, Reply::Pong) {
            got_pong = true;
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(got_pong)
}

/// Show, restore, and focus the main window (from a tray action).
#[cfg(windows)]
fn show_main<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    use tauri::Manager;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Build the system-tray icon with a Show/Quit menu.
#[cfg(windows)]
fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let show = MenuItemBuilder::with_id("show", "Show pbsgui").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app).items(&[&show, &quit]).build()?;

    TrayIconBuilder::with_id("main")
        .tooltip("pbsgui")
        .icon(app.default_window_icon().cloned().expect("window icon"))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default().invoke_handler(tauri::generate_handler![
        engine_ping,
        engine_status,
        build_info,
        list_jobs,
        save_job,
        delete_job,
        run_job,
        cancel_job,
        list_running,
        list_snapshots,
        list_files,
        restore,
        discover_sql,
        probe_sql,
        check_sql,
        list_sql_snapshots,
        get_sql_restore_window,
        restore_sql,
        list_sql_connections,
        save_sql_connection,
        delete_sql_connection,
        list_pbs_servers,
        save_pbs_server,
        delete_pbs_server,
        test_pbs_server,
        generate_encryption_key,
        import_encryption_key,
        get_encryption_key,
        clear_encryption_key,
        get_notifications,
        save_notifications,
        get_metrics,
        save_metrics,
        test_notification,
        pick_destination,
        pick_folders,
        pick_files
    ]);

    // On Windows, closing or minimizing tucks the window into the system tray
    // instead of exiting; the backup service keeps running regardless.
    #[cfg(windows)]
    {
        builder = builder
            .on_window_event(|window, event| match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    let _ = window.hide();
                    api.prevent_close();
                }
                tauri::WindowEvent::Resized(_) => {
                    if window.is_minimized().unwrap_or(false) {
                        let _ = window.hide();
                    }
                }
                _ => {}
            })
            .setup(|app| {
                build_tray(app)?;
                Ok(())
            });
    }

    builder
        .run(tauri::generate_context!())
        .expect("error while running pbsgui");
}
