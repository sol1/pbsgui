# Testing

pbsgui's logic is unit-tested in CI, but its core value (SQL Server backup over
VDI, the PBS protocol against a live server, point-in-time restore) only runs on
Windows against real SQL Server and PBS. This page makes that integration layer a
**repeatable, objective pass** instead of ad-hoc clicking.

## Tiers

1. **Unit tests** - `cargo test --workspace`, run in CI on every push. Cross-platform
   logic: restore-chain selection, data-blob/index wire formats, repository and
   namespace parsing, scheduling, the `BACKUP`/`RESTORE` statement builders.
2. **Windows cross-compile check** - compiles the `cfg(windows)` code from Linux:
   ```
   cargo clippy -p pbsgui-engine --target x86_64-pc-windows-gnu --all-targets -- -D warnings
   cargo check  -p pbsgui        --target x86_64-pc-windows-gnu
   ```
3. **Manual integration pass** (this page) - on Windows + SQL Server + PBS. Run it
   before tagging a release, and record the result (the GUI shows the build
   version + commit in the top bar).
4. **Future: self-hosted Windows CI** - a runner with SQL Server Developer (free)
   and a PBS test instance, running `#[ignore]` integration tests automatically.

## Setup for the manual pass

- The engine service installed and running; a SQL Server with TCP/IP enabled and
  `NT AUTHORITY\SYSTEM` (or your connection's login) in the `sysadmin` role; a PBS
  server with an API token and a datastore (see [DEVELOPMENT.md](DEVELOPMENT.md)).
- In the GUI: a saved SQL connection and a saved PBS server.
- A test database in **FULL** recovery for the point-in-time scenarios.
- The point-in-time verifier: `scripts/sql-pitr-probe.ps1`.

## Scenario checklist

Record build + date, and pass/fail per row.

### Files

- [ ] Files job to PBS: run, then Browse -> snapshot -> Restore all to a folder; files match.
- [ ] Restore selected files only.
- [ ] Change detection: re-run with nothing changed -> "no changes; skipped".

### SQL - daily restore points (full only)

- [ ] Wizard guidance: a SIMPLE-recovery DB shows no warning; a FULL-recovery DB warns the log will grow.
- [ ] Run -> a full lands; Browse shows a full restore point with a size.
- [ ] Restore over the original name; data matches.
- [ ] Restore to a new name (`..._restored`); files relocate, data matches.

### SQL - point-in-time

- [ ] Wizard guidance: a SIMPLE-recovery DB is **blocked** with the `ALTER DATABASE ... SET RECOVERY FULL` hint.
- [ ] Run once manually -> the full anchors the chain; logs then appear on the log interval.
- [ ] The restore view shows full points (with sizes) and a point-in-time window with a log count + total.
- [ ] **Point-in-time correctness** (objective, see below): restore to a chosen moment and verify the data matches that moment.
- [ ] Edge: restore to "latest"; restore to just after the full (before the first log).
- [ ] Log truncation: the DB's log file stops growing once log backups run (`DBCC SQLPERF(LOGSPACE)`).

### Encryption

- [ ] Encrypted job: generate a key (shown once), back up, then restore -> decrypts transparently.
- [ ] Restore with the key cleared from the credential store -> fails with a clear "encrypted but no key" message.

### Notifications

- [ ] Configure email and a webhook; the per-channel Test buttons succeed.
- [ ] Run a job -> a success notification arrives with the metrics; a forced failure -> a failure notification.

## Objective point-in-time check

"It restored without error" is not enough - verify it restored to the *right
moment*. `scripts/sql-pitr-probe.ps1` writes a timestamped marker row every few
seconds so you have a ground-truth timeline.

1. Start the generator against the live DB while a point-in-time job runs:
   ```
   .\scripts\sql-pitr-probe.ps1 -Server <srv> -Database PbsTestDb -IntervalSeconds 15
   ```
   It prints each marker's UTC time. Let a full and a few logs run. Note a UTC
   time `T` between two markers.
2. In the GUI, restore the database to `T` into a new name (e.g. `PbsTestDb_pit`).
3. Verify against the restored copy:
   ```
   .\scripts\sql-pitr-probe.ps1 -Server <srv> -Database PbsTestDb_pit -Verify -AtUtc "<T>"
   ```
   A correct restore shows the last marker at or just before `T` and
   **`after_target = 0`** (no rows newer than `T`). Any rows after `T` mean the
   chain over- or under-applied logs.
