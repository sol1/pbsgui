# Always On availability groups

How pbsgui backs up databases in an Always On availability group (AG), how to set
it up, and how to fix a transaction log that is not truncating.

## Setup

1. Install pbsgui on **every replica** in the group.
2. On each replica, create the same SQL job: the same databases, protection plan,
   and schedule.
3. Use the same **backup id** on every replica. The job wizard defaults it to the
   availability group name. All replicas then write one continuous backup chain
   into the same PBS snapshot groups, whichever node is primary.
4. Use the same **PBS server and API token** on every replica. PBS makes the token
   that first writes a snapshot group its owner and refuses backups into it from
   any other token, so a different token per node fails after a failover.

The pbsgui instances have no connection to each other. Each one asks its local
SQL Server what this replica should do, and SQL Server answers the same way across
the group. After a failover the roles move, and the backups follow them.

## What runs where

| Protection plan | Full backups | Log backups |
| --- | --- | --- |
| Any moment in time (point-in-time) | The **primary**, as normal full backups | The **preferred backup replica** |
| Daily restore points | The preferred backup replica (copy-only on a secondary) | None |
| Safety copy alongside another tool | The preferred backup replica, always copy-only | None |

For point-in-time, the fulls must run on the primary. A secondary can only take
copy-only full backups (before SQL Server 2025), and a copy-only full does not
start the log chain that log backups extend. A log backup taken on a secondary is
a normal log backup and truncates the log for the whole group.

pbsgui on the primary takes a full backup at its next log interval instead of a
log backup, even when the primary is not the preferred backup replica, in two
cases:

- The database has no full backup that starts its log chain (it was just added,
  or its recovery model was changed).
- The job has not taken a full backup on this machine yet, so PBS would otherwise
  hold log backups with no full to restore them onto.

Until then a secondary's log backup for that database finishes with a warning,
not an error. If a secondary still finds no chain after an hour, the run fails,
because something needs fixing (see [Troubleshooting](#troubleshooting)).

Read-only databases take no log backups. Nothing changes them, so their full
backups cover them.

The run log on each replica says what it did with each database. These skips are
normal:

- `point-in-time full backups are taken on the Always On primary replica`: a
  secondary skipping a full backup.
- `not the Always On preferred backup replica`: a replica skipping a log backup
  that another replica takes.

## The backup preference

The preferred backup replica comes from the group's `AUTOMATED_BACKUP_PREFERENCE`
and each replica's backup priority. SQL Server's default is **prefer secondary**,
so by default log backups run on a secondary.

Check the setting:

```sql
SELECT name, automated_backup_preference_desc FROM sys.availability_groups;

SELECT ar.replica_server_name, ar.backup_priority
FROM sys.availability_replicas ar;
```

To run everything on the primary instead:

```sql
ALTER AVAILABILITY GROUP [YourGroup] SET (AUTOMATED_BACKUP_PREFERENCE = PRIMARY);
```

Prefer secondary moves the log backup reads off the primary. Primary is simpler to
reason about. Both work with pbsgui.

A **Basic availability group** (SQL Server Standard edition) does not allow
backups on a secondary at all, so pbsgui runs everything on its primary whatever
the preference says.

Only one tool should take log backups of a database. If another tool also backs
up the log, each tool's chain has gaps the other filled. Use the "safety copy"
plan in pbsgui alongside another tool, or turn the other tool's log backups off.

## Transaction log size

SQL Server truncates the log (marks its space reusable) after a log backup. In an
availability group it also waits until every secondary has received that part of
the log. Truncation does not make the log file smaller; the space is reused.

Check why a log is not truncating:

```sql
SELECT name, recovery_model_desc, log_reuse_wait_desc FROM sys.databases;
```

| `log_reuse_wait_desc` | Meaning | What to do |
| --- | --- | --- |
| `LOG_BACKUP` | Waiting for a log backup. | Check the pbsgui run log on the preferred backup replica. See [Troubleshooting](#troubleshooting). |
| `AVAILABILITY_REPLICA` | A secondary is behind or disconnected. | Fix the replica's synchronization; this is not a backup problem. |
| `ACTIVE_TRANSACTION` | A long-running transaction holds the log. | Wait for it, or find it with `DBCC OPENTRAN`. |
| `NOTHING` | The log can be reused. | Nothing. |

pbsgui never shrinks log files. Once log backups run, the log stops growing. If a
log grew very large while backups were failing and you want the disk space back,
shrink it once, on the primary, after a successful log backup:

```sql
USE [YourDatabase];
-- The logical name of the log file:
SELECT name, size / 128 AS size_mb FROM sys.database_files WHERE type_desc = 'LOG';
DBCC SHRINKFILE (N'YourDatabase_log', 1024); -- target size in MB
```

Do not schedule shrinks. A log that is shrunk and then grows again fragments the
file and slows the database.

## Troubleshooting

**Log backups fail with error 4214, "no current database backup".** No normal
full backup starts the log chain. pbsgui on the primary takes one at its next log
interval. If the warning keeps coming back on a secondary, or the run fails with
"no full backup to start the log chain for over an hour":

- Make sure pbsgui is installed on the primary with the same job.
- Run the job once on the primary (Run on the job card). That takes the full.
- If the primary's run log shows log backups or skips but no full, check
  `log_reuse_wait_desc` and the secondary's synchronization state; a secondary
  that is far behind may not see the primary's full yet.

**The log keeps growing and shows `LOG_BACKUP`.** Look at the run log on the
preferred backup replica, which takes the log backups. If that replica has no
pbsgui, or its job differs, nothing takes them.

**Backups fail after a failover with a PBS ownership or permission error.** The
replicas use different PBS API tokens. Use the same token on every replica.

**A backup fails right after a failover because the snapshot is older than the
last one.** PBS refuses a snapshot older than the newest in its group, so a
replica whose clock is behind can fail one run after taking over. Keep the
replicas' clocks in sync (domain members normally are).

**System databases.** `master`, `model`, and `msdb` are not part of the group and
differ on each node. Back them up with a separate job on each node, using the
"daily restore points" plan and a **different backup id per node** (for example
the host name), so the nodes do not share snapshot groups.

**Distributed availability groups** are not supported yet.

**Failover Cluster Instances** are different: one instance moves between nodes.
pbsgui skips the backup on any node where the instance is not running. See
[DEPLOYMENT.md](DEPLOYMENT.md).
