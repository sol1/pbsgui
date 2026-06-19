//! The pbsgui desktop application.
//!
//! Unprivileged control/monitor UI. It connects to `pbsgui-engine` over a local
//! socket (a named pipe on Windows), exposes job CRUD/run commands plus a native
//! folder picker, and forwards run progress to the frontend over a channel. It
//! best-effort launches the engine sitting next to it; otherwise the engine must
//! be started separately (`pbsgui-engine serve`).

use std::time::Duration;

use pbsgui_ipc::{FileInfo, Job, Reply, Request, SnapshotInfo, DEFAULT_SOCKET};
use tauri::ipc::Channel;

/// Check (and if needed start) the engine.
#[tauri::command]
async fn engine_ping() -> Result<String, String> {
    ensure_engine().await?;
    Ok("connected".to_string())
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

/// Ensure the engine is reachable: ping it, and if that fails, try to launch the
/// engine next to us and retry.
async fn ensure_engine() -> Result<(), String> {
    if ping_once().await.unwrap_or(false) {
        return Ok(());
    }
    spawn_engine();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if ping_once().await.unwrap_or(false) {
            return Ok(());
        }
    }
    Err(
        "could not reach or start the backup engine (try running `pbsgui-engine serve`)"
            .to_string(),
    )
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

fn spawn_engine() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    for name in [
        "pbsgui-engine.exe",
        "pbsgui-engine",
        "pbsgui-engine-x86_64-pc-windows-msvc.exe",
    ] {
        let candidate = dir.join(name);
        if candidate.exists() {
            let _ = std::process::Command::new(candidate).arg("serve").spawn();
            return;
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            engine_ping,
            list_jobs,
            save_job,
            delete_job,
            run_job,
            list_snapshots,
            list_files,
            restore,
            pick_destination,
            pick_folders,
            pick_files
        ])
        .run(tauri::generate_context!())
        .expect("error while running pbsgui");
}
