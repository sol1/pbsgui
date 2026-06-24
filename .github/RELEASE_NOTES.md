pbsgui is a Windows GUI for backing up Windows files and Microsoft SQL Server to a
Proxmox Backup Server (PBS), with browse and restore. It talks to PBS using a
clean-room reimplementation of the PBS backup protocol.

This is an early release. Please test against non-production data first and report
issues.

## New in this release

- **See backups that are still running, even ones you did not start.** Open the
  app and any job with a run in progress shows "Running..." on its card with a live
  progress bar and status line, including a backup the scheduler started in the
  background. Click it to watch progress and Stop it if needed. Previously a
  scheduled backup running in the background was invisible until it finished, so
  there was no way to tell one was still going.
- **Run several backups at once and switch between them.** The run output panel
  keeps a separate live log and progress per run and has a tab bar to swap between
  them, so a manual run, a scheduled one, and a restore can all be watched without
  losing any of their output. A job's Run button reads "Running..." for the life of
  its run and links to that run's output.
- **Clearer status while a backup starts.** Instead of sitting on a generic
  "starting..." while a large database is read, the status shows the current phase
  (connecting to SQL Server, reading a database, archiving files), so the long
  initial read of a big database no longer looks stalled.

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
