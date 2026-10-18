pbsgui backs up Windows files and Microsoft SQL Server to a Proxmox Backup Server
(PBS), with browse and point-in-time restore.

## Testing release (new architecture)

This is a **pre-release for testing a new architecture** and has not yet been
validated end to end on real hardware. If you want the last tested, stable build,
use **v0.2.0**: https://github.com/sol1/pbsgui/releases/tag/v0.2.0

Please try this build in a non-production environment and report what you find.

## New in this release

- **Remote SQL Server backup through a relay proxy (the new architecture).** A thin
  pbsgui agent on the SQL Server host now runs only the backup device and streams the
  raw backup over an encrypted connection to a separate pbsgui "proxy" machine, which
  does the deduplication, compression, encryption, and upload to PBS. The heavy CPU
  work moves off the database server, so a fleet of SQL Servers can be backed up
  without each one paying that cost during the backup window. Both backup and
  point-in-time restore stream over the relay.

  Setup: on the proxy run `pbsgui-engine relay add-agent <name>` (it prints the exact
  command to run on the SQL host), run that `relay join` command on the SQL host, then
  in the SQL Servers tab route a saved connection through the agent with the Relay
  button. The SQL Servers tab shows each agent's live connection status.

- **Snapshot group safety.** A SQL job that would write colliding PBS snapshot groups
  (for example two databases whose names reduce to the same id) is now refused when
  saved, and the job wizard shows the exact snapshot groups a job will create so the
  per-database naming is clear up front.

## Known limitations

- The relay path is new and unvalidated on real SQL Server and PBS hardware; use
  v0.2.0 for production until this has been tested.
- Restoring a very old snapshot (one taken before pbsgui stored the database file
  list) under a new name or into a different instance is not supported over the
  relay; restore it under its original name, or run the restore from pbsgui installed
  directly on the SQL host.

Installers are attached below. See the
[README](https://github.com/sol1/pbsgui#install) for which one to choose, plus
setup and upgrade steps.
