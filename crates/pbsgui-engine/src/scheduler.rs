//! Runs jobs on their schedule while the engine is running.
//!
//! This covers interactive/served scheduling. Unattended scheduling that
//! survives logoff/reboot arrives with the Windows Service.

use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, Days, Local, NaiveDate, TimeZone};
use pbsgui_ipc::{Job, JobSource, Reply, Schedule, SqlProtection};
use tokio::sync::mpsc;

use crate::backup::{self, SqlRun};
use crate::config::unix_now;
use crate::jobstore::JobStore;
use crate::sqlsched;

/// Check every minute and run any due jobs.
pub async fn run(store: Arc<JobStore>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    // A long backup blocks this loop for its whole duration (runs are sequential).
    // Skip the ticks it overran instead of firing them all at once on return,
    // which would otherwise re-evaluate every job in a tight burst.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
        Ok(Ok(report)) => report.status(),
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
            // Full cadence is gated on the last full ATTEMPT (so a failed full is
            // not instantly due again), but logs require a SUCCESSFUL anchoring
            // full and run off the success timers.
            if schedule_due(full, sqlsched::last_full_attempt(job_id), now) {
                return Some(SqlRun::Full);
            }
            let last_full = sqlsched::last_full(job_id);
            last_full?; // logs require an anchoring full first
            let interval = (*log_interval_minutes as i64).max(1) * 60;
            let baseline = sqlsched::last_log_attempt(job_id).or(last_full);
            match baseline {
                Some(b) if now - b >= interval => Some(SqlRun::Log),
                _ => None,
            }
        }
        SqlProtection::DailyRestorePoints { schedule }
        | SqlProtection::SecondaryCopy { schedule } => {
            schedule_due(schedule, sqlsched::last_full_attempt(job_id), now).then_some(SqlRun::Full)
        }
    }
}

/// Whether a schedule is due, given the last run time.
///
/// A timed schedule (daily, weekly, monthly) is due once its most recent
/// occurrence has passed and the job has not run since, so a run missed while
/// the engine was down catches up when it is back. A job that has never run
/// waits for its first occurrence, unless that is earlier today.
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
        _ => match latest_occurrence(schedule, now) {
            None => false,
            Some((occurrence, today)) => match last_run {
                None => today,
                Some(last) => last < occurrence,
            },
        },
    }
}

/// The most recent occurrence of a timed schedule at or before `now` (unix
/// seconds), and whether it falls on today's local date. `None` for manual and
/// interval schedules, or a weekly schedule with no days.
fn latest_occurrence(schedule: &Schedule, now: i64) -> Option<(i64, bool)> {
    let (hour, minute) = match schedule {
        Schedule::Daily { hour, minute }
        | Schedule::Weekly { hour, minute, .. }
        | Schedule::Monthly { hour, minute, .. } => (*hour, *minute),
        Schedule::Manual | Schedule::Interval { .. } => return None,
    };
    let today = Local.timestamp_opt(now, 0).single()?.date_naive();
    // Two months back covers every monthly day, including one that is skipped
    // because it fell on a DST gap.
    for back in 0..=62u64 {
        let date = today.checked_sub_days(Days::new(back))?;
        if !runs_on(schedule, date) {
            continue;
        }
        let at = local_unix(date, hour, minute)?;
        if at <= now {
            return Some((at, back == 0));
        }
    }
    None
}

/// Whether a timed schedule has an occurrence on `date`.
fn runs_on(schedule: &Schedule, date: NaiveDate) -> bool {
    match schedule {
        Schedule::Daily { .. } => true,
        Schedule::Weekly { days, .. } => {
            days.contains(&(date.weekday().num_days_from_sunday() as u8))
        }
        Schedule::Monthly { day, .. } => date.day() == u32::from(*day).min(days_in_month(date)),
        Schedule::Manual | Schedule::Interval { .. } => false,
    }
}

fn days_in_month(date: NaiveDate) -> u32 {
    let (y, m) = (date.year(), date.month());
    let next = if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1)
    };
    next.and_then(|n| n.pred_opt())
        .map(|d| d.day())
        .unwrap_or(28)
}

/// Unix time of `date` at local HH:MM. In a DST gap (the time does not exist
/// that day) the occurrence moves to the end of the gap.
fn local_unix(date: NaiveDate, hour: u8, minute: u8) -> Option<i64> {
    let naive = date.and_hms_opt(u32::from(hour), u32::from(minute), 0)?;
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(t) => Some(t.timestamp()),
        chrono::LocalResult::Ambiguous(first, _) => Some(first.timestamp()),
        chrono::LocalResult::None => {
            let later = naive + chrono::Duration::hours(1);
            Local
                .from_local_datetime(&later)
                .earliest()
                .map(|t| t.timestamp())
        }
    }
}

