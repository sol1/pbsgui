pbsgui is a Windows GUI for backing up Windows files and Microsoft SQL Server to a
Proxmox Backup Server (PBS), with browse and restore. It talks to PBS using a
clean-room reimplementation of the PBS backup protocol.

This is an early release. Please test against non-production data first and report
issues.

## New in 0.0.3

- **PBS namespaces.** A repository can now target a namespace within a datastore,
  written as `host:8007:datastore/namespace` (nested paths work too). Previously
  the namespace was folded into the datastore name and PBS rejected the backup
  with a 400 error. Backup, browse, and restore all honor the namespace.
- **Encryption key is shown only once.** The key now appears just when you create
  or import it; the "Show key" button that re-revealed the stored key in plaintext
  has been removed, and the engine returns only the fingerprint afterward. The key
  stays in the Windows Credential Manager so restores still decrypt transparently;
  reuse it on another machine by importing it from your password manager.
- **SQL transaction-log backups** (from 0.0.2): a Log backup type takes and
  truncates the transaction log so FULL/BULK_LOGGED databases do not grow forever.
- **Notifications** (from 0.0.2): email (SMTP) and a Slack-compatible webhook on
  job success/failure, with Test buttons.
- **Smaller installer** (from 0.0.2): a ~5 MB online installer that downloads
  WebView2, alongside the full offline one.

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
  made with it cannot be restored by anyone. The key is shown only once, when you
  create or import it, so copy it into a password manager then.
- SQL Server backup requires TCP/IP enabled on the instance and the engine's
  service identity (`NT AUTHORITY\SYSTEM`) in the `sysadmin` role. See the docs.
- Point-in-time restore from a log chain is not implemented yet; log backups
  truncate the log and are stored offsite, and restore is from a full snapshot.

See the [documentation](https://github.com/sol1/pbsgui/tree/main/docs) for setup
and details.
