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

## In progress

- **SQL Server VDI backup.** Streaming backup over the Virtual Device Interface:
  the COM device loop on SQLVDI.dll plus a concurrent `BACKUP DATABASE ... TO
  VIRTUAL_DEVICE` statement. The first stage writes the backup stream to a local
  file to validate the device handshake on real hardware; the next stage streams
  it into the PBS uploader as a fixed-index image, one snapshot per backup.

## Planned

- **Transaction-log backups and log-chain management** for FULL and BULK_LOGGED
  databases (frequent log backups, copy-only policy, log-reuse monitoring).
- **SQL connection without TCP/IP**: connect to local instances over Shared
  Memory via the Microsoft ODBC driver, keeping TCP for remote instances.
- **Network SQL discovery**: SQL Browser (UDP 1434), host/subnet scanning, and
  Active Directory lookups, including Availability Group listeners.
- **Additional authentication**: explicit Windows accounts and Azure AD / Entra.
- **Notifications**: email (SMTP), Slack and Microsoft Teams webhooks, a generic
  webhook, and a heartbeat / dead-man's-switch, on success and failure, with
  metrics and a durable retry queue.
- **Client-side encryption**: AES-256-GCM matching the PBS scheme, with a
  dedup-preserving keyed digest and key material held in the credential store.
- **Code signing** of the installer and binaries (currently deferred).

## Known limitations and notes

- SQL Server connections currently require TCP/IP to be enabled on the instance,
  because the TDS driver is TCP-only. Fresh installs often ship with TCP/IP
  disabled; the Shared Memory path above removes this requirement for local
  instances.
- VDI backup requires the connecting login to be in the `sysadmin` server role.
- The in-process scheduler runs only while the engine process is running, which is
  fine when the engine runs as the always-on service.
