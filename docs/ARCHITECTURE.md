# Architecture

## Components

### pbsgui (Tauri desktop app, `src-tauri` + `ui`)
The user facing control and monitor application. It runs unprivileged
(`asInvoker`) and performs no backup work itself. It connects to the engine over a
Windows named pipe, sends requests, and renders the progress and log events the
engine streams back. The frontend uses the global Tauri API for now so there is no
JavaScript build step; a bundler and framework can be added later without touching
the Rust side.

### pbsgui-engine (`crates/pbsgui-engine`)
The privileged backup engine. It runs elevated, either as a Windows Service
(LocalSystem, for scheduled and unattended backups that survive logoff and reboot)
or as a sidecar launched by the GUI for interactive runs. It owns:

- the PBS protocol client (via `pbs-client`),
- SQL Server topology detection and VDI streaming backup/restore,
- VSS based filesystem backup,
- the scheduler,
- the IPC server.

Running privileged work in a separate process keeps the webview bearing GUI
unprivileged and gives a clean process boundary.

### pbs-client (`crates/pbs-client`)
A clean-room Rust implementation of the PBS backup protocol, written from the
documented protocol rather than the AGPL Proxmox client code. It handles the
HTTP/2 upgrade backup session, fixed and dynamic indexes, blobs, content-defined
chunking, server fingerprint pinning, and AES-256-GCM client side encryption.

## SQL Server backup

Detection runs first and selects the strategy:

- Standalone: full, differential, and log backups on the instance.
- Failover Cluster Instance: backups run against the virtual network name; the
  physical node may change on failover.
- Always On Availability Group: honor the backup preference and
  `sys.fn_hadr_backup_is_preferred_replica`. On a secondary (before SQL 2025) only
  COPY_ONLY full and regular log backups are allowed, never differentials.

Backups stream over the Virtual Device Interface. A `BACKUP ... TO VIRTUAL_DEVICE`
statement is issued through a TDS client while a native COM loop on `SQLVDI.dll`
reads SQL's backup buffers and forwards them to PBS. The VDI connection must be a
member of the `sysadmin` role.

Transaction log management: for FULL and BULK_LOGGED databases the engine takes
regular non copy-only log backups to truncate the log, keep it bounded, and
provide point-in-time recovery. The log chain must not be broken.

## Storage model

Each SQL backup operation becomes one PBS snapshot per database. The VDI byte
stream is stored as a deduplicated dynamic-index archive (a fixed index needs the
size up front, which a VDI stream does not provide). Full and log backups use
separate snapshot groups so they do not interleave. Restore today reassembles a
full snapshot and streams it back over VDI. Point-in-time restore from a full
plus a differential and a log chain, driven by per-snapshot metadata (backup
type, recovery model, LSNs, and the instance / database / cluster identity), is
the intended direction and not yet implemented.

## IPC

The GUI and engine exchange newline-delimited JSON over a named pipe. The GUI
sends requests and receives an immediate response; long running jobs then stream
progress and log events. The message types and transport live in
`crates/pbsgui-ipc`; the engine's request handler is
`crates/pbsgui-engine/src/handler.rs`.
