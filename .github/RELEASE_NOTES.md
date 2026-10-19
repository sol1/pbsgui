pbsgui backs up Windows files and Microsoft SQL Server to a Proxmox Backup Server
(PBS), with browse and point-in-time restore.

## Testing release

This is a **pre-release for testing** and has not yet been validated end to end on
real hardware. If you want the last tested, stable build, use **v0.2.0**:
https://github.com/sol1/pbsgui/releases/tag/v0.2.0

Please try this build in a non-production environment and report what you find.

## New in this release

- **File backups no longer fail on open or locked files** (#5, #6). pbsgui now
  takes a Volume Shadow Copy (VSS) snapshot and reads the files from it, so
  locked files such as a signed-in user's profile under `C:\Users` are backed up.
  The snapshot is a copy backup, so SQL Server and other VSS writers do not
  record it as one of their backups.
- **Unreadable files are skipped, not fatal.** A file that still cannot be read
  (permission denied, or a OneDrive file that is only in the cloud) is skipped and
  the rest of the backup continues. The run finishes with **warnings**: the job
  card turns amber and names the skipped files, the run log lists them all, and
  notifications report WARNINGS.
- **No more large temp file.** File backups stream straight to PBS instead of
  writing the whole archive to the temp folder first.
- **Weekly and monthly schedules** (#5), for file jobs and for SQL full backups.
  Weekly takes any set of days (for example weekdays at 22:00). Monthly takes a
  day from 1 to 31; a day past the end of a shorter month runs on its last day. A
  run missed while the service was down now runs as soon as it is back.
- **SQL connections with a Windows account or a SQL login.** The SQL Servers tab
  has a connection form (Add connection, and Edit on a saved one) with a choice of
  the service account, a Windows account (`DOMAIN\user`), or a SQL login, and a
  Test button that runs the readiness checks. Use this for remote instances
  instead of changing the account the pbsgui service runs as.
- **Configuration is never overwritten after a failed load.** Previously, running
  the service as a different Windows account showed empty settings, and saving
  anything then replaced the real configuration. pbsgui now leaves the files
  untouched, refuses changes, and shows an error explaining the cause and how to
  recover (set the service back to Local System).
- **Fix:** a SQL connection entered as `HOST\MSSQLSERVER` now works as the
  default instance for backup and restore.

## Upgrading

Install over the previous version as usual. The app and the service must be the
same version; the installer updates both.

## Known limitations

- VSS file backups, Windows-account SQL connections, and the relay are new and
  unvalidated on real hardware; use v0.2.0 for production until they have been
  tested.
- A Windows-account SQL connection signs in with NTLM, so it does not work where
  the domain blocks NTLM.
- With Always On or a failover cluster, use the same PBS API token on every node:
  PBS refuses backups into a snapshot group from a token other than its owner.
- Restoring a very old snapshot (one taken before pbsgui stored the database file
  list) under a new name or into a different instance is not supported over the
  relay; restore it under its original name, or run the restore from pbsgui
  installed directly on the SQL host.

Installers are attached below. See the
[README](https://github.com/sol1/pbsgui#install) for which one to choose, plus
setup and upgrade steps.
