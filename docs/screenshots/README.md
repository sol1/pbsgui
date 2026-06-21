# Screenshots

These images are referenced from [../README.md](../README.md). They are captured
from the running Windows desktop app, so they have to be taken on Windows (the GUI
cannot be shown on the headless Linux build).

## Capturing

1. Build and run the app on Windows (see [../DEVELOPMENT.md](../DEVELOPMENT.md)),
   with the engine running and a PBS server and SQL Server connection configured
   so the views have real content.
2. Take each shot below at a consistent window size (about 1100x800), PNG, and
   save it here with the exact file name.
3. Crop to the app window; avoid capturing the desktop or other windows. Do not
   include real server hostnames, tokens, or encryption keys: use a test PBS
   server, and never screenshot a revealed encryption key.

## Shot list

| File | View | What to show |
| --- | --- | --- |
| `jobs.png` | Jobs | A few jobs of different kinds (files to PBS, SQL full, SQL log), with the source/destination summary visible. |
| `wizard-source.png` | Job wizard, Source step | The SQL source with a connection picked, databases listed, and the backup type (Full / Log) selector. |
| `wizard-destination.png` | Job wizard, Destination step | A PBS destination with "Encrypt this backup" enabled and the key panel showing a fingerprint (not the raw key). |
| `sql-servers.png` | SQL Servers | Discovered instances and a saved connection, ideally with a Probe or Check result expanded. |
| `browse-restore.png` | Browse & restore | A job selected with its snapshots listed and a file list or restore action visible. |
| `pbs-servers.png` | PBS servers | The PBS server form and a saved server in the list. |

Until the PNGs are added, the image links in the project README will show as
broken images; that is expected.
