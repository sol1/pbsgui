//! The Windows implementation: COM setup, async helpers, and the volume
//! snapshot requester.

use std::path::{Path, PathBuf};
use std::sync::Once;

use anyhow::Context as _;
use windows::core::{Interface, BSTR, GUID, HRESULT, PCWSTR};
use windows::Win32::Storage::Vss::{IVssAsync, VSS_BT_COPY, VSS_SNAPSHOT_PROP, VSS_WRITER_STATE};
use windows::Win32::System::Com::{
    CoInitializeEx, CoInitializeSecurity, CoUninitialize, COINIT_MULTITHREADED, EOAC_NONE,
    RPC_C_AUTHN_LEVEL_PKT_PRIVACY, RPC_C_IMP_LEVEL_IDENTIFY,
};

use crate::{bindings, shadow_path};

// Generous asynchronous-phase timeouts: metadata phases are quick; the snapshot
// itself (writer freeze + provider work) gets longer. A wedged VSS then surfaces
// as a clear error instead of a hang.
pub const ASYNC_METADATA_MS: u32 = 5 * 60 * 1000;
pub const ASYNC_SNAPSHOT_MS: u32 = 15 * 60 * 1000;
/// VSS_S_ASYNC_FINISHED: the async operation completed successfully.
const VSS_S_ASYNC_FINISHED: i32 = 0x0004_230A;

/// Set the process-wide COM security level VSS documents for requesters, once,
/// and keep the multithreaded apartment alive for the life of the process.
///
/// Writers call back into the requester over COM, and `CoInitializeSecurity`
/// only takes effect if it runs before any other COM activity in the process
/// sets the defaults. A long-running service that also uses COM elsewhere (SQL
/// VDI) should call this at startup. A parked thread holds the MTA so the
/// setting is never torn down by the last COM user uninitializing.
pub fn init_process_security() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("com-mta".into())
            .spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    set_security();
                }
                let _ = tx.send(());
                loop {
                    std::thread::park();
                }
            });
        if spawned.is_ok() {
            let _ = rx.recv();
        }
    });
}

/// `CoInitializeSecurity` with the requester settings. Best effort:
/// RPC_E_TOO_LATE means the process already has a security level.
unsafe fn set_security() {
    let _ = CoInitializeSecurity(
        None,
        -1,
        None,
        None,
        RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
        RPC_C_IMP_LEVEL_IDENTIFY,
        None,
        EOAC_NONE,
        None,
    );
}

/// Run `f` on this thread inside a COM MTA. VSS requesters need COM initialized,
/// and CoUninitialize must come after every interface is released, so the scope
/// is explicit rather than hidden in a Drop.
pub fn with_com<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    // S_FALSE (already initialized on this thread) is not a failure.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx failed")?;
        set_security();
    }
    let result = f();
    unsafe { CoUninitialize() };
    result
}

/// Run a method that returns an `IVssAsync` out-parameter.
///
/// # Safety
/// `f` must be a VSS method that fills its argument with an `IVssAsync`.
pub unsafe fn call_async(
    f: impl FnOnce(*mut *mut core::ffi::c_void) -> HRESULT,
) -> anyhow::Result<IVssAsync> {
    let mut raw = std::ptr::null_mut();
    f(&mut raw)
        .ok()
        .context("starting the async operation failed")?;
    Ok(IVssAsync::from_raw(raw))
}

/// Wait for a VSS async phase and verify it FINISHED (Wait returning is not
/// enough; the operation's own HRESULT arrives via QueryStatus).
///
/// # Safety
/// Call on the COM thread that owns the backup components object.
pub unsafe fn wait_async(
    async_op: anyhow::Result<IVssAsync>,
    what: &str,
    timeout_ms: u32,
) -> anyhow::Result<()> {
    let op = async_op.with_context(|| format!("{what} did not start"))?;
    op.Wait(timeout_ms)
        .with_context(|| format!("{what} wait failed"))?;
    let mut hr = HRESULT(0);
    op.QueryStatus(&mut hr, std::ptr::null_mut())
        .with_context(|| format!("{what} status query failed"))?;
    if hr.0 != VSS_S_ASYNC_FINISHED {
        anyhow::bail!("{what} did not finish: {}", HRESULT(hr.0).message());
    }
    Ok(())
}

