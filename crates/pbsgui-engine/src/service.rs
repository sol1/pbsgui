//! Windows Service entry point.
//!
//! When installed as a service the engine runs as LocalSystem so it has the
//! privileges needed for VSS and SQL VDI and can perform scheduled, unattended
//! backups that survive logoff and reboot. Service registration and the SCM
//! dispatch loop will use the `windows-service` crate.

#![cfg(windows)]

/// Run the Windows Service. Not yet implemented.
pub fn run() -> anyhow::Result<()> {
    // TODO: register with the SCM, run the dispatcher, and serve the same IPC
    // pipe used in foreground mode plus the scheduler.
    anyhow::bail!("Windows service mode not yet implemented");
}
