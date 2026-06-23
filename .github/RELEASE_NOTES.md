pbsgui is a Windows GUI for backing up Windows files and Microsoft SQL Server to a
Proxmox Backup Server (PBS), with browse and restore. It talks to PBS using a
clean-room reimplementation of the PBS backup protocol.

This is an early release. Please test against non-production data first and report
issues.

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

- **Run the app as an administrator.** As of 0.0.5 the engine's control socket is
  restricted to administrators (a security fix), so the GUI must be run by an
  administrator to manage and run backups. The background service itself still runs
  as `LocalSystem` and keeps running scheduled jobs regardless.
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
