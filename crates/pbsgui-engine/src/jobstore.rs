//! Persistent store of backup jobs (JSON, no secrets).

use std::path::PathBuf;
use std::sync::Mutex;

use pbsgui_ipc::Job;

use crate::config::config_dir;

/// Stores jobs in a JSON file, guarded by a mutex.
pub struct JobStore {
    path: PathBuf,
    jobs: Mutex<Vec<Job>>,
    /// Set when the file exists but could not be loaded: the store is then
    /// read-only so the file survives for recovery.
    load_error: Option<String>,
}

impl JobStore {
    /// Load the store from the default config location.
    pub fn load() -> Self {
        Self::with_path(config_dir().join("jobs.json"))
    }

    /// Load the store from a specific path.
    pub fn with_path(path: PathBuf) -> Self {
        let (jobs, load_error) = crate::signed::load_list(&path);
        Self {
            path,
            jobs: Mutex::new(jobs),
            load_error,
        }
    }

    /// Why the store file could not be loaded, if it could not.
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Fail if the store is read-only (its file could not be loaded). The
    /// mutators check this first so a refused change never reaches memory.
    fn ensure_writable(&self) -> anyhow::Result<()> {
        match &self.load_error {
            Some(e) => anyhow::bail!("{e}"),
            None => Ok(()),
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
        self.ensure_writable()?;
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
        self.ensure_writable()?;
        self.jobs.lock().unwrap().retain(|j| j.id != id);
        self.persist()
    }

    /// Record the outcome of a run, then persist.
    pub fn record_run(&self, id: &str, time: i64, status: String) -> anyhow::Result<()> {
        self.ensure_writable()?;
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
        self.ensure_writable()?;
        // Hold the lock across the snapshot and the write so two concurrent
        // persists cannot reorder: without this a slower writer could land an
        // older state on disk after a newer one (a lost update). The mutators
        // release the lock before calling persist, so this does not re-enter.
        let jobs = self.jobs.lock().unwrap();
        let data = crate::signed::serialize(&*jobs)?;
        crate::signed::write_atomic(&self.path, &data)
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
            last_run: None,
            last_status: None,
            encrypted: false,
            compress: true,
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

    #[test]
    fn unverifiable_store_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("pbsgui-jobstore-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.json");
        // A signed envelope whose MAC does not match, as seen by a service
        // account with a different signing key.
        let original = br#"{"mac":"AAAA","data":"[]"}"#;
        std::fs::write(&path, original).unwrap();

        let store = JobStore::with_path(path.clone());
        assert!(store.list().is_empty());
        let err = store.load_error().expect("load error recorded").to_string();
        assert!(err.contains("will not change this file"), "{err}");
        assert!(store.save_job(job("a")).is_err());
        assert!(store.record_run("a", 1, "ok".into()).is_err());
        assert!(store.delete("a").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
