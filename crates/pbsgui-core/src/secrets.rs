//! Secret storage (PBS token secrets, SMTP passwords, the store-signing key, ...).
//!
//! On Windows the secret lives in the OS credential store (Windows Credential
//! Manager) via the `keyring` crate, under the active [`crate::Profile`]'s
//! `keyring_service` - it never touches a config file. On other platforms
//! (development only) a plaintext fallback file is used, with a warning, so the
//! engines still build and run.

#[cfg(windows)]
mod imp {
    use keyring::{Entry, Error};

    fn service() -> &'static str {
        crate::profile().keyring_service
    }

    pub fn set(key: &str, secret: &str) -> anyhow::Result<()> {
        Entry::new(service(), key)?.set_password(secret)?;
        Ok(())
    }

    pub fn get(key: &str) -> anyhow::Result<Option<String>> {
        match Entry::new(service(), key)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete(key: &str) -> anyhow::Result<()> {
        match Entry::new(service(), key)?.delete_credential() {
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
