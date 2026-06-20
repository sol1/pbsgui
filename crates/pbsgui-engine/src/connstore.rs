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
}

impl<T: Clone + Serialize + DeserializeOwned + HasId> JsonStore<T> {
    pub fn with_path(path: PathBuf) -> Self {
        let items = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<T>>(&b).ok())
            .unwrap_or_default();
        Self {
            path,
            items: Mutex::new(items),
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
        self.items.lock().unwrap().retain(|i| i.id() != id);
        self.persist()
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = {
            let items = self.items.lock().unwrap();
            serde_json::to_vec_pretty(&*items)?
        };
        std::fs::write(&self.path, data)?;
        Ok(())
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
