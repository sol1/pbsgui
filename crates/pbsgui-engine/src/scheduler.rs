//! Runs jobs on their schedule while the engine is running.
//!
//! This covers interactive/served scheduling. Unattended scheduling that
//! survives logoff/reboot arrives with the Windows Service.

use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, Timelike};
use pbsgui_ipc::{Job, JobSource, Reply, Schedule, SqlProtection};
use tokio::sync::mpsc;

use crate::backup::{self, SqlRun};
use crate::config::unix_now;
use crate::jobstore::JobStore;
use crate::sqlsched;

/// Check every minute and run any due jobs.
pub async fn run(store: Arc<JobStore>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut last_stall_check = 0i64;
    // Per (job, database) last alert time, to avoid repeating a stall warning.
    let mut alerted: std::collections::HashMap<(String, String), i64> =
        std::collections::HashMap::new();
    loop {
        tick.tick().await;
        let now = unix_now();
        for job in store.list() {
            if let Some(kind) = due_kind(&job, now) {
                run_due(&store, job, kind).await;
            }
        }
        // Refresh SQL chain health (stall alerts + metrics) every 15 minutes.
        if now - last_stall_check >= 15 * 60 {
            last_stall_check = now;
            refresh_sql_health(&store, now, &mut alerted).await;
        }
    }
}

/// Read the shared PBS groups for every point-in-time job, update the metrics
/// state, and warn about stalled chains (re-warning at most every 6 hours).
async fn refresh_sql_health(
    store: &Arc<JobStore>,
    now: i64,
    alerted: &mut std::collections::HashMap<(String, String), i64>,
) {
    const REALERT_SECS: i64 = 6 * 3600;
    for status in crate::handler::collect_sql_status(store, now).await {
        crate::metrics::set_sql_status(&status.job_id, status.databases.clone());
        for db in &status.databases {
            if !db.stalled {
                continue;
            }
            let key = (status.job_id.clone(), db.database.clone());
            if alerted.get(&key).is_some_and(|t| now - t < REALERT_SECS) {
                continue;
            }
            alerted.insert(key, now);
            let hours = db.chain_latest.map_or(0, |l| ((now - l) / 3600).max(1));
            tracing::warn!(job = %status.job_name, database = %db.database, hours, "backup chain stalled");
            crate::notify::backup_stalled(&status.job_name, &db.database, hours).await;
        }
    }
    // Keep the textfile fresh (no-op unless textfile mode is on).
    crate::metrics::write_textfile(store);
}

async fn run_due(store: &Arc<JobStore>, job: Job, kind: SqlRun) {
    tracing::info!(job = %job.name, "running scheduled job");
    let (tx, mut rx) = mpsc::channel::<Reply>(64);
    let job_for_run = job.clone();
    let handle = tokio::spawn(async move { backup::run_job_kind(&job_for_run, kind, tx).await });

    while let Some(reply) = rx.recv().await {
        if let Reply::Log { line } = reply {
            tracing::info!("{line}");
        }
    }

    let status = match handle.await {
        Ok(Ok(_)) => "ok".to_string(),
        Ok(Err(e)) => e.to_string(),
        Err(e) => format!("task failed: {e}"),
    };
    // The SQL full/log chain timers are recorded inside run_job_kind on success,
    // so both manual and scheduled runs advance them.
    let _ = store.record_run(&job.id, unix_now(), status);
    // Refresh the metrics textfile (no-op unless textfile mode is on).
    crate::metrics::write_textfile(store);
}

/// Whether a job is due now and, for a SQL job, whether a full or log is due. A
/// file job returns `Some(SqlRun::Full)` (the kind is ignored for non-SQL).
fn due_kind(job: &Job, now: i64) -> Option<SqlRun> {
    match &job.source {
        JobSource::Sql { protection, .. } => sql_due_kind(&job.id, protection, now),
        _ => schedule_due(&job.schedule, job.last_run, now).then_some(SqlRun::Full),
    }
}

/// For a SQL job, decide whether a full (chain-anchoring) or a log backup is due.
/// Full takes precedence; logs only run once a full has anchored the chain.
fn sql_due_kind(job_id: &str, protection: &SqlProtection, now: i64) -> Option<SqlRun> {
    match protection {
        SqlProtection::PointInTime {
            full,
            log_interval_minutes,
        } => {
            let last_full = sqlsched::last_full(job_id);
            if schedule_due(full, last_full, now) {
                return Some(SqlRun::Full);
            }
            last_full?; // logs require an anchoring full first
            let interval = (*log_interval_minutes as i64).max(1) * 60;
            let baseline = sqlsched::last_log(job_id).or(last_full);
            match baseline {
                Some(b) if now - b >= interval => Some(SqlRun::Log),
                _ => None,
            }
        }
        SqlProtection::DailyRestorePoints { schedule }
        | SqlProtection::SecondaryCopy { schedule } => {
            schedule_due(schedule, sqlsched::last_full(job_id), now).then_some(SqlRun::Full)
        }
    }
}

/// Whether a schedule is due, given the last run time.
fn schedule_due(schedule: &Schedule, last_run: Option<i64>, now: i64) -> bool {
    match schedule {
        Schedule::Manual => false,
        Schedule::Interval { minutes } => {
            let interval = (*minutes as i64).max(1) * 60;
            match last_run {
                None => true,
                Some(last) => now - last >= interval,
            }
        }
        Schedule::Daily { hour, minute } => {
            let scheduled = today_scheduled_unix(*hour, *minute);
            now >= scheduled
                && match last_run {
                    None => true,
                    Some(last) => last < scheduled,
                }
        }
    }
}

/// Unix time of today's local HH:MM.
fn today_scheduled_unix(hour: u8, minute: u8) -> i64 {
    let now = Local::now();
    now.with_hour(hour as u32)
        .and_then(|d| d.with_minute(minute as u32))
        .and_then(|d| d.with_second(0))
        .map(|d| d.timestamp())
        .unwrap_or_else(|| now.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_is_never_due() {
        assert!(!schedule_due(&Schedule::Manual, None, 1_000_000));
    }

    #[test]
    fn interval_due_when_elapsed() {
        let now = 1_000_000;
        let sched = Schedule::Interval { minutes: 30 };
        assert!(schedule_due(&sched, None, now)); // never run
        assert!(schedule_due(&sched, Some(now - 31 * 60), now));
        assert!(!schedule_due(&sched, Some(now - 10 * 60), now));
    }

    #[test]
    fn sql_point_in_time_runs_full_then_logs() {
        // No full yet -> a full is due (interval schedule), not a log.
        let p = SqlProtection::PointInTime {
            full: Schedule::Interval { minutes: 1440 },
            log_interval_minutes: 15,
        };
        // With no stored state, the full interval has "never run" -> full due.
        assert!(matches!(
            sql_due_kind("nope-no-state", &p, 1_000_000),
            Some(SqlRun::Full)
        ));
    }
}
