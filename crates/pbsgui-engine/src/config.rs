//! Config directory and time helpers.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// The per-machine config directory for pbsgui.
///
/// Windows: `%ProgramData%\pbsgui`. Elsewhere: `$XDG_CONFIG_HOME/pbsgui` or
/// `~/.config/pbsgui` (a temp dir as last resort).
pub fn config_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(std::env::temp_dir)
    };
    base.join("pbsgui")
}

/// Current time in unix seconds.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
