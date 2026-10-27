pbsgui backs up Windows files and Microsoft SQL Server to a Proxmox Backup Server
(PBS), with browse and point-in-time restore.

## Testing release

This is a **pre-release for testing** and has not yet been validated end to end on
real hardware. If you want the last tested, stable build, use **v0.2.0**:
https://github.com/sol1/pbsgui/releases/tag/v0.2.0

Please try this build in a non-production environment and report what you find.

## New in this release

- **Always On availability groups: log backups no longer get stuck.** With
  point-in-time protection, pbsgui took full backups on the preferred backup
  replica, which SQL Server sets to a secondary by default. A secondary can only
  take copy-only full backups, and those do not start the log chain, so log
  backups could fail with error 4214 indefinitely and the log never truncated
  (`log_reuse_wait_desc = LOG_BACKUP`). Now:
  - Full backups run on the **primary**, as normal full backups.
  - Log backups run on the **preferred backup replica**, as before.
  - When a database has no full backup to start its chain (newly added, or after
    a recovery model change), the primary takes a full at its next log interval,
    even if it is not the preferred replica. A secondary that cannot take its log
    backup for that reason reports a warning, and fails the run if that lasts
    over an hour, so a missing primary setup does not go unnoticed.
  - In a Basic availability group (Standard edition), which allows no backups on
    a secondary, everything runs on the primary.
  - The replica's role is read from SQL Server directly, so a non-readable
    secondary is no longer mistaken for the primary.
- **Log backups start on the first interval.** A new point-in-time job no longer
  waits for its first scheduled full: its first log interval takes a full, then
  log backups follow. A replica whose fulls run on another node takes log
  backups straight away.
- **Read-only databases** under point-in-time protection no longer attempt log
  backups.
- **New Always On guide** ([docs/ALWAYS-ON.md](https://github.com/sol1/pbsgui/blob/main/docs/ALWAYS-ON.md)):
  setting up every replica, what runs where for each plan, checking and changing
  the group's backup preference, reading `log_reuse_wait_desc`, shrinking a log
  once after it grew, and troubleshooting.

## Upgrading

Install over the previous version as usual. The app and the service must be the
same version; the installer updates both.

## Known limitations

- The availability group changes, VSS file backups, Windows-account SQL
  connections, and the relay are new and unvalidated on real hardware; use v0.2.0
  for production until they have been tested.
- A Windows-account SQL connection signs in with NTLM, so it does not work where
  the domain blocks NTLM.
- Distributed availability groups are not supported yet.
- With Always On or a failover cluster, use the same PBS API token on every node:
  PBS refuses backups into a snapshot group from a token other than its owner.
- Restoring a very old snapshot (one taken before pbsgui stored the database file
  list) under a new name or into a different instance is not supported over the
  relay; restore it under its original name, or run the restore from pbsgui
  installed directly on the SQL host.

Installers are attached below. See the
[README](https://github.com/sol1/pbsgui#install) for which one to choose, plus
setup and upgrade steps.
