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

/// When the last full backup ran (unix seconds), if any.
pub fn last_full(job_id: &str) -> Option<i64> {
    load(job_id).full
}

/// When the last log backup ran (unix seconds), if any.
pub fn last_log(job_id: &str) -> Option<i64> {
    load(job_id).log
}

/// Record a full backup at `time`. A full also anchors the log chain, so the log
/// timer is reset to it.
pub fn record_full(job_id: &str, time: i64) {
    let mut s = load(job_id);
    s.full = Some(time);
    s.log = Some(time);
    let _ = store(job_id, &s);
}

/// Record a log backup at `time`.
pub fn record_log(job_id: &str, time: i64) {
    let mut s = load(job_id);
    s.log = Some(time);
    let _ = store(job_id, &s);
}