/// Reject a schedule the scheduler cannot run (out-of-range time, a weekly
/// schedule with no days, a month day outside 1-31).
pub fn validate(schedule: &Schedule) -> anyhow::Result<()> {
    let check_time = |hour: u8, minute: u8| {
        if hour > 23 || minute > 59 {
            anyhow::bail!("the schedule time {hour:02}:{minute:02} is not a valid time of day");
        }
        Ok(())
    };
    match schedule {
        Schedule::Manual => Ok(()),
        Schedule::Interval { minutes } => {
            if *minutes == 0 {
                anyhow::bail!("the interval must be at least one minute");
            }
            Ok(())
        }
        Schedule::Daily { hour, minute } => check_time(*hour, *minute),
        Schedule::Weekly { days, hour, minute } => {
            if days.is_empty() {
                anyhow::bail!("a weekly schedule needs at least one day");
            }
            if let Some(d) = days.iter().find(|d| **d > 6) {
                anyhow::bail!("{d} is not a weekday (0 = Sunday .. 6 = Saturday)");
            }
            check_time(*hour, *minute)
        }
        Schedule::Monthly { day, hour, minute } => {
            if !(1..=31).contains(day) {
                anyhow::bail!("the day of the month must be between 1 and 31");
            }
            check_time(*hour, *minute)
        }
    }
}

/// Validate every schedule a job carries: its own, or its SQL plan's.
pub fn validate_job(job: &Job) -> anyhow::Result<()> {
    match &job.source {
        JobSource::Sql { protection, .. } => match protection {
            SqlProtection::PointInTime {
                full,
                log_interval_minutes,
            } => {
                if *log_interval_minutes == 0 {
                    anyhow::bail!("the log backup interval must be at least one minute");
                }
                validate(full)
            }
            SqlProtection::DailyRestorePoints { schedule }
            | SqlProtection::SecondaryCopy { schedule } => validate(schedule),
        },
        JobSource::Files { .. } => validate(&job.schedule),
    }
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

    /// Unix time of a local date and time (June 2026 has no DST change in
    /// common time zones, so these hold wherever the tests run).
    fn at(day: u32, hour: u32, minute: u32) -> i64 {
        Local
            .with_ymd_and_hms(2026, 6, day, hour, minute, 0)
            .single()
            .unwrap()
            .timestamp()
    }

    #[test]
    fn daily_waits_for_today_then_catches_up() {
        let s = Schedule::Daily { hour: 2, minute: 0 };
        // Never run: not before today's time, then due.
        assert!(!schedule_due(&s, None, at(10, 1, 30)));
        assert!(schedule_due(&s, None, at(10, 2, 0)));
        // Ran today after the time: not due again until tomorrow.
        assert!(!schedule_due(&s, Some(at(10, 2, 5)), at(10, 23, 0)));
        assert!(schedule_due(&s, Some(at(10, 2, 5)), at(11, 2, 1)));
        // Missed yesterday (engine down): due right away, before today's time.
        assert!(schedule_due(&s, Some(at(8, 2, 5)), at(10, 1, 0)));
    }

    #[test]
    fn weekly_runs_on_the_chosen_days() {
        // 2026-06-01 is a Monday. Mon-Fri at 22:00.
        let s = Schedule::Weekly {
            days: vec![1, 2, 3, 4, 5],
            hour: 22,
            minute: 0,
        };
        // Never run, created Saturday: waits (no weekday occurrence today).
        assert!(!schedule_due(&s, None, at(6, 23, 0)));
        // Ran Friday night: nothing over the weekend, due Monday night.
        let friday = at(5, 22, 1);
        assert!(!schedule_due(&s, Some(friday), at(7, 23, 0)));
        assert!(!schedule_due(&s, Some(friday), at(8, 21, 0)));
        assert!(schedule_due(&s, Some(friday), at(8, 22, 0)));
        // Sunday only.
        let sunday = Schedule::Weekly {
            days: vec![0],
            hour: 3,
            minute: 30,
        };
        assert!(schedule_due(&sunday, None, at(7, 3, 30)));
        assert!(!schedule_due(&sunday, Some(at(7, 3, 31)), at(13, 23, 0)));
        assert!(schedule_due(&sunday, Some(at(7, 3, 31)), at(14, 3, 30)));
    }

    #[test]
    fn monthly_clamps_to_the_last_day() {
        let first = Schedule::Monthly {
            day: 1,
            hour: 1,
            minute: 0,
        };
        assert!(schedule_due(&first, None, at(1, 1, 0)));
        assert!(!schedule_due(&first, None, at(2, 1, 0)));
        assert!(!schedule_due(&first, Some(at(1, 1, 5)), at(30, 23, 0)));
        // Day 31 in a 30-day month runs on the 30th.
        let last = Schedule::Monthly {
            day: 31,
            hour: 2,
            minute: 0,
        };
        assert!(!schedule_due(&last, None, at(29, 3, 0)));
        assert!(schedule_due(&last, None, at(30, 2, 0)));
        assert_eq!(
            latest_occurrence(&last, at(30, 3, 0)),
            Some((at(30, 2, 0), true))
        );
    }

    #[test]
    fn validation_rejects_unrunnable_schedules() {
        assert!(validate(&Schedule::Daily {
            hour: 24,
            minute: 0
        })
        .is_err());
        assert!(validate(&Schedule::Interval { minutes: 0 }).is_err());
        let weekly = |days: Vec<u8>| Schedule::Weekly {
            days,
            hour: 1,
            minute: 0,
        };
        assert!(validate(&weekly(vec![])).is_err());
        assert!(validate(&weekly(vec![7])).is_err());
        assert!(validate(&weekly(vec![0, 6])).is_ok());
        let monthly = |day| Schedule::Monthly {
            day,
            hour: 1,
            minute: 0,
        };
        assert!(validate(&monthly(0)).is_err());
        assert!(validate(&monthly(32)).is_err());
        assert!(validate(&monthly(31)).is_ok());
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
