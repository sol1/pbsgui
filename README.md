# pbsgui

**Back up Windows and Microsoft SQL Server to a
[Proxmox Backup Server](https://www.proxmox.com/en/products/proxmox-backup-server)
with a native Windows app, and restore to any point in time.**

pbsgui talks to PBS through a clean-room reimplementation of the PBS backup
protocol, so it runs natively on Windows without the Linux-only official client.
It is built for SQL Server: standalone instances, Failover Cluster Instances, and
Always On Availability Groups, with full and transaction-log chains and restore to
a moment in time.

![Jobs dashboard](docs/screenshots/pbsgui-joblist.png)

> Early release. Test against non-production data first, and please report issues.

## Features

- **SQL Server backup, done right.** Stream backups straight to PBS over the
  Virtual Device Interface, with no on-disk staging. Pick what you want to be able
  to restore, not a raw backup type: **point-in-time recovery** (scheduled fulls
  plus frequent log backups that also truncate the log), **daily restore points**,
  or a **secondary copy** (copy-only fulls that coexist with another backup tool).
- **Restore to the second.** Recover a database to any moment in the retained
  window (the covering full plus its log chain, replayed with `STOPAT`), or to a
  chosen full, over the original name or a new one.
- **Always On and Failover Cluster aware.** Install on every node; it coordinates
  through SQL Server with no link between the pbsgui instances. Exactly one node
  backs up at a time and it follows failover automatically.
- **File and folder backup** with content-defined chunking and incremental
  deduplication, so repeat backups are fast and small. Browse snapshots and restore
  in full or by selected files.
- **Compression and encryption.** zstd compression before upload (on by default,
  never inflates incompressible data) and optional client-side AES-256-GCM, byte
  compatible with the PBS scheme; keys live in the Windows Credential Manager and
  never reach the server.
- **Runs unattended as a Windows service.** Scheduled jobs run with the GUI closed
  and across logoff and reboot. A tray icon and a jobs dashboard show status and
  size-over-time, and live progress lets you see, and stop, a backup that is still
  running, including one the scheduler started in the background.
- **Notifications** on success, failure, and a stalled point-in-time chain, via
  email (SMTP) and a Slack-compatible webhook.
- **Optional Prometheus metrics** (an HTTP `/metrics` endpoint or a textfile), off
  by default and secret-free.

See [docs/STATUS.md](docs/STATUS.md) for the full list of what works, what is in
progress, and the roadmap.

## Requirements

- **Proxmox Backup Server 4.2 or newer** (older 3.x servers reject the backup at
  the protocol upgrade and are not supported).
- **Windows** with the WebView2 runtime (preinstalled on current Windows; the full
  installer can bundle it).
- For SQL Server: **TCP/IP enabled** on the instance, and the engine's service
  identity (`NT AUTHORITY\SYSTEM`) in the **`sysadmin`** server role. pbsgui detects
  and flags a disabled TCP/IP during discovery and tells you the fix.

## Install

Download an installer from the
[latest release](https://github.com/sol1/pbsgui/releases/latest). Two are attached;
pick one:

- **`pbsgui_<ver>_x64-setup.exe`** (full, ~200 MB) bundles the WebView2 runtime and
  installs with no internet access. Use this for air-gapped servers.
- **`pbsgui_<ver>_x64-setup-online.exe`** (small) downloads the WebView2 runtime at
  install time if it is missing. Use this when the machine has internet.

1. Download and run one of the installers.
2. The installer is unsigned, so Windows SmartScreen may warn: choose
   **More info -> Run anyway**.
3. The installer registers and starts the `pbsgui-engine` background service and
   opens the app. Add a PBS server and (optionally) a SQL Server connection, then
   create a job.

To upgrade, run the new installer over the old one: it restarts the service for you
and keeps your jobs, connections, servers, settings, and secrets.

## How it works

pbsgui is two processes. The **GUI** runs unprivileged and only configures and
monitors, so closing it never stops a backup. The **engine** does the privileged
work (the PBS protocol, the scheduler, SQL Server, secret storage) and runs as a
LocalSystem Windows service, so scheduled jobs run unattended. The GUI requests
admin rights at launch (a UAC prompt) so it can connect to the engine's
administrator-only control socket. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Good to know

- **The app prompts for elevation (UAC) at launch** so the GUI can reach the
  engine; accept the prompt. The service runs scheduled jobs whether or not the GUI
  is open.
- **Encryption keys cannot be recovered.** If you lose a key, backups made with it
  cannot be restored, by anyone. The key is shown only once, when you create or
  import it, so copy it into a password manager then.
- **Always On:** give each replica's job the same backup id (it defaults to the
  Availability Group name) so they share one continuous chain.

## Documentation

- [docs/](docs/README.md) - overview, screenshots, and the guides below.
- [STATUS.md](docs/STATUS.md) - what works, what is in progress, and planned.
- [ARCHITECTURE.md](docs/ARCHITECTURE.md) - components and the SQL backup strategy.
- [DEVELOPERS.md](docs/DEVELOPERS.md) - build from source, run, and test.
- [TESTING.md](docs/TESTING.md) - the test tiers and the manual integration pass.

## Building from source

pbsgui is a Cargo workspace with a static front end. See
[docs/DEVELOPERS.md](docs/DEVELOPERS.md) for prerequisites, building the app and
installer, and the development loop.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
