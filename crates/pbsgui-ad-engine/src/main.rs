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

mod adproto;
mod capture;
mod dit;
mod matchspec;
mod profile;
mod restore;
#[cfg_attr(not(windows), allow(dead_code))]
mod stream;
#[cfg(windows)]
mod vss;

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
    /// Back up this Domain Controller's System State (dev entry point). Without
    /// --repo it is a capture-only smoke test; with it the System State streams
    /// to PBS (token secret from PBSGUI_AD_PBS_SECRET or PBS_PASSWORD).
    Backup {
        /// PBS repository, e.g. `user@pbs!token@server:datastore`.
        #[arg(long)]
        repo: Option<String>,
        /// PBS server certificate fingerprint (SHA-256; required with --repo).
        #[arg(long, default_value = "")]
        fingerprint: String,
        /// PBS namespace.
        #[arg(long)]
        namespace: Option<String>,
        /// Backup group id (default: `<hostname>-ad`).
        #[arg(long)]
        backup_id: Option<String>,
        /// Disable zstd compression of uploaded chunks.
        #[arg(long)]
        no_compress: bool,
    },
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
    // Bind this process to its own on-disk config directory and credential-store
    // namespace, so it never collides with the SQL/files engine on the same host.
    pbsgui_core::set_profile(pbsgui_core::Profile {
        config_subdir: profile::CONFIG_SUBDIR,
        keyring_service: profile::KEYRING_SERVICE,
    });
    match Cli::parse().command {
        Command::Serve { socket } => serve(&socket).await,
        Command::Service { action } => run_service(action),
        Command::Backup {
            repo,
            fingerprint,
            namespace,
            backup_id,
            no_compress,
        } => {
            if repo.is_some() && fingerprint.is_empty() {
                anyhow::bail!("--fingerprint is required with --repo");
            }
            capture::run_system_state_backup(capture::BackupOptions {
                repository: repo,
                fingerprint,
                namespace,
                backup_id,
                compress: !no_compress,
            })
        }
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

/// Serve the AD engine's IPC protocol on its own socket until the process stops.
async fn serve(socket: &str) -> anyhow::Result<()> {
    let name = pbsgui_ipc::socket_name(socket)?;
    tracing::info!(
        service = profile::SERVICE_NAME,
        socket,
        "AD engine serving IPC"
    );
    pbsgui_ipc::serve_typed::<adproto::AdRequest, adproto::AdReply, _, _>(
        name,
        |request, responder| async move { handle(request, responder).await },
    )
    .await?;
    Ok(())
}

/// Handle one AD IPC request. Small for now (see [`adproto`]); it grows with the
/// job, browse, and restore capabilities.
async fn handle(
    request: adproto::AdRequest,
    mut responder: pbsgui_ipc::Responder<adproto::AdReply>,
) {
    use adproto::{AdReply, AdRequest};
    let reply = match request {
        AdRequest::Ping => AdReply::Pong,
        AdRequest::Version => AdReply::Version {
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    };
    let _ = responder.send(&reply).await;
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
