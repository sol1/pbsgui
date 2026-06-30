pbsgui backs up Windows files and Microsoft SQL Server to a Proxmox Backup Server
(PBS), with browse and point-in-time restore.

## New in this release

This release is about trust: a backup or restore is now either complete and
correct, or it fails loudly, and the place pbsgui keeps its settings is locked
down.

- **Restores are verified end to end.** Every chunk pulled back from PBS is checked
  against its content hash as it arrives, so a corrupt or tampered datastore can no
  longer produce a silently wrong restore. A point-in-time restore also refuses to
  run unless the backup chain actually reaches the moment you picked: if the logs
  needed to get there are missing or unreadable, you get a clear error naming how
  far the backup really covers, instead of a restore that quietly stops short and
  reports success.

- **Backups never commit a partial snapshot.** If a SQL Server BACKUP fails partway,
  or you cancel a running backup, the truncated bytes already streamed are discarded
  rather than finalised, so a half-written snapshot can never be mistaken for a good
  one. A backup that did finish is always recorded as the success it was, even when
  a cancel arrives at the same instant.

- **Hardened settings storage.** The configuration directory is restricted to SYSTEM
  and administrators, and the job and connection files are signed and written
  atomically, so tampering or corruption is detected and a half-written file is never
  loaded. The pre/post job hook scripts have been removed, closing the path they
  offered for running arbitrary commands as the backup service.

- **Steadier under load.** Concurrent edits to jobs no longer risk losing an update,
  and a GUI that disconnects mid-run no longer stalls the run: a manual backup always
  finishes and is recorded, the same as a scheduled one.

Installers are attached below. See the
[README](https://github.com/sol1/pbsgui#install) for which one to choose, plus
setup and upgrade steps.
