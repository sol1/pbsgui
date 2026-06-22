pbsgui is a Windows GUI for backing up Windows files and Microsoft SQL Server to a
Proxmox Backup Server (PBS), with browse and restore. It talks to PBS using a
clean-room reimplementation of the PBS backup protocol.

This is an early release. Please test against non-production data first and report
issues.

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
- SQL Server: local discovery, connection probe and readiness checks, full and
  transaction-log backups over the Virtual Device Interface streamed to PBS, and
  restore (over the original database or to a new name).
- Optional client-side AES-256-GCM encryption, byte-compatible with the PBS
  scheme; keys are stored in the Windows Credential Manager.
- Notifications on job success/failure via email (SMTP) and a Slack-compatible
  webhook, each with a Test button.
- Runs as a Windows service with a tray icon; scheduled jobs run without the GUI
  open.

## Important

- **Encryption keys cannot be recovered.** If you lose an encryption key, backups
  made with it cannot be restored by anyone. Copy the key into a password manager
  when it is shown.
- SQL Server backup requires TCP/IP enabled on the instance and the engine's
  service identity (`NT AUTHORITY\SYSTEM`) in the `sysadmin` role. See the docs.
- Point-in-time restore from a log chain is not implemented yet; log backups
  truncate the log and are stored offsite, and restore is from a full snapshot.

See the [documentation](https://github.com/sol1/pbsgui/tree/main/docs) for setup
and details.
