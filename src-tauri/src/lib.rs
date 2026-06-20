//! The pbsgui desktop application.
//!
//! Unprivileged control/monitor UI. It connects to `pbsgui-engine` over a local
//! socket (a named pipe on Windows), exposes job CRUD/run commands plus a native
//! folder picker, and forwards run progress to the frontend over a channel. It
//! connects to the engine, which runs as a Windows Service (or `pbsgui-engine
//! serve` in development) - it does not start the engine itself, so closing the
//! GUI never stops backups.

use pbsgui_ipc::{
    FileInfo, Job, Reply, Request, SnapshotInfo, SqlAuth, SqlCheck, SqlInstance, SqlProbe,
    DEFAULT_SOCKET,
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

/// Create or update a job. `secret` is stored only when present.
#[tauri::command]
async fn save_job(job: Job, secret: Option<String>) -> Result<String, String> {
    let replies = request_all(Request::SaveJob { job, secret }).await?;
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

/// Delete a job and its stored secret.
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

/// Back up a SQL Server database over VDI to a local file, streaming progress.
#[tauri::command]
async fn backup_sql_to_file(
    server: String,
    port: Option<u16>,
    auth: SqlAuth,
    password: Option<String>,
    database: String,
    output_path: String,
    on_event: Channel<Reply>,
) -> Result<(), String> {
    ensure_engine().await?;
    let name = pbsgui_ipc::socket_name(DEFAULT_SOCKET).map_err(|e| e.to_string())?;
    let request = Request::BackupSqlToFile {
        server,
        port,
        auth,
        password,
        database,
        output_path,
    };
    pbsgui_ipc::send_request(name, &request, move |reply| {
        let _ = on_event.send(reply);
    })
    .await
    .map_err(|e| e.to_string())
}

/// Back up a SQL Server database over VDI, streaming it to PBS, with progress.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn backup_sql_to_pbs(
    server: String,
    port: Option<u16>,
    auth: SqlAuth,
    password: Option<String>,
    database: String,
    pbs_job_id: String,
    backup_id: String,
    on_event: Channel<Reply>,
) -> Result<(), String> {
    ensure_engine().await?;
    let name = pbsgui_ipc::socket_name(DEFAULT_SOCKET).map_err(|e| e.to_string())?;
    let request = Request::BackupSqlToPbs {
        server,
        port,
        auth,
        password,
        database,
        pbs_job_id,
        backup_id,
    };
    pbsgui_ipc::send_request(name, &request, move |reply| {
        let _ = on_event.send(reply);
    })
    .await
    .map_err(|e| e.to_string())
}

/// Native save-file picker (for choosing where to write a .bak).
#[tauri::command]
async fn pick_save_file(default_name: String) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        rfd::FileDialog::new()
            .set_file_name(&default_name)
            .save_file()
            .map(|p| p.display().to_string())
    })
    .await
    .ok()
    .flatten()
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

fn first_error(replies: &[Reply]) -> Option<String> {
    replies.iter().find_map(|r| match r {
        Reply::Error { message } => Some(message.clone()),
        _ => None,
    })
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
        list_snapshots,
        list_files,
        restore,
        discover_sql,
        probe_sql,
        check_sql,
        backup_sql_to_file,
        backup_sql_to_pbs,
        pick_save_file,
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
