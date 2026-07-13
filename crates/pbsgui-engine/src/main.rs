//! pbsgui backup engine.
//!
//! Does the backup work: archives a job's sources and streams them to a Proxmox
//! Backup Server (deduplicated) using the clean-room [`pbs_client`] crate, and
//! serves the GUI over a local socket (a named pipe on Windows; see
//! [`pbsgui_ipc`]). A built-in scheduler runs due jobs.
//!
//! Modes:
//!   - `serve` runs the engine in the foreground (for development).
//!   - `service install|uninstall|run` manages/runs the Windows Service, so
//!     scheduled backups run unattended whether or not the GUI is open.

mod archive;
mod backup;
mod changedet;
mod connstore;
mod enckey;
mod handler;
mod jobstore;
mod metrics;
mod notify;
mod restore;
mod scheduler;
#[cfg(windows)]
mod service;
mod sql;
mod sqlsched;

// The config directory, OS secret store, and signed on-disk stores live in the
// shared core crate now, parameterized by a per-app profile. Re-export them under
// the old `crate::` paths so the rest of the engine is unchanged; this engine uses
// the default profile (its own identity), so behavior is identical.
pub(crate) use pbsgui_core::{config, secrets, signed};

use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::jobstore::JobStore;

#[derive(Parser)]
#[command(name = "pbsgui-engine", version, about = "pbsgui backup engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine in the foreground (for development).
    Serve {
        /// Socket base name the GUI connects to.
        #[arg(long, default_value_t = pbsgui_ipc::DEFAULT_SOCKET.to_string())]
        socket: String,
    },
    /// Manage or run the Windows Service.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
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
    apply_worker_limit();
    match Cli::parse().command {
        Command::Serve { socket } => {
            config::ensure_dirs();
            let store = Arc::new(JobStore::load());
            tracing::info!(socket, "engine serving IPC");
            run_engine(store, &socket).await
        }
        Command::Service { action } => run_service(action),
        Command::Version => {
            println!(
                "pbsgui-engine {} build {} ({})",
                env!("CARGO_PKG_VERSION"),
                option_env!("PBSGUI_BUILD").unwrap_or("dev"),
                std::env::consts::OS
            );
            Ok(())
        }
    }
}

/// Run the scheduler and the IPC server until the process stops.
pub(crate) async fn run_engine(store: Arc<JobStore>, socket: &str) -> anyhow::Result<()> {
    let scheduler_store = store.clone();
    tokio::spawn(async move { scheduler::run(scheduler_store).await });

    // Start the metrics exporter if it is configured (off by default).
    metrics::apply(store.clone());

    let name = pbsgui_ipc::socket_name(socket)?;
    pbsgui_ipc::serve(name, move |request, responder| {
        let store = store.clone();
        async move { handler::handle(store, request, responder).await }
    })
    .await?;
    Ok(())
}

#[cfg(windows)]
fn run_service(action: ServiceAction) -> anyhow::Result<()> {
    match action {
        ServiceAction::Install => service::install(),
        ServiceAction::Uninstall => service::uninstall(),
        ServiceAction::Run => service::run(),
    }
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

/// Backup and restore compress/decompress and encrypt/decrypt each chunk on a
/// blocking thread; by default the client runs half the machine's cores in
/// parallel so a run leaves CPU headroom for a live database server. Set
/// `PBSGUI_WORKER_LIMIT` to a positive integer to override (e.g. widen it for a
/// dedicated backup window).
fn apply_worker_limit() {
    if let Ok(v) = std::env::var("PBSGUI_WORKER_LIMIT") {
        match v.trim().parse::<usize>() {
            Ok(n) if n > 0 => {
                pbs_client::set_pipeline_width(n);
                tracing::info!("backup/restore worker limit set to {n} (PBSGUI_WORKER_LIMIT)");
            }
            _ => tracing::warn!(
                "ignoring invalid PBSGUI_WORKER_LIMIT={v:?} (want a positive integer)"
            ),
        }
    }
}
