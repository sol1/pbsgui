//! Network restore destinations.
//!
//! The engine runs as LocalSystem, which reaches the network as the computer
//! account (`DOMAIN\HOST$`), so a restore to a UNC path like
//! `\\server\share\folder` succeeds on its own only if that machine account has
//! rights on the share. Supplying credentials lets a restore target any share the
//! given user can write, without changing the service account: we open a
//! connection to the share for the duration of the restore (Windows
//! `WNetAddConnection2W`) and tear it down after. Failures are mapped to messages
//! that name the actual cause (bad credentials, path not found, access denied,
//! already connected), replacing the old "folder does not exist" that masked all
//! of these.

use std::path::Path;

/// The share root (`\\server\share`) of a UNC path, or `None` if `path` is not a
/// UNC path. `WNetAddConnection2` connects to a share, not to a subfolder, so a
/// destination like `\\srv\backups\sql\nightly` connects to `\\srv\backups`.
pub fn unc_share_root(path: &str) -> Option<String> {
    let p = path.replace('/', "\\");
    let rest = p.strip_prefix("\\\\")?;
    let mut parts = rest.split('\\').filter(|s| !s.is_empty());
    let server = parts.next()?;
    let share = parts.next()?;
    Some(format!("\\\\{server}\\{share}"))
}

/// Prepare `destination` for a restore: if credentials are given, connect to its
/// share (holding the returned guard for the restore, dropping it disconnects),
/// then validate the folder is reachable and writable. Returns the connection
/// guard (if any) to keep alive until the restore finishes.
pub fn prepare_destination(
    destination: &str,
    creds: Option<&pbsgui_ipc::DestCredentials>,
) -> anyhow::Result<Option<DestConnection>> {
    let conn = match creds {
        Some(c) => {
            let root = unc_share_root(destination).ok_or_else(|| {
                anyhow::anyhow!(
                    "destination credentials only apply to a network path like \
                     \\\\server\\share\\folder; '{destination}' is not one"
                )
            })?;
            Some(DestConnection::connect(&root, &c.username, &c.password)?)
        }
        None => None,
    };
    validate_writable_dir(Path::new(destination))?;
    Ok(conn)
}

/// Ensure the destination folder exists (creating it if needed) and is writable,
/// with errors that name the likely cause instead of a bare "does not exist".
fn validate_writable_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| {
        anyhow::anyhow!(
            "cannot create or reach the destination folder {}: {e} (if it is a \
             network path, provide credentials with access, or check the service \
             account's rights on the share)",
            dir.display()
        )
    })?;
    // create_dir_all can succeed on a folder we still cannot write into (read
    // access only), so probe with a temp file and clean it up.
    let probe = dir.join(format!(".pbsgui-write-test-{}", std::process::id()));
    std::fs::File::create(&probe).map_err(|e| {
        anyhow::anyhow!(
            "cannot write to the destination folder {}: {e} (check the credentials \
             or the account's permissions)",
            dir.display()
        )
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// A connection to a network share, held open for the duration of a restore.
/// Dropping it disconnects.
#[cfg(windows)]
pub struct DestConnection {
    share_wide: Vec<u16>,
}

#[cfg(windows)]
impl DestConnection {
    fn connect(share_root: &str, username: &str, password: &str) -> anyhow::Result<Self> {
        use windows::core::{PCWSTR, PWSTR};
        use windows::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_ALREADY_ASSIGNED, ERROR_BAD_NETPATH, ERROR_BAD_NET_NAME,
            ERROR_LOGON_FAILURE, ERROR_SESSION_CREDENTIAL_CONFLICT, NO_ERROR,
        };
        use windows::Win32::NetworkManagement::WNet::{
            WNetAddConnection2W, NETRESOURCEW, NET_CONNECT_FLAGS, RESOURCETYPE_DISK,
        };

        let mut share_wide = wide(share_root);
        let user_wide = wide(username);
        let pass_wide = wide(password);
        let res = NETRESOURCEW {
            dwType: RESOURCETYPE_DISK,
            lpRemoteName: PWSTR(share_wide.as_mut_ptr()),
            ..Default::default()
        };
        // SAFETY: lpRemoteName and the user/password buffers are NUL-terminated
        // UTF-16 we own for the call. NET_CONNECT_FLAGS(0) is a transient
        // connection (not persisted across logons) with no mapped drive letter.
        let err = unsafe {
            WNetAddConnection2W(
                &res,
                PCWSTR(pass_wide.as_ptr()),
                PCWSTR(user_wide.as_ptr()),
                NET_CONNECT_FLAGS(0),
            )
        };
        if err == NO_ERROR {
            Ok(Self { share_wide })
        } else if err == ERROR_LOGON_FAILURE || err == ERROR_ACCESS_DENIED {
            anyhow::bail!(
                "access to {share_root} was denied: the username or password was rejected, \
                 or that account lacks permission on the share"
            )
        } else if err == ERROR_BAD_NETPATH || err == ERROR_BAD_NET_NAME {
            anyhow::bail!(
                "network path {share_root} was not found (check the server and share name)"
            )
        } else if err == ERROR_SESSION_CREDENTIAL_CONFLICT || err == ERROR_ALREADY_ASSIGNED {
            anyhow::bail!(
                "{share_root} is already connected on this host with different credentials; \
                 disconnect it first (net use {share_root} /delete)"
            )
        } else {
            anyhow::bail!(
                "could not connect to {share_root} (Windows error {})",
                err.0
            )
        }
    }
}

#[cfg(windows)]
impl Drop for DestConnection {
    fn drop(&mut self) {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::BOOL;
        use windows::Win32::NetworkManagement::WNet::{WNetCancelConnection2W, NET_CONNECT_FLAGS};
        // SAFETY: share_wide is NUL-terminated; fForce = FALSE so an in-use
        // connection is not force-closed (the restore has finished by now).
        unsafe {
            let _ = WNetCancelConnection2W(
                PCWSTR(self.share_wide.as_ptr()),
                NET_CONNECT_FLAGS(0),
                BOOL(0),
            );
        }
    }
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Off Windows there is no UNC/WNet, so credentialed network destinations are not
/// supported (the product runs on Windows; this keeps dev builds compiling).
#[cfg(not(windows))]
pub struct DestConnection;

#[cfg(not(windows))]
impl DestConnection {
    fn connect(_share_root: &str, _username: &str, _password: &str) -> anyhow::Result<Self> {
        anyhow::bail!("network destination credentials are only supported on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_share_root() {
        assert_eq!(
            unc_share_root(r"\\srv\backups\sql\nightly").as_deref(),
            Some(r"\\srv\backups")
        );
        assert_eq!(
            unc_share_root(r"\\srv\backups").as_deref(),
            Some(r"\\srv\backups")
        );
        // Forward slashes are accepted too.
        assert_eq!(
            unc_share_root("//srv/backups/x").as_deref(),
            Some(r"\\srv\backups")
        );
        // Not UNC / incomplete -> None (falls back to the service account path).
        assert_eq!(unc_share_root(r"C:\local\folder"), None);
        assert_eq!(unc_share_root(r"\\srv"), None);
        assert_eq!(unc_share_root("relative\\path"), None);
    }

    #[test]
    fn no_credentials_validates_a_local_dir() {
        let dir = std::env::temp_dir().join(format!("pbsgui-netdest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Creates the folder and confirms it is writable.
        let conn = prepare_destination(dir.to_str().unwrap(), None).unwrap();
        assert!(conn.is_none());
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
