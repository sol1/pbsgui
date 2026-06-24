pbsgui backs up Windows files and Microsoft SQL Server to a Proxmox Backup Server
(PBS), with browse and point-in-time restore.

## New in this release

- **Restore a SQL database to a file.** A new "Save to file" action in the SQL
  restore view writes the native SQL Server backup to a folder without touching a
  SQL Server: pick a full restore point for its `.bak`, or a point in time for the
  covering full plus its `.trn` log backups and a steps file with the exact
  `RESTORE` statements. The files restore on any SQL Server (SSMS or sqlcmd), so you
  can recover to a different or newer server, hand the backup to a DBA, or migrate.
- **Large databases now restore.** The to-file export and the restore back into SQL
  Server both stream the backup chunk by chunk instead of holding it in memory, so a
  multi-hundred-GB or terabyte database restores without running out of RAM.
  Restoring under a new name relocates the database files using a file list now
  recorded in each backup; backups made before this release restore under a new name
  the previous way.
- **Clearer "Active runs" switcher.** When more than one backup or restore is in
  progress, the tabs that swap the output between them are now labelled and read
  clearly as tabs.

Installers are attached below. See the
[README](https://github.com/sol1/pbsgui#install) for which one to choose, plus
setup and upgrade steps.
