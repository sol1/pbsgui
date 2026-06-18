//! pbsgui backup engine.
//!
//! This binary does the privileged work: it talks to SQL Server over the Virtual
//! Device Interface (VDI), snapshots volumes via VSS, and streams data to a
//! Proxmox Backup Server using the clean-room [`pbs_client`] protocol crate. It
//! is intended to run elevated, either as a Windows Service (for scheduled and
//! unattended backups) or as a sidecar launched by the GUI for interactive runs.
//!
//! The unprivileged GUI controls and monitors the engine over a named pipe; see
//! [`ipc`] for the message protocol.

// Temporary while the engine is scaffolded: the IPC, job, SQL, and PBS modules
// define the shape of the engine ahead of the code that will use them. Remove
// once the engine is wired up.
#![allow(dead_code)]

mod fsbackup;
mod ipc;
mod jobs;
mod pbs;
#[cfg(windows)]
mod service;
mod sql;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pbsgui-engine", version, about = "pbsgui backup engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine in the foreground, serving the IPC pipe (used as a sidecar).
    Run {
        /// Named pipe to listen on.
        #[arg(long, default_value_t = ipc::DEFAULT_PIPE_NAME.to_string())]
        pipe: String,
    },
    /// Run as a Windows Service (the service must be registered separately).
    Service,
    /// Print version and platform information.
    Version,
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    match Cli::parse().command {
        Command::Run { pipe } => run_foreground(&pipe),
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

fn run_foreground(pipe: &str) -> anyhow::Result<()> {
    tracing::info!(pipe, "engine starting in foreground mode");
    // TODO: start the IPC server on `pipe` and dispatch jobs. See ipc.rs.
    anyhow::bail!("foreground IPC server not yet implemented");
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
