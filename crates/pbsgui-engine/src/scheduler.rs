//! Runs jobs on their schedule while the engine is running.
//!
//! This covers interactive/served scheduling. Unattended scheduling that
//! survives logoff/reboot arrives with the Windows Service.

use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, Timelike};
use pbsgui_ipc::{Job, Reply, Schedule};
use tokio::sync::mpsc;

use crate::config::unix_now;
use crate::jobstore::JobStore;
use crate::{backup, secrets};

/// Check every minute and run any due jobs.
pub async fn run(store: Arc<JobStore>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        let now = unix_now();
        for job in store.list() {
            if !is_due(&job, now) {
                continue;
            }
            run_due(&store, job, now).await;
        }
    }
}

async fn run_due(store: &Arc<JobStore>, job: Job, now: i64) {
    let secret = match secrets::get(&job.id) {
        Ok(Some(secret)) => secret,
        _ => {
            tracing::warn!(job = %job.id, "scheduled job has no stored credential; skipping");
            let _ = store.record_run(&job.id, now, "no stored credential".to_string());
            return;
        }
    };

    tracing::info!(job = %job.name, "running scheduled job");
    let (tx, mut rx) = mpsc::channel::<Reply>(64);
    let job_for_run = job.clone();
    let handle = tokio::spawn(async move { backup::run_job(&job_for_run, &secret, tx).await });

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
    let _ = store.record_run(&job.id, unix_now(), status);
}

fn is_due(job: &Job, now: i64) -> bool {
    match &job.schedule {
        Schedule::Manual => false,
        Schedule::Interval { minutes } => {
            let interval = (*minutes as i64).max(1) * 60;
            match job.last_run {
                None => true,
                Some(last) => now - last >= interval,
            }
        }
        Schedule::Daily { hour, minute } => {
            let scheduled = today_scheduled_unix(*hour, *minute);
            now >= scheduled
                && match job.last_run {
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
    use pbsgui_ipc::PbsDestination;

    fn job_with(schedule: Schedule, last_run: Option<i64>) -> Job {
        Job {
            id: "j".into(),
            name: "j".into(),
            destination: PbsDestination {
                repository: "u@pbs!t@host:8007:store".into(),
                fingerprint: "ab".repeat(32),
                backup_id: "host".into(),
            },
            sources: vec!["/data".into()],
            excludes: vec![],
            schedule,
            last_run,
            last_status: None,
        }
    }

    #[test]
    fn manual_is_never_due() {
        assert!(!is_due(&job_with(Schedule::Manual, None), 1_000_000));
    }

    #[test]
    fn interval_due_when_elapsed() {
        let now = 1_000_000;
        let sched = Schedule::Interval { minutes: 30 };
        assert!(is_due(&job_with(sched.clone(), None), now)); // never run
        assert!(is_due(&job_with(sched.clone(), Some(now - 31 * 60)), now));
        assert!(!is_due(&job_with(sched, Some(now - 10 * 60)), now));
    }
}
