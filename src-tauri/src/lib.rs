//! The pbsgui desktop application.
//!
//! This is the unprivileged control and monitor UI. It does not perform backups
//! itself: it connects to the `pbsgui-engine` over a local socket (a named pipe
//! on Windows), sends requests, and forwards the engine's progress and log
//! replies to the frontend over a Tauri channel.
//!
//! For now the GUI makes a best effort to launch the engine sitting next to it;
//! if that is not found it assumes the engine was started separately
//! (`pbsgui-engine serve`). A bundled service is a later step.

use std::time::Duration;

use pbsgui_ipc::{
    BackupKind, BackupRequest, PbsDestination, Reply, Request, Target, DEFAULT_SOCKET,
};
use serde::Deserialize;
use tauri::ipc::Channel;

/// Backup form input from the frontend.
#[derive(Debug, Deserialize)]
struct BackupConfig {
    repository: String,
    secret: String,
    fingerprint: String,
    backup_id: String,
    path: String,
}

/// Check (and if needed start) the engine, reporting a simple status string.
#[tauri::command]
async fn engine_ping() -> Result<String, String> {
    ensure_engine().await?;
    Ok("connected".to_string())
}

/// Run a filesystem backup, streaming engine replies to the frontend channel.
#[tauri::command]
async fn run_backup(config: BackupConfig, on_event: Channel<Reply>) -> Result<(), String> {
    ensure_engine().await?;
    let name = pbsgui_ipc::socket_name(DEFAULT_SOCKET).map_err(|e| e.to_string())?;
    let request = Request::StartBackup {
        destination: PbsDestination {
            repository: config.repository,
            secret: config.secret,
            fingerprint: config.fingerprint,
            backup_id: config.backup_id,
        },
        job: BackupRequest {
            target: Target::Filesystem {
                paths: vec![config.path],
            },
            kind: BackupKind::FilesystemFull,
            copy_only: false,
        },
    };
    pbsgui_ipc::send_request(name, &request, move |reply| {
        let _ = on_event.send(reply);
    })
    .await
    .map_err(|e| e.to_string())
}

/// Ensure the engine is reachable: ping it, and if that fails, try to launch the
/// engine binary next to us and retry.
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

/// Best-effort launch of the engine binary sitting next to the app.
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
        .invoke_handler(tauri::generate_handler![engine_ping, run_backup])
        .run(tauri::generate_context!())
        .expect("error while running pbsgui");
}
