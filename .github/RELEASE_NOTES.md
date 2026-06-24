pbsgui backs up Windows files and Microsoft SQL Server to a Proxmox Backup Server
(PBS), with browse and point-in-time restore.

## New in this release

- **See backups that are still running, even ones you did not start.** Open the
  app and any job with a run in progress shows "Running..." on its card with a live
  progress bar and status line, including a backup the scheduler started in the
  background. Click it to watch progress and Stop it if needed.
- **Run several backups at once and switch between them.** The run output panel
  keeps a separate live log and progress per run and has a tab bar to swap between
  them, so a manual run, a scheduled one, and a restore can all be watched at once.
  A job's Run button reads "Running..." for the life of its run.
- **Clearer status while a backup starts.** Instead of a generic "starting..."
  while a large database is read, the status shows the current phase (connecting,
  reading a database, archiving files), so a long initial read no longer looks
  stalled.

Installers are attached below. See the
[README](https://github.com/sol1/pbsgui#install) for which one to choose, plus
setup and upgrade steps.
