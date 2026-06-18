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

Prerequisites: a recent stable Rust toolchain, Node is not required (the UI is
static for now), and on Windows the WebView2 runtime (preinstalled on current
Windows) plus the Tauri CLI (`cargo install tauri-cli --version '^2'`).

```sh
# the cross-platform crates (CI checks these on Linux too)
cargo test -p pbs-client
cargo build -p pbsgui-engine

# the full desktop app (Windows; needs icons, see src-tauri/icons/README.md)
cargo tauri build
```

Code signing of release binaries is deferred for now; installers will be unsigned
until a signing identity is in place.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
