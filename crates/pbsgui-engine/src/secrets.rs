//! Secret storage for PBS API token secrets.
//!
//! On Windows the secret lives in the OS credential store (Windows Credential
//! Manager) via the `keyring` crate, keyed by job id - it never touches the job
//! config file. On other platforms (development only) a plaintext fallback file
//! is used, with a warning, so the engine still builds and runs.

#[cfg(windows)]
mod imp {
    use keyring::{Entry, Error};

    const SERVICE: &str = "pbsgui";

    pub fn set(key: &str, secret: &str) -> anyhow::Result<()> {
        Entry::new(SERVICE, key)?.set_password(secret)?;
        Ok(())
    }

    pub fn get(key: &str) -> anyhow::Result<Option<String>> {
        match Entry::new(SERVICE, key)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete(key: &str) -> anyhow::Result<()> {
        match Entry::new(SERVICE, key)?.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(not(windows))]
mod imp {
    //! Insecure development fallback. The product runs on Windows, where secrets
    //! go to the Credential Manager instead of this file.
    use std::collections::HashMap;

    use crate::config::config_dir;

    fn path() -> std::path::PathBuf {
        config_dir().join("secrets-dev.json")
    }

    fn load() -> HashMap<String, String> {
        std::fs::read(path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn store(map: &HashMap<String, String>) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_dir())?;
        std::fs::write(path(), serde_json::to_vec_pretty(map)?)?;
        Ok(())
    }

    pub fn set(key: &str, secret: &str) -> anyhow::Result<()> {
        tracing::warn!("storing secret in an insecure dev file (non-Windows build)");
        let mut map = load();
        map.insert(key.to_string(), secret.to_string());
        store(&map)
    }

    pub fn get(key: &str) -> anyhow::Result<Option<String>> {
        Ok(load().get(key).cloned())
    }

    pub fn delete(key: &str) -> anyhow::Result<()> {
        let mut map = load();
        map.remove(key);
        store(&map)
    }
}

pub use imp::{delete, get, set};
