pbsgui is a Windows GUI for backing up Windows files and Microsoft SQL Server to a
Proxmox Backup Server (PBS), with browse and restore. It talks to PBS using a
clean-room reimplementation of the PBS backup protocol.

This is an early release. Please test against non-production data first and report
issues.

## New in 0.0.4

- **Point-in-time recovery for SQL Server.** A SQL job now picks what you want to
  be able to restore, not a raw backup type: *point-in-time* (scheduled fulls plus
  frequent log backups, which also truncate the log), *daily restore points* (fulls
  only), or a *secondary copy* (copy-only fulls that coexist with another tool).
  Restore to any moment in the retained window: pbsgui rebuilds the chain from the
  covering full and its log backups, replays it with `STOPAT`, and recovers at the
  chosen second, over the original database or a new name. The wizard reads each
  database's recovery model and explains, in plain language, what each choice can
  restore, blocking point-in-time on a SIMPLE database with the exact
  `ALTER DATABASE` to run.
- **Always On and Failover Cluster aware.** Install the engine on each node and it
  coordinates through SQL Server with no connection between the pbsgui instances and
  no extra ports: an Always On database is backed up only on the preferred backup
  replica (a copy-only full on a secondary), every replica's job shares one
  continuous chain (the snapshot group defaults to the Availability Group name), and
  a Failover Cluster Instance is skipped on whichever node is not active.
- **System databases.** master, model, and msdb can be backed up (tempdb is hidden);
  the wizard explains the restore caveats.
- **Network SQL discovery.** Find SQL Server instances on the LAN (SQL Browser
  broadcast) or by scanning specific hosts and subnets, alongside local discovery.
- **Jobs dashboard.** The jobs view is now a dashboard: a health summary, per-job
  status chips, configuration badges, and a size-over-time graph, with a refreshed
  look throughout the app.
- **Better notifications.** Messages name the backup type and the databases, and a
  new warning (on by default) tells you when a point-in-time chain stops advancing,
  so the transaction log does not grow unnoticed. It is detected from the shared PBS
  storage, so it works across Always On replicas.

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
- A jobs dashboard with status and size-over-time graphs.
- Runs as a Windows service with a tray icon; scheduled jobs run without the GUI
  open.

## Important

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
