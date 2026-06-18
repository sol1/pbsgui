# pbsgui

A Windows GUI for backing up Windows machines and Microsoft SQL Server to a
[Proxmox Backup Server](https://www.proxmox.com/en/products/proxmox-backup-server)
(PBS). The primary goal is SQL Server aware backup and restore (standalone,
Failover Cluster Instances, and Always On Availability Groups), with browse and
point-in-time restore.

> Status: early scaffold. The project structure, protocol design, and SQL backup
> approach are in place; the backup engine is being implemented. It does not back
> up anything yet.

## Why this exists

The official Proxmox backup client is Linux only and its Rust crates do not build
on Windows, so pbsgui talks to PBS through a clean-room Rust implementation of the
PBS backup protocol. Two things set it apart from existing Windows PBS clients:

- SQL Server aware backups (full, differential, and transaction log) that respect
  recovery models and the log chain, instead of generic full-volume snapshots.
- Client side encryption.

## Architecture

```
+-------------------------------+        named pipe        +---------------------------+
|  pbsgui (Tauri desktop app)   | <----------------------> |  pbsgui-engine            |
|  unprivileged control / UI    |  requests, status, logs  |  elevated Windows Service |
+-------------------------------+                          |  (or sidecar)             |
                                                           |   - PBS protocol client   |
                                                           |   - SQL Server VDI        |
                                                           |   - VSS filesystem backup |
                                                           |   - scheduler             |
                                                           +---------------------------+
```

The GUI runs unprivileged and only controls and monitors. The engine runs elevated
(as a Windows Service, so scheduled backups survive logoff and reboot, or as a
sidecar for interactive runs) and does the privileged SQL VDI and VSS work.

SQL backups use the Virtual Device Interface (VDI): a `BACKUP DATABASE/LOG ... TO
VIRTUAL_DEVICE` statement streams SQL's native backup bytes to the engine, which
forwards them to PBS as a fixed-index image, one snapshot per backup operation,
with the LSN metadata needed to drive correct restore ordering.

## Repository layout

```
crates/pbs-client      clean-room Rust client for the PBS backup protocol
crates/pbsgui-engine   the privileged backup engine (PBS + SQL + VSS + IPC + service)
src-tauri              the Tauri desktop GUI
ui                     the frontend (static, no build step yet)
docs                   architecture and design notes
```

## Building

Prerequisites: a recent stable Rust toolchain. The UI is static (no Node build
step). On Windows you also need NASM on `PATH` (the `ring` crypto library needs
it), the WebView2 runtime (preinstalled on current Windows), and the Tauri CLI
(`npm install -g @tauri-apps/cli@^2`).

```sh
# the cross-platform crates (CI checks these on Linux too)
cargo test -p pbs-client -p pbsgui-ipc -p pbsgui-engine

# run the desktop app in development
cargo build -p pbsgui-engine   # so the GUI can launch it from the same directory
tauri dev
```

When the GUI starts it tries to launch the engine sitting next to it. If it is
not found, start it yourself in another terminal:

```sh
cargo run -p pbsgui-engine -- serve
```

Then enter a PBS repository, API token secret, server fingerprint, and a file to
back up, and click "Back up". Progress and logs stream from the engine.

### Building the Windows installer locally

Plain `tauri build` produces a GUI-only installer. To build the full installer
that bundles the engine as a sidecar, the engine has to be staged first; the
helper script does that for you (run it from the repository root):

```bat
scripts\build-windows-installer.bat
```

It builds the engine, copies it into `src-tauri\binaries\` with the target-triple
name Tauri expects, then runs `tauri build --config src-tauri\engine-sidecar.conf.json`.
The installer lands in `target\release\bundle\nsis\`.

## Continuous integration and installer

`.github/workflows/ci.yml` lints and tests the cross-platform crates on Linux,
and on Windows builds the engine, stages it as a Tauri sidecar, and produces an
NSIS installer (uploaded as the `pbsgui-nsis-installer` artifact). The installer
bundles both the GUI and the engine.

Code signing is deferred for now (Azure Trusted Signing is not available to
Australian entities), so installers are unsigned until a signing identity is in
place. Windows will show a SmartScreen warning until then.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
