//! Persistent stores of saved connections: SQL Server connections and PBS
//! servers (JSON, no secrets). Jobs reference these by id; secrets live in the
//! credential store under `sql:<id>` / `pbs:<id>`.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use pbsgui_ipc::{PbsServer, SqlConnection};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::config::config_dir;

/// Anything stored here is keyed by a stable id.
pub trait HasId {
    fn id(&self) -> &str;
}

impl HasId for SqlConnection {
    fn id(&self) -> &str {
        &self.id
    }
}

impl HasId for PbsServer {
    fn id(&self) -> &str {
        &self.id
    }
}

/// A list of `T` persisted as a JSON file, guarded by a mutex.
pub struct JsonStore<T> {
    path: PathBuf,
    items: Mutex<Vec<T>>,
    /// Set when the file exists but could not be loaded: the store is then
    /// read-only so the file survives for recovery.
    load_error: Option<String>,
}

impl<T: Clone + Serialize + DeserializeOwned + HasId> JsonStore<T> {
    pub fn with_path(path: PathBuf) -> Self {
        let (items, load_error) = crate::signed::load_list(&path);
        Self {
            path,
            items: Mutex::new(items),
            load_error,
        }
    }

    /// Why the store file could not be loaded, if it could not.
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Fail if the store is read-only (its file could not be loaded). Check
    /// this before side effects such as storing a secret for a new item.
    pub fn ensure_writable(&self) -> anyhow::Result<()> {
        match &self.load_error {
            Some(e) => anyhow::bail!("{e}"),
            None => Ok(()),
        }
    }

    pub fn list(&self) -> Vec<T> {
        self.items.lock().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<T> {
        self.items
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.id() == id)
            .cloned()
    }

    /// Insert or replace (matched by id), then persist.
    pub fn save(&self, item: T) -> anyhow::Result<()> {
        self.ensure_writable()?;
        {
            let mut items = self.items.lock().unwrap();
            match items.iter_mut().find(|i| i.id() == item.id()) {
                Some(slot) => *slot = item,
                None => items.push(item),
            }
        }
        self.persist()
    }

    /// Remove by id, then persist.
    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        self.ensure_writable()?;
        self.items.lock().unwrap().retain(|i| i.id() != id);
        self.persist()
    }

    fn persist(&self) -> anyhow::Result<()> {
        self.ensure_writable()?;
        // Hold the lock across the snapshot and the write so two concurrent
        // persists cannot reorder: without this a slower writer could land an
        // older state on disk after a newer one (a lost update). The mutators
        // release the lock before calling persist, so this does not re-enter.
        let items = self.items.lock().unwrap();
        let data = crate::signed::serialize(&*items)?;
        crate::signed::write_atomic(&self.path, &data)
    }
}

static SQL_CONNECTIONS: OnceLock<JsonStore<SqlConnection>> = OnceLock::new();
static PBS_SERVERS: OnceLock<JsonStore<PbsServer>> = OnceLock::new();

/// The shared store of saved SQL Server connections.
pub fn sql_connections() -> &'static JsonStore<SqlConnection> {
    SQL_CONNECTIONS.get_or_init(|| JsonStore::with_path(config_dir().join("sql-connections.json")))
}

/// The shared store of saved PBS servers.
pub fn pbs_servers() -> &'static JsonStore<PbsServer> {
    PBS_SERVERS.get_or_init(|| JsonStore::with_path(config_dir().join("pbs-servers.json")))
}

/// The credential-store key for a SQL connection's password.
pub fn sql_secret_key(id: &str) -> String {
    format!("sql:{id}")
}

/// The credential-store key for a PBS server's token secret.
pub fn pbs_secret_key(id: &str) -> String {
    format!("pbs:{id}")
}
