//! Persistent store of backup jobs (JSON, no secrets).

use std::path::PathBuf;
use std::sync::Mutex;

use pbsgui_ipc::Job;

use crate::config::config_dir;

/// Stores jobs in a JSON file, guarded by a mutex.
pub struct JobStore {
    path: PathBuf,
    jobs: Mutex<Vec<Job>>,
}

impl JobStore {
    /// Load the store from the default config location.
    pub fn load() -> Self {
        Self::with_path(config_dir().join("jobs.json"))
    }

    /// Load the store from a specific path.
    pub fn with_path(path: PathBuf) -> Self {
        let jobs = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Job>>(&b).ok())
            .unwrap_or_default();
        Self {
            path,
            jobs: Mutex::new(jobs),
        }
    }

    pub fn list(&self) -> Vec<Job> {
        self.jobs.lock().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    /// Insert or replace a job (matched by id), then persist.
    pub fn save_job(&self, job: Job) -> anyhow::Result<()> {
        {
            let mut jobs = self.jobs.lock().unwrap();
            match jobs.iter_mut().find(|j| j.id == job.id) {
                Some(slot) => *slot = job,
                None => jobs.push(job),
            }
        }
        self.persist()
    }

    /// Remove a job by id, then persist.
    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        self.jobs.lock().unwrap().retain(|j| j.id != id);
        self.persist()
    }

    /// Record the outcome of a run, then persist.
    pub fn record_run(&self, id: &str, time: i64, status: String) -> anyhow::Result<()> {
        {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
                job.last_run = Some(time);
                job.last_status = Some(status);
            }
        }
        self.persist()
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = {
            let jobs = self.jobs.lock().unwrap();
            serde_json::to_vec_pretty(&*jobs)?
        };
        std::fs::write(&self.path, data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbsgui_ipc::{JobDestination, JobSource, Schedule};

    fn job(id: &str) -> Job {
        Job {
            id: id.into(),
            name: format!("job {id}"),
            source: JobSource::Files {
                sources: vec!["/data".into()],
                excludes: vec![],
                change_detection: false,
            },
            destination: JobDestination::Pbs {
                server_id: "s".into(),
                backup_id: "host".into(),
            },
            schedule: Schedule::Manual,
            pre_script: None,
            post_script: None,
            last_run: None,
            last_status: None,
            encrypted: false,
        }
    }

    #[test]
    fn crud_and_reload() {
        let dir = std::env::temp_dir().join(format!("pbsgui-jobstore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("jobs.json");

        let store = JobStore::with_path(path.clone());
        assert!(store.list().is_empty());
        store.save_job(job("a")).unwrap();
        store.save_job(job("b")).unwrap();
        assert_eq!(store.list().len(), 2);

        // Reload from disk.
        let store2 = JobStore::with_path(path.clone());
        assert_eq!(store2.list().len(), 2);

        store2.record_run("a", 123, "ok".into()).unwrap();
        assert_eq!(store2.get("a").unwrap().last_run, Some(123));

        store2.delete("a").unwrap();
        assert_eq!(store2.list().len(), 1);
        assert!(store2.get("a").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
