//! pbsgui Active Directory backup engine.
//!
//! A lean, SQL-free sibling of `pbsgui-engine` that backs up a Windows Domain
//! Controller to a Proxmox Backup Server: a VSS System State capture (the AD
//! database `ntds.dit`, SYSVOL, registry, and boot/COM+ files), an offline browser
//! over a backed-up `ntds.dit`, and partial restore of directory objects back to
//! live AD. It runs elevated as its own Windows service with its own profile (see
//! [`profile`]), separate from the SQL/files engine, so a Domain Controller never
//! has to carry the SQL client code.
//!
//! Modes:
//!   - `serve` runs the engine in the foreground (for development).
//!   - `service install|uninstall|run` manages/runs the Windows Service.
//!   - the dev commands `backup` / `browse` / `restore` drive the capture, the
//!     `ntds.dit` reader, and restore directly while those are built out.

mod capture;
mod dit;
mod profile;
mod restore;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "pbsgui-ad-engine",
    version,
    about = "pbsgui Active Directory backup engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine in the foreground (for development).
    Serve {
        /// Socket base name the GUI connects to.
        #[arg(long, default_value_t = profile::SOCKET.to_string())]
        socket: String,
    },
    /// Manage or run the Windows Service.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Back up this Domain Controller's System State (dev entry point).
    Backup,
    /// Browse a backed-up ntds.dit (dev entry point).
    Browse,
    /// Restore directory objects from a backup (dev entry point).
    Restore,
    /// Print version and platform information.
    Version,
}

#[derive(Subcommand, Clone, Copy)]
enum ServiceAction {
    /// Register the service and start it (run elevated).
    Install,
    /// Stop and remove the service (run elevated).
    Uninstall,
    /// Run as the service (invoked by the Service Control Manager).
    Run,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    match Cli::parse().command {
        Command::Serve { socket } => {
            // The IPC server is wired in M2; for now this just reports the identity
            // so the profile constants have a home and the skeleton is runnable.
            tracing::info!(
                service = profile::SERVICE_NAME,
                socket,
                config_subdir = profile::CONFIG_SUBDIR,
                secrets_service = profile::KEYRING_SERVICE,
                "AD engine skeleton: IPC not wired yet (see M2)"
            );
            Ok(())
        }
        Command::Service { action } => run_service(action),
        Command::Backup => capture::run_system_state_backup(),
        Command::Browse => dit::browse(),
        Command::Restore => restore::run(),
        Command::Version => {
            println!(
                "pbsgui-ad-engine ({}) {} build {} ({})",
                profile::SERVICE_DISPLAY,
                env!("CARGO_PKG_VERSION"),
                option_env!("PBSGUI_BUILD").unwrap_or("dev"),
                std::env::consts::OS
            );
            Ok(())
        }
    }
}

#[cfg(windows)]
fn run_service(action: ServiceAction) -> anyhow::Result<()> {
    // TODO(M2): install/run as the `pbsgui-ad-engine` Windows service, mirroring
    // pbsgui-engine's service wrapper but bound to this crate's own identity.
    let _ = action;
    anyhow::bail!("the AD engine Windows service is not implemented yet")
}

#[cfg(not(windows))]
fn run_service(_action: ServiceAction) -> anyhow::Result<()> {
    anyhow::bail!("service mode is only available on Windows")
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("PBSGUI_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
