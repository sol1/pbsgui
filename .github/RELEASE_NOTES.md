pbsgui is a Windows GUI for backing up Windows files and Microsoft SQL Server to a
Proxmox Backup Server (PBS), with browse and restore. It talks to PBS using a
clean-room reimplementation of the PBS backup protocol.

This is an early release. Please test against non-production data first and report
issues.

## New in 0.0.6

- **Backup compression.** Backups are now compressed with zstd before upload, so
  compressible databases and files take much less space on PBS and less bandwidth
  to send. It is on by default and configurable per job; data that does not shrink
  (already-compressed or high-entropy content) is stored as-is, so compression
  never inflates a backup. It is a pure-Rust implementation, so there is no extra
  runtime to install, and the format stays compatible with the PBS client.
- **Live backup progress and stats.** A running backup now shows real-time
  throughput, how much was deduplicated, the live compression ratio, and - for SQL
  Server - a percentage against the database size. The same figures are exported to
  the optional Prometheus endpoint (current bytes and throughput while running,
  plus per-run compression and dedup stats).
- **Stop a running backup.** A new Stop button cancels an in-flight backup cleanly:
  the partial snapshot is discarded on the server and the SQL Server BACKUP is
  aborted. A job can also no longer run twice at once, so a manual run cannot
  collide with a scheduled one.
- **Reliability fixes.**
  - A backup that had actually succeeded could be reported as FAILED when PBS
    closed the connection at the very end (right after committing the snapshot). It
    is now reported correctly as a success.
  - A failed (or wrongly-failed) backup could be re-run by the scheduler over and
    over, sending a flood of failure notifications. After a failure the scheduler
    now waits for the next scheduled slot, so one failure is one notification.
- **Token paste fix.** A PBS API token id or secret pasted with a trailing space or
  newline is now trimmed before use, so a token that "isn't working" only because
  of invisible whitespace is accepted. This also repairs already-saved tokens.
- **Fix: "backup service is not reachable" after upgrading to 0.0.5.** 0.0.5
  restricted the engine's control socket to administrators (a security fix), but a
  normally launched GUI runs with a UAC-filtered token and could not satisfy that,
  so it could not connect. The GUI now ships with a manifest that requests
  elevation, so Windows prompts for admin rights at launch and it connects again.
  You no longer need to right-click "Run as administrator" - just accept the UAC
  prompt. The background service keeps running scheduled jobs regardless.

## New in 0.0.5

- **PBS server validation.** A saved PBS server is now checked the moment you add
  it: reachability, the pinned TLS fingerprint, that the token authenticates, and
  that it actually holds `Datastore.Backup` on the datastore (and namespace). The
  server's indicator turns green only once it passes, and a Test button reports the
  precise reason on failure (unreachable, fingerprint mismatch, bad token, or
  missing DatastoreBackup) so you catch permission problems at setup, not at 2am.
- **Clean in-place upgrades.** Installing a new version over an old one now stops
  and restarts the background service cleanly, and your configuration (jobs,
  connections, servers, notification and metrics settings) and stored secrets are
  preserved automatically. No need to uninstall first.
- **Prometheus metrics (optional).** The engine can export per-job and per-database
  metrics (last run, result, duration, size, dedup counts, and the point-in-time
  chain freshness and stall state) either over an HTTP `/metrics` endpoint or as a
  `pbsgui.prom` textfile for a node/windows_exporter collector. Off by default,
  bound to localhost, and the metrics carry no secrets.
- **Clearer PBS errors.** A permission or credential failure when starting a backup
  now explains itself (for example, that the token lacks DatastoreBackup on the
  datastore, or that another owner holds the backup group) instead of showing a raw
  status code.
- **Hardening (security review).** The engine's control socket is now restricted to
  administrators, so only an administrator can drive backups (see *Important*
  below). Several internal cleanups removed dead code paths.

Removed: the one-off per-database "To PBS" / "To file" buttons on the SQL Servers
tab. Back up SQL Server through a job (a saved connection plus a protection plan),
which uses your saved credentials, can encrypt, and is browsable and restorable.

## Install

Two installers are attached; pick one:

- **`pbsgui_<ver>_x64-setup.exe`** (full, ~200 MB) - bundles the WebView2 runtime
  and installs with no internet access. Use this for air-gapped servers.
- **`pbsgui_<ver>_x64-setup-online.exe`** (small) - downloads the WebView2 runtime
  at install time if it is missing (most Windows 11 and updated Windows 10 already
  have it). Use this when the machine has internet; it is a much smaller download.

1. Download and run one of the installers.
2. The installer is unsigned, so Windows SmartScreen may warn: choose
   **More info -> Run anyway**.
3. The installer registers and starts the `pbsgui-engine` background service, then
   opens the app. Add a PBS server and (optionally) a SQL Server connection, then
   create a job.

## What works

- File and folder backup to PBS with content-defined chunking and incremental
  deduplication; browse and restore (full or selected files).
- SQL Server: local and network discovery, connection probe and readiness checks,
  outcome-driven protection plans with point-in-time recovery (fulls plus log
  backups streamed over the Virtual Device Interface), and restore to a moment in
  time or a chosen full, over the original database or a new name. Always On and
  Failover Cluster aware; system databases supported.
- Optional client-side AES-256-GCM encryption, byte-compatible with the PBS scheme;
  keys are stored in the Windows Credential Manager.
- Notifications on job success, failure, and a stalled point-in-time chain, via
  email (SMTP) and a Slack-compatible webhook, each with a Test button.
- A jobs dashboard with status and size-over-time graphs; PBS servers are validated
  (reachable, fingerprint, token, DatastoreBackup) with a health indicator.
- Optional Prometheus metrics (an HTTP `/metrics` endpoint or a textfile), off by
  default.
- Runs as a Windows service with a tray icon; scheduled jobs run without the GUI
  open.

## Important

- **The app prompts for elevation (UAC) at launch.** It manages backups through the
  engine, which runs as `LocalSystem`, and the engine's control socket is restricted
  to administrators (a security fix); the GUI requests admin rights so it can
  connect, so accept the prompt. The background service runs scheduled jobs
  regardless of whether the GUI is open.
- **Upgrading from an earlier version:** just run the new installer over the old
  one. It stops and restarts the service for you, and your jobs, connections,
  servers, settings, and secrets are kept.
- **Encryption keys cannot be recovered.** If you lose an encryption key, backups
  made with it cannot be restored by anyone. The key is shown only once, when you
  create or import it, so copy it into a password manager then.
- SQL Server backup requires TCP/IP enabled on the instance and the engine's service
  identity (`NT AUTHORITY\SYSTEM`) in the `sysadmin` role. See the docs.
- **Always On:** install pbsgui on every replica and give each replica's job the
  same backup id (it defaults to the Availability Group name) so they share one
  continuous chain. Exactly one node backs up at a time, and it follows failover
  automatically.

See the [documentation](https://github.com/sol1/pbsgui/tree/main/docs) for setup
and details.
