# Screenshots

These images are referenced from [../README.md](../README.md). They are captured
from the running Windows desktop app, so they have to be taken on Windows (the GUI
cannot be shown on the headless Linux build).

## Capturing

1. Build and run the app on Windows (see [../DEVELOPERS.md](../DEVELOPERS.md)),
   with the engine running and a PBS server and SQL Server connection configured
   so the views have real content.
2. Take each shot below at a consistent window size (about 1100x800), PNG, and
   save it here with the exact file name.
3. Crop to the app window; avoid capturing the desktop or other windows. Do not
   include real server hostnames, tokens, or encryption keys: use a test PBS
   server, and never screenshot a revealed encryption key.

## Collected screenshots

| File | View | What it shows |
| --- | --- | --- |
| `pbsgui-joblist.png` | Jobs dashboard | The job dashboard: a summary strip (jobs / healthy / failed / never-run / encrypted) above a job card with a status chip, configuration badges (SQL Server, point-in-time), a size-over-time sparkline with the latest size, the last-backup time, and Run / Edit / Delete actions. |
| `pbsgui-mssql.png` | SQL Servers | Discovery with the "search the network" toggle and a hosts/subnets field, a saved connection, and a discovered instance showing its topology and edition. The database list carries recovery-model and `system` badges (master / model / msdb) and log-wait reasons, each row with To PBS / To file. |
| `pbsgui-restore.png` | Browse & restore | Point-in-time restore: the job and database pickers, a "Restore to any moment" box describing the recoverable window (earliest to latest, log-backup count and total size) with a date-time picker, and below it the list of full restore points with their sizes. |
| `pbsgui-notifications.png` | Notifications | Global notification settings: notify on failure and/or success, the "warn when a point-in-time chain stalls" toggle, and the email (SMTP) and Slack-compatible webhook sections, each with a Test button. |
| `pbsgui-pbsservers.png` | PBS servers | The add-server form (name, repository with optional namespace, SHA-256 fingerprint, API token secret) and a saved server listed with the PBS avatar and its repository. |
