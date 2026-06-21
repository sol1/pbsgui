# Status and roadmap

A snapshot of what is implemented, what is in progress, and what is planned.
Cross-platform code is built and tested on Linux and Windows; Windows-only code
(the service, the tray, credential storage, SQL Server integration) is built in
CI on Windows and exercised manually.

## Working

- **PBS protocol client (clean-room).** HTTP/1.1 to HTTP/2 upgrade, TLS
  fingerprint pinning, data blobs, fixed and dynamic indexes, FastCDC chunking,
  and incremental deduplication against the previous snapshot. Backup and restore
  have been validated against a live PBS, including a deduplicated re-run that
  reused all chunks.
- **File and folder backup jobs.** Create, edit, delete, and run jobs; manual,
  interval, and daily schedules; glob excludes; deterministic archiving for
  reliable dedup.
- **Secure settings.** Job configuration is stored as JSON; the PBS API token
  secret is stored in the Windows Credential Manager and never written to the
  config file or carried in job messages.
- **Browse and restore.** List snapshots by date and time, list files within a
  snapshot, and restore everything or selected paths, with progress.
- **Change detection and scripts.** Optionally skip a run when no source file has
  changed; run pre-job and post-job scripts, with job status passed to the
  post-job script through environment variables.
- **Windows service.** The engine installs and runs as a LocalSystem service; the
  installer registers and starts it, and removes it on uninstall. The GUI shows
  service reachability and connects to the service rather than spawning it.
- **Tray and window behavior.** A system-tray icon with Show/Quit; closing or
  minimizing the window hides it to the tray while the service keeps running.
- **Build versioning.** Builds are stamped with a version and short commit id,
  shown in the GUI and printed by the engine.
- **SQL Server discovery.** Enumerate local instances from the registry (instance
  names, TCP port, login mode, clustered flag) with no credentials, and surface
  when an instance has TCP/IP disabled.
- **SQL Server probe.** Connect to an instance and report version, edition,
  topology (standalone, failover cluster, or Always On with replica role), and
  databases with recovery model, state, Availability Group membership, and
  preferred-backup-replica. Validated against a live SQL Server instance.
- **SQL Server readiness check.** A per-instance Check runs connectivity, login
  identity, and `sysadmin`-role checks, each with a copyable fix hint.
- **Saved connections.** SQL Server connections and PBS servers are first-class
  saved entities (managed in their own tabs); jobs reference them by id and carry
  no secrets. SQL passwords and PBS token secrets live in the credential store.
- **SQL Server VDI backup to PBS.** Streaming backup over the Virtual Device
  Interface: a `BACKUP ... TO VIRTUAL_DEVICE` statement runs while a native COM
  loop on SQLVDI.dll forwards SQL's backup buffers to the PBS uploader as a
  deduplicated dynamic-index snapshot, one snapshot per database per run, with no
  on-disk staging. Validated end to end against a live SQL Server and PBS,
  including deduplicated re-runs. The connecting login must be `sysadmin`.
- **SQL Server transaction-log backups.** A Log backup type takes
  `BACKUP LOG ... TO VIRTUAL_DEVICE` (never copy-only) so the inactive
  transaction log is truncated and FULL / BULK_LOGGED databases do not grow
  without bound. Log snapshots land in a separate snapshot group. Full backups
  are copy-only by default (so they do not disturb another tool's chain); turning
  copy-only off lets pbsgui own the chain so log backups can run.
- **SQL Server browse and restore.** List a database's snapshots by date and
  time and restore over VDI, either over the original database or to a new name
  (the latter relocates the data and log files via `WITH MOVE`).
- **Three-step job wizard.** Jobs pair a source (files or SQL Server databases)
  with a destination (a PBS server or a folder) across a Source / Destination /
  Schedule wizard.
- **Client-side encryption.** Optional per-job AES-256-GCM, byte-compatible with
  the PBS encryption scheme (so the official client can read the backups given
  the key), with a dedup-preserving keyed chunk digest. Keys are generated or
  imported per job, shown once to copy into a password manager, and stored in the
  credential store; backups encrypt and restores decrypt transparently.

## In progress

- **Differential SQL backups** and **point-in-time / log-chain restore** (the log
  backups are taken and truncate the log today; replaying a full plus a log chain
  to a point in time is not wired up yet).

## Planned

- **Network SQL discovery**: SQL Browser (UDP 1434), host/subnet scanning, and
  Active Directory lookups, including Availability Group listeners.
- **Additional authentication**: explicit Windows accounts and Azure AD / Entra.
- **Notifications**: email (SMTP), Slack and Microsoft Teams webhooks, a generic
  webhook, and a heartbeat / dead-man's-switch, on success and failure, with
  metrics and a durable retry queue.
- **VSS-based filesystem backup** for consistent snapshots of open files.
- **Code signing** of the installer and binaries (currently deferred).

## Known limitations and notes

- SQL Server connections require TCP/IP to be enabled on the instance (the client
  uses the TDS protocol over TCP). Fresh installs often ship with TCP/IP disabled;
  pbsgui detects and flags this during discovery, and the per-instance Check
  reports it with the fix, which is a one-time step in SQL Server Configuration
  Manager.
- VDI backup and restore require the connecting login to be in the `sysadmin`
  server role.
- Encryption keys cannot be recovered. If a key is lost, backups made with it
  cannot be restored, by anyone. Copy the key into a password manager when it is
  shown.
- Transaction-log backups truncate the log and are stored offsite, but restoring a
  point in time from a full plus a log chain is not implemented yet; restore today
  is from a full snapshot.
- The in-process scheduler runs only while the engine process is running, which is
  fine when the engine runs as the always-on service.
