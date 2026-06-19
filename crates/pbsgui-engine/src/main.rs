//! pbsgui backup engine.
//!
//! Does the backup work: archives a job's sources and streams them to a Proxmox
//! Backup Server (deduplicated) using the clean-room [`pbs_client`] crate, and
//! (in future) talks to SQL Server over VDI and snapshots volumes via VSS. The
//! unprivileged GUI drives it over a local socket (a named pipe on Windows); see
//! [`pbsgui_ipc`]. A built-in scheduler runs due jobs while the engine is up.
//!
//! Run `pbsgui-engine serve` to listen for the GUI. On Windows it will also be
//! installable as a Service for scheduled, elevated backups (not yet built).

// Temporary while the engine is scaffolded: the SQL module defines topology
// detection ahead of the code that will use it.
#![allow(dead_code)]

mod archive;
mod backup;
mod changedet;
mod config;
mod handler;
mod jobstore;
mod restore;
mod scheduler;
mod scripts;
mod secrets;
#[cfg(windows)]
mod service;
mod sql;

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
    /// Serve the IPC socket so the GUI can drive backups, and run the scheduler.
    Serve {
        /// Socket base name the GUI connects to.
        #[arg(long, default_value_t = pbsgui_ipc::DEFAULT_SOCKET.to_string())]
        socket: String,
    },
    /// Run as a Windows Service (registered separately).
    Service,
    /// Print version and platform information.
    Version,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    match Cli::parse().command {
        Command::Serve { socket } => serve(&socket).await,
        Command::Service => run_service(),
        Command::Version => {
            println!(
                "pbsgui-engine {} ({})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS
            );
            Ok(())
        }
    }
}

async fn serve(socket: &str) -> anyhow::Result<()> {
    let store = Arc::new(JobStore::load());
    tracing::info!(socket, "engine serving IPC");

    // Scheduler: run due jobs while we are up.
    let scheduler_store = store.clone();
    tokio::spawn(async move { scheduler::run(scheduler_store).await });

    let name = pbsgui_ipc::socket_name(socket)?;
    pbsgui_ipc::serve(name, move |request, responder| {
        let store = store.clone();
        async move { handler::handle(store, request, responder).await }
    })
    .await?;
    Ok(())
}

#[cfg(windows)]
fn run_service() -> anyhow::Result<()> {
    service::run()
}

#[cfg(not(windows))]
fn run_service() -> anyhow::Result<()> {
    anyhow::bail!("service mode is only available on Windows");
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("PBSGUI_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
