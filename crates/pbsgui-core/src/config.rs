//! Config directory and time helpers.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// The per-machine config directory for this app (the subdirectory comes from the
/// active [`crate::Profile`]).
///
/// Windows: `%ProgramData%\<subdir>`. Elsewhere: `$XDG_CONFIG_HOME/<subdir>` or
/// `~/.config/<subdir>` (a temp dir as last resort).
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
    base.join(crate::profile().config_subdir)
}

/// Create the config directory and, on Windows, restrict its ACL to SYSTEM and
/// Administrators so a non-administrator cannot plant or alter files the
/// LocalSystem engine reads (e.g. the job store). Best effort: failures are
/// logged, not fatal, so a non-elevated developer run still works.
///
/// Call this once at startup, before reading any config, and on every start so a
/// directory created with weaker permissions is re-hardened.
pub fn ensure_dirs() {
    let dir = config_dir();
    match std::fs::create_dir_all(&dir) {
        Ok(()) =>
        {
            #[cfg(windows)]
            if let Err(e) = harden_dir(&dir) {
                tracing::warn!("could not restrict permissions on {}: {e}", dir.display());
            }
        }
        Err(e) => tracing::warn!("could not create config dir {}: {e}", dir.display()),
    }
}

/// Restrict `path`'s DACL to SYSTEM and Builtin Administrators (full control),
/// with inheritance broken so the permissive default `%ProgramData%` ACEs (which
/// let the Users group create files) do not apply. The inheritable ACEs
/// propagate to files and subdirectories created later.
#[cfg(windows)]
fn harden_dir(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, BOOL, HLOCAL, WIN32_ERROR};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    // SYSTEM (SY) and Builtin Administrators (BA) get full control (FA), inherited
    // by child objects and containers (OICI). The protection flag below, not the
    // SDDL, breaks inheritance from the parent.
    const SDDL: &str = "D:(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
    let sddl_wide: Vec<u16> = SDDL.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: `sddl_wide` is a valid NUL-terminated UTF-16 string; `psd` receives a
    // newly allocated self-relative descriptor we free with LocalFree below.
    let mut psd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_wide.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )?;
    }

    let result = (|| -> anyhow::Result<()> {
        // Pull the DACL out of the parsed descriptor.
        let mut present = BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut defaulted = BOOL(0);
        // SAFETY: `psd` is a valid descriptor from the call above.
        unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted)? };
        if !present.as_bool() || dacl.is_null() {
            anyhow::bail!("parsed security descriptor had no DACL");
        }

        let mut path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        path_wide.push(0);

        // Set the DACL and mark it protected (PROTECTED_DACL_SECURITY_INFORMATION =
        // break inheritance), propagating the inheritable ACEs to existing children.
        // SAFETY: `path_wide` is NUL-terminated; `dacl` points into the live `psd`.
        let err = unsafe {
            SetNamedSecurityInfoW(
                PCWSTR(path_wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                PSID::default(),
                PSID::default(),
                Some(dacl as *const ACL),
                None,
            )
        };
        if err != WIN32_ERROR(0) {
            anyhow::bail!("SetNamedSecurityInfoW failed (error {})", err.0);
        }
        Ok(())
    })();

    // SAFETY: `psd.0` was allocated by ConvertStringSecurityDescriptor... above.
    unsafe {
        let _ = LocalFree(HLOCAL(psd.0));
    }
    result
}

/// Current time in unix seconds.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
