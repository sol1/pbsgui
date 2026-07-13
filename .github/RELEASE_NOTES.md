pbsgui backs up Windows files and Microsoft SQL Server to a Proxmox Backup Server
(PBS), with browse and point-in-time restore.

## New in this release

- **Restore to a different SQL Server, or to a network folder.** A SQL database can
  now be restored into a different SQL Server instance: pick a saved connection as
  the target, and pbsgui relocates the database files to that server's default data
  and log directories after checking the target is reachable and a sysadmin. A
  restore to files can also target a network (UNC) path, prompting for credentials
  when the backup service cannot reach the share on its own.

- **One database's failure no longer stops the rest.** A SQL job covering several
  databases now backs up every eligible one and reports a clear per-database summary,
  instead of aborting the whole run on the first failure. A database in SIMPLE
  recovery is skipped for transaction-log backups with a plain message (SQL Server
  manages that log itself) rather than being treated as an error, and any database
  whose log backups are failing is now visible instead of silently growing its log.

- **Backups and restores leave CPU headroom.** Compression and encryption no longer
  run wide open across every core, so a backup, and especially a restore, no longer
  saturates a live database server. The default is about half the machine's cores;
  set the PBSGUI_WORKER_LIMIT environment variable to widen it for a dedicated backup
  window.

- **Compact notifications.** Slack and webhook messages are now a compact one or two
  lines led by a status symbol (success, warning, or failure), with human-readable
  sizes (for example 716 GiB) instead of raw byte counts. Email and the in-app log
  show readable sizes too.

Installers are attached below. See the
[README](https://github.com/sol1/pbsgui#install) for which one to choose, plus
setup and upgrade steps.
