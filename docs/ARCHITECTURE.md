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
- the backup relay (proxy and agent roles, see below),
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
- Always On Availability Group: on a secondary (before SQL 2025) only COPY_ONLY
  full and regular log backups are allowed, never differentials, and a COPY_ONLY
  full does not start a log chain. So point-in-time fulls run on the primary, and
  log backups on the replica `sys.fn_hadr_backup_is_preferred_replica` selects.
  The rules are in `sql/plan.rs`; operator guidance in [ALWAYS-ON.md](ALWAYS-ON.md).

Backups stream over the Virtual Device Interface. A `BACKUP ... TO VIRTUAL_DEVICE`
statement is issued through a TDS client while a native COM loop on `SQLVDI.dll`
reads SQL's backup buffers and forwards them to PBS. The VDI connection must be a
member of the `sysadmin` role.

Transaction log management: for FULL and BULK_LOGGED databases the engine takes
regular non copy-only log backups to truncate the log, keep it bounded, and
provide point-in-time recovery. The log chain must not be broken.

## Remote backup relay

The CPU-heavy part of a backup is on the client: chunking, hashing, compression,
and encryption of every chunk. In local mode that runs on the SQL Server host. VDI
is local-only, so the backup device must stay on the SQL host, but nothing else
must.

Relay mode splits the work there. One engine binary has two roles. An agent on the
SQL host runs only the VDI device. It streams the raw backup bytes over a TLS link
to a proxy. The proxy runs the job: it issues BACKUP and RESTORE over TDS, does the
chunk/compress/encrypt/upload work, and talks to PBS.

The agent dials out to the proxy. The proxy pins nothing new for the operator: the
agent pins the proxy certificate by fingerprint and authenticates with a token, the
same trust model pbsgui uses toward PBS. The agent holds no SQL or PBS credentials.

The wire protocol frames a control stream and a per-session data stream. A backup
data stream ends with a verdict frame, so the proxy never commits a snapshot from a
stream the agent did not finish. Restore reverses the flow: the proxy streams the
image to the agent, which fills the device. See [RELAY.md](RELAY.md) for the
deployment and the credential model.

## Storage model

Each SQL backup operation becomes one PBS snapshot per database. The VDI byte
stream is stored as a deduplicated dynamic-index archive (a fixed index needs the
size up front, which a VDI stream does not provide). Full and log backups use
separate snapshot groups so they do not interleave. Relay backups use the same
storage layout; only the path the bytes take to the proxy differs.

Point-in-time restore is implemented. Each snapshot stores its backup metadata
(backup type, recovery model, LSN range, and the database file list). pbsgui picks
the covering full and its log chain, restores the full `WITH NORECOVERY`, replays
the logs `WITH STOPAT`, and recovers at the chosen second. It can also restore a
chosen full. Differential backups are not supported yet.

## IPC

The GUI and engine exchange newline-delimited JSON over a named pipe. The GUI
sends requests and receives an immediate response; long running jobs then stream
progress and log events. The message types and transport live in
`crates/pbsgui-ipc`; the engine's request handler is
`crates/pbsgui-engine/src/handler.rs`.
