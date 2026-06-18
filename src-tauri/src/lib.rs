//! The pbsgui desktop application.
//!
//! This is the unprivileged control and monitor UI. It does not perform backups
//! itself: it connects to the elevated `pbsgui-engine` (a Windows Service, or a
//! sidecar for interactive use) over a named pipe, sends requests, and renders
//! the progress and log events the engine streams back.

/// Placeholder command: reports the engine connection state to the frontend.
///
/// Replace with a real query over the IPC pipe once the engine server lands.
#[tauri::command]
fn engine_status() -> String {
    "disconnected".to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![engine_status])
        .run(tauri::generate_context!())
        .expect("error while running pbsgui");
}
