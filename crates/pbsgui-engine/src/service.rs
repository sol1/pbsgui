//! Windows Service: install/uninstall and run the engine under the SCM as
//! LocalSystem, so scheduled backups run unattended (surviving logoff and
//! reboot). The GUI is a separate, unprivileged process that connects to it.

#![cfg(windows)]

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::jobstore::JobStore;

const SERVICE_NAME: &str = "pbsgui-engine";
const SERVICE_DISPLAY: &str = "pbsgui backup engine";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// Register the service with the SCM (binary = this exe, args `service run`,
/// auto-start, LocalSystem) and start it. Idempotent.
pub fn install() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let service = match manager.create_service(&info, access) {
        Ok(service) => {
            let _ = service
                .set_description("Runs scheduled pbsgui backups to a Proxmox Backup Server.");
            service
        }
        // Already installed: open it and (re)start below.
        Err(_) => manager.open_service(
            SERVICE_NAME,
            ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )?,
    };
    let _ = service.start::<&str>(&[]); // ignore "already running"
    Ok(())
}

/// Stop and remove the service. Idempotent (no-op if not installed).
pub fn uninstall() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let access = ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS;
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => service,
        Err(_) => return Ok(()),
    };
    let _ = service.stop(); // best effort
                            // Wait for the engine to actually stop so its exe is unlocked (an installer can
                            // then overwrite it during an in-place upgrade). Bounded to ~10s.
    for _ in 0..50 {
        match service.query_status() {
            Ok(status) if status.current_state == ServiceState::Stopped => break,
            Ok(_) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => break,
        }
    }
    service.delete()?;
    Ok(())
}

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Entry point for `pbsgui-engine service run` (invoked by the SCM).
pub fn run() -> anyhow::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service_loop() {
        tracing::error!("service stopped with error: {e}");
    }
}

fn run_service_loop() -> windows_service::Result<()> {
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = shutdown_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Run the engine (scheduler + IPC) until a stop is requested.
    if let Ok(runtime) = tokio::runtime::Runtime::new() {
        runtime.block_on(async move {
            let store = Arc::new(JobStore::load());
            let engine = tokio::spawn(async move {
                let _ = crate::run_engine(store, pbsgui_ipc::DEFAULT_SOCKET).await;
            });
            let _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()).await;
            engine.abort();
        });
    }

    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;
    Ok(())
}
