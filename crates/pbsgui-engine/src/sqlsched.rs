//! Per-job timing state for SQL point-in-time jobs: when the last full and last
//! log backup ran, so the scheduler drives the two cadences independently.
//! Stored next to the change-detection state, keyed by job id.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::config_dir;

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    full: Option<i64>,
    #[serde(default)]
    log: Option<i64>,
    // Last attempt times (success OR failure). The scheduler gates re-runs on
    // these so a failed backup is not immediately due again (which otherwise
    // drives a retry storm, since the success timers above do not advance on
    // failure).
    #[serde(default)]
    full_attempt: Option<i64>,
    #[serde(default)]
    log_attempt: Option<i64>,
}

fn path(job_id: &str) -> PathBuf {
    config_dir()
        .join("state")
        .join(format!("{job_id}.sqlsched"))
}

fn load(job_id: &str) -> State {
    std::fs::read(path(job_id))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn store(job_id: &str, state: &State) -> anyhow::Result<()> {
    let p = path(job_id);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(p, serde_json::to_vec(state)?)?;
    Ok(())
}

/// When the last SUCCESSFUL full backup ran (the chain anchor), if any.
pub fn last_full(job_id: &str) -> Option<i64> {
    load(job_id).full
}

/// When a full backup was last ATTEMPTED (success or failure), if any. Falls back
/// to the last SUCCESS so state written before these attempt fields existed
/// (pre-0.0.6) is not read as "never attempted", which would make every existing
/// SQL job look due at once after an upgrade.
pub fn last_full_attempt(job_id: &str) -> Option<i64> {
    let s = load(job_id);
    s.full_attempt.or(s.full)
}

/// When a log backup was last ATTEMPTED (success or failure), if any. Falls back
/// to the last SUCCESS for pre-0.0.6 state (see [`last_full_attempt`]).
pub fn last_log_attempt(job_id: &str) -> Option<i64> {
    let s = load(job_id);
    s.log_attempt.or(s.log)
}

/// Record a SUCCESSFUL full backup at `time`. A full also anchors the log chain,
/// so the log timer is reset to it; the attempt timers reset too.
pub fn record_full(job_id: &str, time: i64) {
    let mut s = load(job_id);
    s.full = Some(time);
    s.log = Some(time);
    s.full_attempt = Some(time);
    s.log_attempt = Some(time);
    let _ = store(job_id, &s);
}

/// Record a SUCCESSFUL log backup at `time`.
pub fn record_log(job_id: &str, time: i64) {
    let mut s = load(job_id);
    s.log = Some(time);
    s.log_attempt = Some(time);
    let _ = store(job_id, &s);
}

/// Record a full backup ATTEMPT (called after a failed full so the scheduler does
/// not immediately re-run it; the success anchor is left untouched).
pub fn record_full_attempt(job_id: &str, time: i64) {
    let mut s = load(job_id);
    s.full_attempt = Some(time);
    let _ = store(job_id, &s);
}

/// Record a log backup ATTEMPT (called after a failed log).
pub fn record_log_attempt(job_id: &str, time: i64) {
    let mut s = load(job_id);
    s.log_attempt = Some(time);
    let _ = store(job_id, &s);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_falls_back_to_success_for_pre_upgrade_state() {
        let job = "test-sqlsched-fallback-7b2e9d";
        // State as written before 0.0.6: success timers set, attempt timers absent.
        store(
            job,
            &State {
                full: Some(1000),
                log: Some(900),
                full_attempt: None,
                log_attempt: None,
            },
        )
        .unwrap();
        // The scheduler gates due-ness on these; they must read the last success,
        // not None (None would make the job look never-run, hence due immediately).
        assert_eq!(last_full_attempt(job), Some(1000));
        assert_eq!(last_log_attempt(job), Some(900));
        // A real attempt, once recorded, takes precedence over the fallback.
        record_full_attempt(job, 2000);
        assert_eq!(last_full_attempt(job), Some(2000));
        let _ = std::fs::remove_file(path(job));
    }
}
