//! Identity for this agent.
//!
//! These values keep the AD engine's on-disk config, credential-store namespace,
//! Windows service, and IPC socket separate from the SQL/files engine, so both can
//! run on the same host (unusual, but a DC can also be a file server) without
//! colliding. When the shared core crate lands (M1) these feed its `Profile`.

/// Config directory subdirectory (under `%ProgramData%`, or `$XDG_CONFIG_HOME`).
pub const CONFIG_SUBDIR: &str = "pbsgui-ad";

/// Windows Credential Manager service name for this agent's secrets.
pub const KEYRING_SERVICE: &str = "pbsgui-ad";

/// Windows service name (as registered with the SCM).
pub const SERVICE_NAME: &str = "pbsgui-ad-engine";

/// Windows service display name.
pub const SERVICE_DISPLAY: &str = "pbsgui Active Directory backup engine";

/// IPC socket base name. The `-vN` suffix is the protocol version, bumped on any
/// incompatible change (kept distinct from the SQL/files engine's socket).
pub const SOCKET: &str = "pbsgui-ad-engine-v1";