/// A NUL-terminated UTF-16 copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a NUL-terminated UTF-16 string (empty for null).
///
/// # Safety
/// `p` must be null or point at a NUL-terminated UTF-16 string.
pub unsafe fn pwsz_to_string(p: *mut u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

/// The volume mount root of `path` (e.g. `C:\`), which is what VSS snapshots.
pub fn volume_of(path: &str) -> anyhow::Result<String> {
    use windows::Win32::Storage::FileSystem::GetVolumePathNameW;
    let wide_path = wide(path);
    let mut root = vec![0u16; 512];
    unsafe { GetVolumePathNameW(PCWSTR(wide_path.as_ptr()), &mut root) }
        .with_context(|| format!("GetVolumePathNameW({path}) failed"))?;
    let len = root.iter().position(|&c| c == 0).unwrap_or(root.len());
    Ok(String::from_utf16_lossy(&root[..len]))
}

/// Writers that failed in the current backup session, as display strings. The
/// VSS_WS_FAILED_* states are 6..=15; a writer can report one with an S_OK
/// failure hresult, so both are checked.
///
/// # Safety
/// Call on the COM thread that owns `backup`, after the snapshot.
pub unsafe fn failed_writers(
    backup: &bindings::IVssBackupComponents,
) -> anyhow::Result<Vec<(GUID, String)>> {
    wait_async(
        call_async(|out| backup.GatherWriterStatus(out)),
        "GatherWriterStatus",
        ASYNC_METADATA_MS,
    )?;
    let mut count = 0u32;
    backup
        .GetWriterStatusCount(&mut count)
        .ok()
        .context("GetWriterStatusCount failed")?;
    let mut failures = Vec::new();
    for i in 0..count {
        let (mut inst, mut writer) = (GUID::zeroed(), GUID::zeroed());
        let mut name = BSTR::default();
        let mut state = VSS_WRITER_STATE::default();
        let mut hr = HRESULT(0);
        backup
            .GetWriterStatus(i, &mut inst, &mut writer, &mut name, &mut state, &mut hr)
            .ok()
            .context("GetWriterStatus failed")?;
        if hr.is_err() || (6..=15).contains(&state.0) {
            failures.push((writer, format!("{name} (state {}, {hr})", state.0)));
        }
    }
    backup
        .FreeWriterStatus()
        .ok()
        .context("FreeWriterStatus failed")?;
    Ok(failures)
}

/// A shadow copy of one or more volumes, for consistent file reads.
///
/// A copy backup in non-component mode: every writer freezes so its files are
/// consistent in the image, but none records a backup (the SQL Server writer
/// marks it copy-only, so differential and log chains are untouched). Volumes
/// VSS cannot snapshot (FAT32, network shares) are left out and read live.
///
/// Must be created, used, and dropped on one thread inside [`with_com`].
/// Dropping without [`Self::complete`] aborts the session; either way the
/// shadow copies are released (auto-release context).
pub struct VolumeSnapshot {
    backup: bindings::IVssBackupComponents,
    /// (lowercase volume root like `c:\`, shadow device without a trailing
    /// separator) for each volume in the set.
    roots: Vec<(String, String)>,
    /// Volumes left out of the snapshot and writer problems, for the run log.
    pub warnings: Vec<String>,
    completed: bool,
}

impl VolumeSnapshot {
    /// Snapshot the volumes holding `paths`, as one consistent set.
    pub fn create(paths: &[&Path]) -> anyhow::Result<Self> {
        unsafe { Self::create_inner(paths) }
    }

    unsafe fn create_inner(paths: &[&Path]) -> anyhow::Result<Self> {
        let backup = bindings::create_backup_components()?;
        backup.InitializeForBackup(std::ptr::null()).ok().context(
            "InitializeForBackup failed (is the VSS service running, and is pbsgui elevated?)",
        )?;
        // VSS_CTX_BACKUP: auto-release, writer-involved snapshots.
        backup.SetContext(0).ok().context("SetContext failed")?;
        // Non-component mode, not a bootable system state, copy backup.
        backup
            .SetBackupState(0, 0, VSS_BT_COPY, 0)
            .ok()
            .context("SetBackupState failed")?;
        // Required before PrepareForBackup, even when no component is selected.
        wait_async(
            call_async(|out| backup.GatherWriterMetadata(out)),
            "GatherWriterMetadata",
            ASYNC_METADATA_MS,
        )?;
        let _ = backup.FreeWriterMetadata();

        let mut warnings = Vec::new();
        let mut volumes: Vec<String> = Vec::new();
        for path in paths {
            let text = path.to_string_lossy();
            match volume_of(&text) {
                Ok(vol) => {
                    if !volumes.iter().any(|v| v.eq_ignore_ascii_case(&vol)) {
                        volumes.push(vol);
                    }
                }
                Err(e) => {
                    warnings.push(format!("{text}: not on a local volume, read live ({e:#})"))
                }
            }
        }

        let mut snapshot_set = GUID::zeroed();
        backup
            .StartSnapshotSet(&mut snapshot_set)
            .ok()
            .context("StartSnapshotSet failed")?;
        let mut snapshot_ids = Vec::new();
        for vol in &volumes {
            let w = wide(vol);
            let mut id = GUID::zeroed();
            match backup
                .AddToSnapshotSet(PCWSTR(w.as_ptr()), GUID::zeroed(), &mut id)
                .ok()
            {
                Ok(()) => snapshot_ids.push((vol.clone(), id)),
                Err(e) => warnings.push(format!(
                    "{vol}: VSS cannot snapshot this volume, read live ({e})"
                )),
            }
        }

        let mut roots = Vec::new();
        if !snapshot_ids.is_empty() {
            wait_async(
                call_async(|out| backup.PrepareForBackup(out)),
                "PrepareForBackup",
                ASYNC_METADATA_MS,
            )?;
            // Writers freeze, the provider snapshots, writers thaw.
            wait_async(
                call_async(|out| backup.DoSnapshotSet(out)),
                "DoSnapshotSet",
                ASYNC_SNAPSHOT_MS,
            )?;
            // A failed writer leaves its files crash-consistent rather than
            // application-consistent: worth a warning, not a failed backup.
            for (_, w) in failed_writers(&backup)? {
                warnings.push(format!(
                    "VSS writer {w} failed; its files are crash-consistent only"
                ));
            }
            for (vol, id) in &snapshot_ids {
                let mut prop = VSS_SNAPSHOT_PROP::default();
                backup
                    .GetSnapshotProperties(*id, &mut prop)
                    .ok()
                    .with_context(|| format!("GetSnapshotProperties({vol}) failed"))?;
                let device = pwsz_to_string(prop.m_pwszSnapshotDeviceObject);
                bindings::free_snapshot_properties(&mut prop);
                roots.push((
                    vol.to_ascii_lowercase(),
                    device.trim_end_matches('\\').to_string(),
                ));
            }
        }

        Ok(Self {
            backup,
            roots,
            warnings,
            completed: false,
        })
    }

    /// Whether any volume made it into the snapshot.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The shadow-copy path for a live `path`, or `None` when its volume is not
    /// in the snapshot (read it live).
    pub fn map(&self, path: &Path) -> Option<PathBuf> {
        let text = path.to_string_lossy();
        let vol = volume_of(&text).ok()?.to_ascii_lowercase();
        let (_, device) = self.roots.iter().find(|(v, _)| *v == vol)?;
        Some(PathBuf::from(shadow_path(device, &vol, &text)?))
    }

    /// Finish the session (the writers see a completed copy backup) and release
    /// the shadow copies.
    pub fn complete(mut self) -> anyhow::Result<()> {
        self.completed = true;
        if self.roots.is_empty() {
            return Ok(());
        }
        unsafe {
            wait_async(
                call_async(|out| self.backup.BackupComplete(out)),
                "BackupComplete",
                ASYNC_METADATA_MS,
            )
        }
    }
}

impl Drop for VolumeSnapshot {
    fn drop(&mut self) {
        if !self.completed {
            unsafe {
                let _ = self.backup.AbortBackup();
            }
        }
    }
}
