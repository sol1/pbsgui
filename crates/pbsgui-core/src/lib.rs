//! Shared core for the pbsgui backup engines.
//!
//! The config directory, the OS secret store, and the tamper-evident signed
//! on-disk stores, all parameterized by a per-app [`Profile`] so the SQL/files
//! engine (`pbsgui-engine`) and the Active Directory engine (`pbsgui-ad-engine`)
//! keep separate config directories and credential namespaces even when installed
//! on the same host.
//!
//! Each binary calls [`set_profile`] once at startup. Unset, the profile is
//! [`Profile::DEFAULT`], which is the SQL/files engine's identity, so that engine
//! (and unit tests) behave exactly as before this crate existed.

use std::sync::OnceLock;

pub mod config;
pub mod secrets;
pub mod signed;

/// The identity that separates one engine's on-disk state from another's.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    /// Subdirectory under `%ProgramData%` (or `$XDG_CONFIG_HOME`) for config.
    pub config_subdir: &'static str,
    /// Windows Credential Manager service name for this app's secrets.
    pub keyring_service: &'static str,
}

impl Profile {
    /// The SQL/files engine's identity, and the default when none is set.
    pub const DEFAULT: Profile = Profile {
        config_subdir: "pbsgui",
        keyring_service: "pbsgui",
    };
}

static PROFILE: OnceLock<Profile> = OnceLock::new();

/// Set this process's identity. Call once at startup, before any config or secret
/// access. A later call is ignored (first wins); reads before the first set see
/// [`Profile::DEFAULT`].
pub fn set_profile(p: Profile) {
    let _ = PROFILE.set(p);
}

/// This process's identity ([`Profile::DEFAULT`] if [`set_profile`] was not called).
pub fn profile() -> Profile {
    PROFILE.get().copied().unwrap_or(Profile::DEFAULT)
}
