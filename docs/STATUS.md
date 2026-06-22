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
- **SQL Server protection plans (outcome-driven).** A SQL job picks what it
  should be able to restore, not a raw backup type: *point-in-time recovery*
  (scheduled fulls plus frequent log backups, which also truncate the log),
  *daily restore points* (fulls only), or a *secondary copy* (copy-only fulls
  that coexist with another backup tool). The wizard reads each database's
  recovery model and, detect-and-explain only, blocks point-in-time on a SIMPLE
  database (with the exact `ALTER DATABASE ... SET RECOVERY FULL` to run) and
  warns when a FULL-recovery database is on a full-only plan (the log-growth
  trap). The engine schedules the full and log cadences independently.
- **High-availability aware backups.** Install the engine on each node and it
  coordinates through SQL Server, with no connection between the pbsgui instances:
  an Always On database is backed up only on the preferred backup replica
  (`sys.fn_hadr_backup_is_preferred_replica`), forcing a copy-only full on a
  secondary; a Failover Cluster Instance is skipped on whichever node is not the
  active one. System databases (master/model/msdb) are offered for backup
  (tempdb is hidden); the wizard explains the restore caveats.
- **SQL Server point-in-time restore.** Restore a database to any moment within
  the retained window: pbsgui picks the covering full plus the log chain, restores
  the full `WITH NORECOVERY`, replays the logs `WITH STOPAT`, and recovers at the
  chosen second. It can also restore a specific full backup. Restore is over the
  original name or a new one (files relocated via `WITH MOVE`). Each backup's LSN
  range is stored in the snapshot so the chain can be rebuilt.
- **Three-step job wizard.** Jobs pair a source (files or SQL Server databases)
  with a destination (a PBS server or a folder) across a Source / Destination /
  Schedule wizard.
- **Client-side encryption.** Optional per-job AES-256-GCM, byte-compatible with
  the PBS encryption scheme (so the official client can read the backups given
  the key), with a dedup-preserving keyed chunk digest. Keys are generated or
  imported per job, shown once to copy into a password manager, and stored in the
  credential store; backups encrypt and restores decrypt transparently.
- **Notifications.** Global settings notify when a job succeeds and/or fails,
  through email (SMTP, with STARTTLS/TLS) and a Slack-compatible webhook. Each
  channel has a Test button; the SMTP password and webhook URL are stored in the
  credential store. The message carries the job name, status, and backup metrics.

## In progress

- **Differential SQL backups**, **log-chain re-base detection** (an external tool
  breaking the chain), and managing the PBS **retention** that bounds the
  point-in-time window.

## Planned

- **Active Directory SQL discovery** (SPN lookup) and Availability Group listener
  enumeration. SQL Browser (UDP 1434) broadcast and host/subnet scanning already
  work.
- **Additional authentication**: explicit Windows accounts and Azure AD / Entra.
- **More notifications**: Microsoft Teams cards, a heartbeat / dead-man's-switch,
  per-job routing, a durable retry queue, and daily digests (email and a
  Slack-compatible webhook already work).
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
