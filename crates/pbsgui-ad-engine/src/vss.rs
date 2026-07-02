//! VSS System State capture: snapshot a live Domain Controller consistently and
//! resolve the System State file set (ntds.dit + logs, SYSVOL, registry, COM+,
//! WMI, and the System Writer's OS files) to readable shadow-copy paths.
//!
//! The requester API (`IVssBackupComponents` and friends from `vsbackup.h`) is
//! absent from the `windows` crate: Microsoft ships it as a C++ static library
//! (VssApi.lib), so it has no COM registration metadata. The interfaces are
//! therefore declared by hand below, transcribed from the mingw-w64 `vsbackup.h`
//! (which carries the exact vtable order and IIDs), and the entry points are the
//! `*Internal` exports of `vssapi.dll` that the static-lib wrappers call. The
//! same technique the SQL engine uses for SQLVDI. ABI notes: `boolean` is one
//! byte (u8), `WINBOOL` is four (BOOL); `VSS_ID` (GUID) parameters pass by value.
//!
//! Flow (one thread, COM MTA): InitializeForBackup -> SetBackupState(component
//! mode, bootable system state, full) -> GatherWriterMetadata -> select the
//! System State writers by their well-known IDs and AddComponent their top-level
//! components -> snapshot the union of their volumes -> resolve each component's
//! file descriptors against the shadow devices -> (caller reads/uploads) ->
//! SetBackupSucceeded + BackupComplete, so the writers see a real backup and AD
//! records it (a supported backup, visible in `repadmin /showbackup`, rather
//! than a file copy).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use windows::core::{Interface, BSTR, GUID, HRESULT, PCWSTR};
use windows::Win32::Storage::Vss::{
    IVssAsync, VSS_BT_FULL, VSS_COMPONENT_TYPE, VSS_SNAPSHOT_PROP, VSS_WRITER_STATE,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoInitializeSecurity, CoUninitialize, COINIT_MULTITHREADED, EOAC_NONE,
    RPC_C_AUTHN_LEVEL_PKT_PRIVACY, RPC_C_IMP_LEVEL_IDENTIFY,
};

use crate::matchspec::{expand_env, filespec_matches};

/// The in-box writers that make up a Domain Controller's System State, by their
/// documented, stable writer class IDs.
const NTDS_WRITER: GUID = GUID::from_u128(0xb2014c9e_8711_4c5c_a5a9_3cf384484757);
const DFSR_WRITER: GUID = GUID::from_u128(0x2707761b_2324_473d_88eb_eb007a359533);
const FRS_WRITER: GUID = GUID::from_u128(0xd76f5a28_3092_4589_ba48_2958fb88ce29);
const REGISTRY_WRITER: GUID = GUID::from_u128(0xafbab4a2_367d_4d15_a586_71dbb18f8485);
const COMPLUS_REGDB_WRITER: GUID = GUID::from_u128(0x542da469_d3e1_473c_9f4f_7847f01fc64f);
const WMI_WRITER: GUID = GUID::from_u128(0xa6ad56c2_b509_4e6c_bb19_49d8f43532f0);
const SYSTEM_WRITER: GUID = GUID::from_u128(0xe8132975_6f93_4464_a53e_1050253ae220);

const SYSTEM_STATE_WRITERS: &[GUID] = &[
    NTDS_WRITER,
    DFSR_WRITER,
    FRS_WRITER,
    REGISTRY_WRITER,
    COMPLUS_REGDB_WRITER,
    WMI_WRITER,
    SYSTEM_WRITER,
];

/// One file resolved from the snapshot, readable at `shadow_path`.
pub struct CapturedFile {
    /// Where to read the bytes (on the shadow-copy device).
    pub shadow_path: PathBuf,
    /// The live path the file came from (for the catalog and restore mapping).
    pub original_path: String,
    /// Deterministic name inside the backup archive: the original path with the
    /// drive colon dropped and forward slashes (`C:\Windows\NTDS\ntds.dit` ->
    /// `C/Windows/NTDS/ntds.dit`), so unchanged files chunk identically run to
    /// run and dedup holds.
    pub archive_name: String,
    pub size: u64,
    /// Display name of the writer that claimed the file.
    pub writer: String,
}

/// Per-writer resolution summary (for logs and the smoke test).
pub struct WriterSummary {
    pub name: String,
    pub components: usize,
    pub files: usize,
    pub bytes: u64,
}

/// A component we added to the backup set, remembered so its writer can be told
/// the outcome at completion.
struct AddedComponent {
    instance_id: GUID,
    writer_id: GUID,
    kind: VSS_COMPONENT_TYPE,
    logical_path: Option<Vec<u16>>,
    name: Vec<u16>,
}

/// A live VSS backup session with its snapshots created and files resolved.
///
/// The caller reads/uploads `files`, then calls [`Self::complete`]. Dropping the
/// session without completing aborts the backup, so writers are never left
/// believing a backup happened when nothing was persisted.
pub struct SystemStateCapture {
    backup: bindings::IVssBackupComponents,
    added: Vec<AddedComponent>,
    pub snapshot_set: GUID,
    pub files: Vec<CapturedFile>,
    pub writers: Vec<WriterSummary>,
    completed: bool,
}

/// Run `f` on this thread inside a COM MTA. VSS requesters need COM initialized,
/// and CoUninitialize must come after every interface is released, so the scope
/// is explicit rather than hidden in a Drop.
pub fn with_com<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    // S_FALSE (already initialized on this thread) is not a failure.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx failed")?
    };
    // Writers talk back to the requester over COM; set the security level VSS
    // documents for requesters. Best effort: RPC_E_TOO_LATE means another part of
    // the process already did it, which is fine.
    let _ = unsafe {
        CoInitializeSecurity(
            None,
            -1,
            None,
            None,
            RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
            RPC_C_IMP_LEVEL_IDENTIFY,
            None,
            EOAC_NONE,
            None,
        )
    };
    let result = f();
    unsafe { CoUninitialize() };
    result
}

impl SystemStateCapture {
    /// Create the snapshot: select the System State writers, snapshot their
    /// volumes, and resolve every claimed file to its shadow-copy path.
    /// `progress` receives human-readable step lines.
    pub fn create(progress: &mut dyn FnMut(String)) -> anyhow::Result<Self> {
        unsafe { Self::create_inner(progress) }
    }

    unsafe fn create_inner(progress: &mut dyn FnMut(String)) -> anyhow::Result<Self> {
        let backup = bindings::create_backup_components()?;
        backup
            .InitializeForBackup(std::ptr::null())
            .ok()
            .context("InitializeForBackup failed (is the VSS service running?)")?;
        // VSS_CTX_BACKUP: auto-release, writer-involved snapshots.
        backup.SetContext(0).ok().context("SetContext failed")?;
        // Component mode + bootable system state + full backup: writers report
        // their components and later record the backup (AD's backup timestamp).
        backup
            .SetBackupState(1, 1, VSS_BT_FULL, 0)
            .ok()
            .context("SetBackupState failed")?;

        progress("asking writers to describe their components".into());
        wait_async(
            call_async(|out| backup.GatherWriterMetadata(out)),
            "GatherWriterMetadata",
            ASYNC_METADATA_MS,
        )?;

        let (added, descriptors, writer_names) = select_system_state(&backup)?;
        if added.is_empty() {
            anyhow::bail!(
                "no System State writers reported components; is this machine a Domain Controller \
                 with the VSS service healthy? (vssadmin list writers)"
            );
        }
        let ntds = writer_names.iter().any(|(id, _)| *id == NTDS_WRITER);
        if !ntds {
            anyhow::bail!(
                "the NTDS writer is not present, so this machine does not look like a Domain \
                 Controller (vssadmin list writers should show 'NTDS')"
            );
        }
        progress(format!(
            "selected {} component(s) from {} writer(s)",
            added.len(),
            writer_names.len()
        ));

        // The volumes the claimed files live on, snapshot together so the whole
        // System State is one consistent point in time.
        let mut volumes: Vec<String> = Vec::new();
        for d in &descriptors {
            let vol = volume_of(&d.directory)?;
            if !volumes.iter().any(|v| v.eq_ignore_ascii_case(&vol)) {
                volumes.push(vol);
            }
        }

        let mut snapshot_set = GUID::zeroed();
        backup
            .StartSnapshotSet(&mut snapshot_set)
            .ok()
            .context("StartSnapshotSet failed")?;
        let mut snapshot_ids = Vec::new();
        for vol in &volumes {
            let wide = wide(vol);
            let mut id = GUID::zeroed();
            backup
                .AddToSnapshotSet(PCWSTR(wide.as_ptr()), GUID::zeroed(), &mut id)
                .ok()
                .with_context(|| format!("AddToSnapshotSet({vol}) failed"))?;
            snapshot_ids.push((vol.clone(), id));
        }
        progress(format!("snapshotting {} volume(s)", volumes.len()));

        wait_async(
            call_async(|out| backup.PrepareForBackup(out)),
            "PrepareForBackup",
            ASYNC_METADATA_MS,
        )?;
        // The writers freeze, the provider snapshots, the writers thaw. The only
        // part with a hard time budget; everything after reads the frozen image.
        wait_async(
            call_async(|out| backup.DoSnapshotSet(out)),
            "DoSnapshotSet",
            ASYNC_SNAPSHOT_MS,
        )?;
        check_writers_ok(&backup, &writer_names)?;

        // Map each volume root to its shadow device to build readable paths.
        let mut shadow_roots: BTreeMap<String, String> = BTreeMap::new();
        for (vol, id) in &snapshot_ids {
            let mut prop = VSS_SNAPSHOT_PROP::default();
            backup
                .GetSnapshotProperties(*id, &mut prop)
                .ok()
                .with_context(|| format!("GetSnapshotProperties({vol}) failed"))?;
            let device = pwsz_to_string(prop.m_pwszSnapshotDeviceObject);
            bindings::free_snapshot_properties(&mut prop);
            shadow_roots.insert(vol.to_ascii_lowercase(), device);
        }

        progress("resolving files on the shadow copy".into());
        let files = resolve_files(&descriptors, &shadow_roots, &writer_names)?;

        // Per-writer summary: component counts from the backup set, file counts
        // and bytes from the resolved list.
        let writers = writer_names
            .iter()
            .map(|(id, name)| WriterSummary {
                name: name.clone(),
                components: added.iter().filter(|c| c.writer_id == *id).count(),
                files: files.iter().filter(|f| &f.writer == name).count(),
                bytes: files
                    .iter()
                    .filter(|f| &f.writer == name)
                    .map(|f| f.size)
                    .sum(),
            })
            .collect();

        Ok(Self {
            backup,
            added,
            snapshot_set,
            files,
            writers,
            completed: false,
        })
    }

    /// The Backup Components Document as it stands now (post-snapshot). The NTDS
    /// writer stamps its backup metadata (e.g. the expiration time) during the
    /// snapshot events, so this is the version to store INSIDE the backup; a
    /// restore-time requester initializes from it.
    pub fn components_xml(&self) -> anyhow::Result<String> {
        unsafe {
            let mut xml = BSTR::default();
            self.backup
                .SaveAsXML(&mut xml)
                .ok()
                .context("SaveAsXML failed")?;
            Ok(xml.to_string())
        }
    }

    /// Tell every writer the backup outcome and finish the session. On success
    /// this is what makes the backup "real" to AD: the NTDS writer records the
    /// backup time (visible via `repadmin /showbackup`). Returns the Backup
    /// Components Document XML, which a restore-time requester needs; store it
    /// with the backup.
    pub fn complete(mut self, succeeded: bool) -> anyhow::Result<String> {
        unsafe {
            for c in &self.added {
                let logical = c
                    .logical_path
                    .as_ref()
                    .map(|w| PCWSTR(w.as_ptr()))
                    .unwrap_or(PCWSTR::null());
                self.backup
                    .SetBackupSucceeded(
                        c.instance_id,
                        c.writer_id,
                        c.kind,
                        logical,
                        PCWSTR(c.name.as_ptr()),
                        u8::from(succeeded),
                    )
                    .ok()
                    .context("SetBackupSucceeded failed")?;
            }
            wait_async(
                call_async(|out| self.backup.BackupComplete(out)),
                "BackupComplete",
                ASYNC_METADATA_MS,
            )?;
            self.completed = true;
            // Save the Backup Components Document after BackupComplete: writers
            // may stamp metadata (e.g. the NTDS backup expiration) during the
            // completion events, and a restore needs the final document.
            let mut xml = BSTR::default();
            self.backup
                .SaveAsXML(&mut xml)
                .ok()
                .context("SaveAsXML failed")?;
            Ok(xml.to_string())
        }
    }
}

impl Drop for SystemStateCapture {
    fn drop(&mut self) {
        if !self.completed {
            // Never let writers think an abandoned session was a backup.
            unsafe {
                let _ = self.backup.AbortBackup();
            }
        }
    }
}

/// One file descriptor from a selected component: a directory, a filespec, and
/// whether subdirectories are included.
struct FileDescriptor {
    writer_id: GUID,
    directory: String,
    spec: String,
    recursive: bool,
}

/// The backup set selection: the components added, their file descriptors, and
/// the involved writers' (id, display name) pairs.
type SelectedSystemState = (
    Vec<AddedComponent>,
    Vec<FileDescriptor>,
    Vec<(GUID, String)>,
);

/// Walk every writer's metadata, keep the System State writers, add their
/// top-level components to the backup set, and collect their file descriptors.
unsafe fn select_system_state(
    backup: &bindings::IVssBackupComponents,
) -> anyhow::Result<SelectedSystemState> {
    let mut added = Vec::new();
    let mut descriptors = Vec::new();
    let mut names: Vec<(GUID, String)> = Vec::new();

    let mut writer_count = 0u32;
    backup
        .GetWriterMetadataCount(&mut writer_count)
        .ok()
        .context("GetWriterMetadataCount failed")?;

    for i in 0..writer_count {
        let mut instance_id = GUID::zeroed();
        let mut raw = std::ptr::null_mut();
        backup
            .GetWriterMetadata(i, &mut instance_id, &mut raw)
            .ok()
            .context("GetWriterMetadata failed")?;
        let meta = bindings::IVssExamineWriterMetadata::from_raw(raw);

        let (mut id_instance, mut writer_id) = (GUID::zeroed(), GUID::zeroed());
        let mut name = BSTR::default();
        let (mut usage, mut source) = (0i32, 0i32);
        meta.GetIdentity(
            &mut id_instance,
            &mut writer_id,
            &mut name,
            &mut usage,
            &mut source,
        )
        .ok()
        .context("GetIdentity failed")?;
        if !SYSTEM_STATE_WRITERS.contains(&writer_id) {
            continue;
        }
        names.push((writer_id, name.to_string()));

        let (mut includes, mut excludes, mut components) = (0u32, 0u32, 0u32);
        meta.GetFileCounts(&mut includes, &mut excludes, &mut components)
            .ok()
            .context("GetFileCounts failed")?;

        for c in 0..components {
            let mut raw = std::ptr::null_mut();
            meta.GetComponent(c, &mut raw)
                .ok()
                .context("GetComponent failed")?;
            let comp = bindings::IVssWMComponent::from_raw(raw);

            let mut info = std::ptr::null();
            comp.GetComponentInfo(&mut info)
                .ok()
                .context("GetComponentInfo failed")?;
            let ci = &*info;
            let logical_path = pwsz_to_string(ci.bstrLogicalPath as *mut u16);
            let comp_name = pwsz_to_string(ci.bstrComponentName as *mut u16);
            let kind = ci.kind;
            let (n_files, n_dbs, n_logs) = (ci.cFileCount, ci.cDatabases, ci.cLogFiles);

            // Register the component with the backup set. System State writers
            // report top-level components (we add them all; a non-selectable
            // top-level component must be included for the backup to be valid).
            let logical_w = if logical_path.is_empty() {
                None
            } else {
                Some(wide(&logical_path))
            };
            let name_w = wide(&comp_name);
            backup
                .AddComponent(
                    instance_id,
                    writer_id,
                    kind,
                    logical_w
                        .as_ref()
                        .map(|w| PCWSTR(w.as_ptr()))
                        .unwrap_or(PCWSTR::null()),
                    PCWSTR(name_w.as_ptr()),
                )
                .ok()
                .with_context(|| format!("AddComponent({comp_name}) failed"))?;
            added.push(AddedComponent {
                instance_id,
                writer_id,
                kind,
                logical_path: logical_w,
                name: name_w,
            });

            // Collect the component's file, database, and log descriptors.
            let mut push_desc = |fd: &bindings::IVssWMFiledesc| -> anyhow::Result<()> {
                let (mut path, mut spec) = (BSTR::default(), BSTR::default());
                let mut recursive = 0u8;
                fd.GetPath(&mut path).ok().context("GetPath failed")?;
                fd.GetFilespec(&mut spec)
                    .ok()
                    .context("GetFilespec failed")?;
                fd.GetRecursive(&mut recursive)
                    .ok()
                    .context("GetRecursive failed")?;
                descriptors.push(FileDescriptor {
                    writer_id,
                    directory: expand_env(&path.to_string(), |v| std::env::var(v).ok()),
                    spec: spec.to_string(),
                    recursive: recursive != 0,
                });
                Ok(())
            };
            for f in 0..n_files {
                let mut raw = std::ptr::null_mut();
                comp.GetFile(f, &mut raw).ok().context("GetFile failed")?;
                push_desc(&bindings::IVssWMFiledesc::from_raw(raw))?;
            }
            for f in 0..n_dbs {
                let mut raw = std::ptr::null_mut();
                comp.GetDatabaseFile(f, &mut raw)
                    .ok()
                    .context("GetDatabaseFile failed")?;
                push_desc(&bindings::IVssWMFiledesc::from_raw(raw))?;
            }
            for f in 0..n_logs {
                let mut raw = std::ptr::null_mut();
                comp.GetDatabaseLogFile(f, &mut raw)
                    .ok()
                    .context("GetDatabaseLogFile failed")?;
                push_desc(&bindings::IVssWMFiledesc::from_raw(raw))?;
            }
            comp.FreeComponentInfo(info)
                .ok()
                .context("FreeComponentInfo failed")?;
        }
    }
    backup
        .FreeWriterMetadata()
        .ok()
        .context("FreeWriterMetadata failed")?;
    Ok((added, descriptors, names))
}

/// After the snapshot, confirm none of the involved writers failed; a writer
/// failure means the frozen image is not trustworthy.
unsafe fn check_writers_ok(
    backup: &bindings::IVssBackupComponents,
    names: &[(GUID, String)],
) -> anyhow::Result<()> {
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
        // The VSS_WS_FAILED_* states are 6..=15; a writer can report one with an
        // S_OK failure hresult, so check both.
        let failed = hr.is_err() || (6..=15).contains(&state.0);
        if failed && names.iter().any(|(id, _)| *id == writer) {
            failures.push(format!("{name} (state {}, {hr})", state.0));
        }
    }
    backup
        .FreeWriterStatus()
        .ok()
        .context("FreeWriterStatus failed")?;
    if !failures.is_empty() {
        anyhow::bail!(
            "writer(s) failed during the snapshot: {}",
            failures.join("; ")
        );
    }
    Ok(())
}

/// Resolve every descriptor against the shadow devices into concrete files,
/// deduplicated by archive name and sorted for a deterministic archive.
fn resolve_files(
    descriptors: &[FileDescriptor],
    shadow_roots: &BTreeMap<String, String>,
    names: &[(GUID, String)],
) -> anyhow::Result<Vec<CapturedFile>> {
    let mut files: BTreeMap<String, CapturedFile> = BTreeMap::new();
    for d in descriptors {
        let writer = names
            .iter()
            .find(|(id, _)| *id == d.writer_id)
            .map(|(_, n)| n.clone())
            .unwrap_or_default();
        let vol = volume_of(&d.directory)?;
        let Some(device) = shadow_roots.get(&vol.to_ascii_lowercase()) else {
            continue;
        };
        // C:\Windows\NTDS with volume root C:\ -> <device>\Windows\NTDS.
        let rel = d.directory[vol.len()..].trim_start_matches('\\');
        let shadow_dir = PathBuf::from(format!("{device}\\{rel}"));
        collect_matching(
            &shadow_dir,
            &d.directory,
            &d.spec,
            d.recursive,
            &writer,
            &mut files,
        );
    }
    Ok(files.into_values().collect())
}

/// Walk `shadow_dir` matching `spec`, recording matches keyed by archive name so
/// overlapping descriptors do not duplicate files. Missing directories are
/// normal (a writer can declare paths that do not exist on this machine).
fn collect_matching(
    shadow_dir: &Path,
    original_dir: &str,
    spec: &str,
    recursive: bool,
    writer: &str,
    files: &mut BTreeMap<String, CapturedFile>,
) {
    let entries = match std::fs::read_dir(shadow_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if recursive {
                collect_matching(
                    &entry.path(),
                    &format!("{}\\{}", original_dir.trim_end_matches('\\'), name),
                    spec,
                    true,
                    writer,
                    files,
                );
            }
            continue;
        }
        if !kind.is_file() || !filespec_matches(spec, &name) {
            continue;
        }
        let original_path = format!("{}\\{}", original_dir.trim_end_matches('\\'), name);
        let archive_name = original_path.replace(':', "").replace('\\', "/");
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        files.entry(archive_name.clone()).or_insert(CapturedFile {
            shadow_path: entry.path(),
            original_path,
            archive_name,
            size,
            writer: writer.to_string(),
        });
    }
}

/// The volume mount root of `path` (e.g. `C:\`), which is what VSS snapshots.
fn volume_of(path: &str) -> anyhow::Result<String> {
    use windows::Win32::Storage::FileSystem::GetVolumePathNameW;
    let wide_path = wide(path);
    let mut root = vec![0u16; 512];
    unsafe { GetVolumePathNameW(PCWSTR(wide_path.as_ptr()), &mut root) }
        .with_context(|| format!("GetVolumePathNameW({path}) failed"))?;
    let len = root.iter().position(|&c| c == 0).unwrap_or(root.len());
    Ok(String::from_utf16_lossy(&root[..len]))
}

// Generous asynchronous-phase timeouts: metadata phases are quick; the snapshot
// itself (writer freeze + provider work) gets longer. A wedged VSS then surfaces
// as a clear error instead of a hang.
const ASYNC_METADATA_MS: u32 = 5 * 60 * 1000;
const ASYNC_SNAPSHOT_MS: u32 = 15 * 60 * 1000;
/// VSS_S_ASYNC_FINISHED: the async operation completed successfully.
const VSS_S_ASYNC_FINISHED: i32 = 0x0004_230A;

/// Run a method that returns an `IVssAsync` out-parameter.
unsafe fn call_async(
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
unsafe fn wait_async(
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

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a NUL-terminated UTF-16 string (empty for null).
unsafe fn pwsz_to_string(p: *mut u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

/// Hand-written bindings for the `vsbackup.h` requester API (see the module doc
/// for why). Method order and IIDs transcribed from mingw-w64's `vsbackup.h`;
/// the vtable must list every method in declaration order, including ones this
/// engine never calls, or later methods would dispatch to the wrong slots.
mod bindings {
    #![allow(
        non_snake_case,
        dead_code,
        clippy::too_many_arguments,
        clippy::missing_safety_doc
    )]

    use windows::core::{
        interface, IUnknown, IUnknown_Vtbl, Interface, BSTR, GUID, HRESULT, PCWSTR,
    };
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::Storage::Vss::{
        VSS_BACKUP_TYPE, VSS_COMPONENT_TYPE, VSS_OBJECT_TYPE, VSS_RESTORE_TYPE, VSS_SNAPSHOT_PROP,
        VSS_WRITER_STATE,
    };

    /// `VSS_COMPONENTINFO` (vsbackup.h). The BSTR members stay raw pointers: the
    /// struct is owned by VSS and released via `FreeComponentInfo`, never by us.
    #[repr(C)]
    pub struct VSS_COMPONENTINFO {
        pub kind: VSS_COMPONENT_TYPE, // `type` in the header
        pub bstrLogicalPath: *const u16,
        pub bstrComponentName: *const u16,
        pub bstrCaption: *const u16,
        pub pbIcon: *mut u8,
        pub cbIcon: u32,
        pub bRestoreMetadata: u8,
        pub bNotifyOnBackupComplete: u8,
        pub bSelectable: u8,
        pub bSelectableForRestore: u8,
        pub dwComponentFlags: u32,
        pub cFileCount: u32,
        pub cDatabases: u32,
        pub cLogFiles: u32,
        pub cDependencies: u32,
    }

    // vsbackup.h declares IVssWMFiledesc and IVssWMComponent with NULL IIDs:
    // they are never QueryInterface'd, only returned through out-parameters, so
    // the IID is never sent anywhere. The macro requires distinct GUIDs, so the
    // second gets a fabricated ...0001; neither is ever used at runtime.
    #[interface("00000000-0000-0000-0000-000000000000")]
    pub unsafe trait IVssWMFiledesc: IUnknown {
        pub unsafe fn GetPath(&self, pbstrPath: *mut BSTR) -> HRESULT;
        pub unsafe fn GetFilespec(&self, pbstrFilespec: *mut BSTR) -> HRESULT;
        pub unsafe fn GetRecursive(&self, pbRecursive: *mut u8) -> HRESULT;
        pub unsafe fn GetAlternateLocation(&self, pbstrAlternateLocation: *mut BSTR) -> HRESULT;
        pub unsafe fn GetBackupTypeMask(&self, pdwTypeMask: *mut u32) -> HRESULT;
    }

    #[interface("00000000-0000-0000-0000-000000000001")]
    pub unsafe trait IVssWMComponent: IUnknown {
        pub unsafe fn GetComponentInfo(&self, ppInfo: *mut *const VSS_COMPONENTINFO) -> HRESULT;
        pub unsafe fn FreeComponentInfo(&self, pInfo: *const VSS_COMPONENTINFO) -> HRESULT;
        pub unsafe fn GetFile(
            &self,
            iFile: u32,
            ppFiledesc: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn GetDatabaseFile(
            &self,
            iDBFile: u32,
            ppFiledesc: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn GetDatabaseLogFile(
            &self,
            iDbLogFile: u32,
            ppFiledesc: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn GetDependency(
            &self,
            iDependency: u32,
            ppDependency: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
    }

    #[interface("902fcf7f-b7fd-42f8-81f1-b2e400b1e5bd")]
    pub unsafe trait IVssExamineWriterMetadata: IUnknown {
        pub unsafe fn GetIdentity(
            &self,
            pidInstance: *mut GUID,
            pidWriter: *mut GUID,
            pbstrWriterName: *mut BSTR,
            pUsage: *mut i32,  // VSS_USAGE_TYPE
            pSource: *mut i32, // VSS_SOURCE_TYPE
        ) -> HRESULT;
        pub unsafe fn GetFileCounts(
            &self,
            pcIncludeFiles: *mut u32,
            pcExcludeFiles: *mut u32,
            pcComponents: *mut u32,
        ) -> HRESULT;
        pub unsafe fn GetIncludeFile(
            &self,
            iFile: u32,
            ppFiledesc: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn GetExcludeFile(
            &self,
            iFile: u32,
            ppFiledesc: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn GetComponent(
            &self,
            iComponent: u32,
            ppComponent: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn GetRestoreMethod(
            &self,
            pMethod: *mut i32, // VSS_RESTOREMETHOD_ENUM
            pbstrService: *mut BSTR,
            pbstrUserProcedure: *mut BSTR,
            pwriterRestore: *mut i32, // VSS_WRITERRESTORE_ENUM
            pbRebootRequired: *mut u8,
            pcMappings: *mut u32,
        ) -> HRESULT;
        pub unsafe fn GetAlternateLocationMapping(
            &self,
            iMapping: u32,
            ppFiledesc: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn GetBackupSchema(&self, pdwSchemaMask: *mut u32) -> HRESULT;
        pub unsafe fn GetDocument(&self, pDoc: *mut *mut core::ffi::c_void) -> HRESULT;
        pub unsafe fn SaveAsXML(&self, pbstrXML: *mut BSTR) -> HRESULT;
        pub unsafe fn LoadFromXML(&self, bstrXML: *const u16) -> HRESULT;
    }

    #[interface("665c1d5f-c218-414d-a05d-7fef5f9d5c86")]
    pub unsafe trait IVssBackupComponents: IUnknown {
        pub unsafe fn GetWriterComponentsCount(&self, pcComponents: *mut u32) -> HRESULT;
        pub unsafe fn GetWriterComponents(
            &self,
            iWriter: u32,
            ppWriter: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn InitializeForBackup(&self, bstrXML: *const u16) -> HRESULT;
        pub unsafe fn SetBackupState(
            &self,
            bSelectComponents: u8,
            bBackupBootableSystemState: u8,
            backupType: VSS_BACKUP_TYPE,
            bPartialFileSupport: u8,
        ) -> HRESULT;
        pub unsafe fn InitializeForRestore(&self, bstrXML: *const u16) -> HRESULT;
        pub unsafe fn SetRestoreState(&self, restoreType: VSS_RESTORE_TYPE) -> HRESULT;
        pub unsafe fn GatherWriterMetadata(&self, pAsync: *mut *mut core::ffi::c_void) -> HRESULT;
        pub unsafe fn GetWriterMetadataCount(&self, pcWriters: *mut u32) -> HRESULT;
        pub unsafe fn GetWriterMetadata(
            &self,
            iWriter: u32,
            pidInstance: *mut GUID,
            ppMetadata: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn FreeWriterMetadata(&self) -> HRESULT;
        pub unsafe fn AddComponent(
            &self,
            instanceId: GUID,
            writerId: GUID,
            ct: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
        ) -> HRESULT;
        pub unsafe fn PrepareForBackup(&self, ppAsync: *mut *mut core::ffi::c_void) -> HRESULT;
        pub unsafe fn AbortBackup(&self) -> HRESULT;
        pub unsafe fn GatherWriterStatus(&self, pAsync: *mut *mut core::ffi::c_void) -> HRESULT;
        pub unsafe fn GetWriterStatusCount(&self, pcWriters: *mut u32) -> HRESULT;
        pub unsafe fn FreeWriterStatus(&self) -> HRESULT;
        pub unsafe fn GetWriterStatus(
            &self,
            iWriter: u32,
            pidInstance: *mut GUID,
            pidWriter: *mut GUID,
            pbstrWriter: *mut BSTR,
            pnStatus: *mut VSS_WRITER_STATE,
            phResultFailure: *mut HRESULT,
        ) -> HRESULT;
        pub unsafe fn SetBackupSucceeded(
            &self,
            instanceId: GUID,
            writerId: GUID,
            ct: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            bSucceded: u8,
        ) -> HRESULT;
        pub unsafe fn SetBackupOptions(
            &self,
            writerId: GUID,
            ct: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            wszBackupOptions: PCWSTR,
        ) -> HRESULT;
        pub unsafe fn SetSelectedForRestore(
            &self,
            writerId: GUID,
            ct: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            bSelectedForRestore: u8,
        ) -> HRESULT;
        pub unsafe fn SetRestoreOptions(
            &self,
            writerId: GUID,
            ct: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            wszRestoreOptions: PCWSTR,
        ) -> HRESULT;
        pub unsafe fn SetAdditionalRestores(
            &self,
            writerId: GUID,
            ct: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            bAdditionalRestores: u8,
        ) -> HRESULT;
        pub unsafe fn SetPreviousBackupStamp(
            &self,
            writerId: GUID,
            ct: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            wszPreviousBackupStamp: PCWSTR,
        ) -> HRESULT;
        pub unsafe fn SaveAsXML(&self, pbstrXML: *mut BSTR) -> HRESULT;
        pub unsafe fn BackupComplete(&self, ppAsync: *mut *mut core::ffi::c_void) -> HRESULT;
        pub unsafe fn AddAlternativeLocationMapping(
            &self,
            writerId: GUID,
            componentType: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            wszPath: PCWSTR,
            wszFilespec: PCWSTR,
            bRecursive: u8,
            wszDestination: PCWSTR,
        ) -> HRESULT;
        pub unsafe fn AddRestoreSubcomponent(
            &self,
            writerId: GUID,
            componentType: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            wszSubComponentLogicalPath: PCWSTR,
            wszSubComponentName: PCWSTR,
            bRepair: u8,
        ) -> HRESULT;
        pub unsafe fn SetFileRestoreStatus(
            &self,
            writerId: GUID,
            ct: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            status: i32, // VSS_FILE_RESTORE_STATUS
        ) -> HRESULT;
        pub unsafe fn AddNewTarget(
            &self,
            writerId: GUID,
            ct: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            wszPath: PCWSTR,
            wszFileName: PCWSTR,
            bRecursive: u8,
            wszAlternatePath: PCWSTR,
        ) -> HRESULT;
        pub unsafe fn SetRangesFilePath(
            &self,
            writerId: GUID,
            ct: VSS_COMPONENT_TYPE,
            wszLogicalPath: PCWSTR,
            wszComponentName: PCWSTR,
            iPartialFile: u32,
            wszRangesFile: PCWSTR,
        ) -> HRESULT;
        pub unsafe fn PreRestore(&self, ppAsync: *mut *mut core::ffi::c_void) -> HRESULT;
        pub unsafe fn PostRestore(&self, ppAsync: *mut *mut core::ffi::c_void) -> HRESULT;
        pub unsafe fn SetContext(&self, lContext: i32) -> HRESULT;
        pub unsafe fn StartSnapshotSet(&self, pSnapshotSetId: *mut GUID) -> HRESULT;
        pub unsafe fn AddToSnapshotSet(
            &self,
            pwszVolumeName: PCWSTR,
            ProviderId: GUID,
            pidSnapshot: *mut GUID,
        ) -> HRESULT;
        pub unsafe fn DoSnapshotSet(&self, ppAsync: *mut *mut core::ffi::c_void) -> HRESULT;
        pub unsafe fn DeleteSnapshots(
            &self,
            SourceObjectId: GUID,
            eSourceObjectType: VSS_OBJECT_TYPE,
            bForceDelete: BOOL,
            plDeletedSnapshots: *mut i32,
            pNondeletedSnapshotID: *mut GUID,
        ) -> HRESULT;
        pub unsafe fn ImportSnapshots(&self, ppAsync: *mut *mut core::ffi::c_void) -> HRESULT;
        pub unsafe fn BreakSnapshotSet(&self, SnapshotSetId: GUID) -> HRESULT;
        pub unsafe fn GetSnapshotProperties(
            &self,
            SnapshotId: GUID,
            pProp: *mut VSS_SNAPSHOT_PROP,
        ) -> HRESULT;
        pub unsafe fn Query(
            &self,
            QueriedObjectId: GUID,
            eQueriedObjectType: VSS_OBJECT_TYPE,
            eReturnedObjectsType: VSS_OBJECT_TYPE,
            ppEnum: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        pub unsafe fn IsVolumeSupported(
            &self,
            ProviderId: GUID,
            pwszVolumeName: PCWSTR,
            pbSupportedByThisProvider: *mut BOOL,
        ) -> HRESULT;
        pub unsafe fn DisableWriterClasses(
            &self,
            rgWriterClassId: *const GUID,
            cClassId: u32,
        ) -> HRESULT;
        pub unsafe fn EnableWriterClasses(
            &self,
            rgWriterClassId: *const GUID,
            cClassId: u32,
        ) -> HRESULT;
        pub unsafe fn DisableWriterInstances(
            &self,
            rgWriterInstanceId: *const GUID,
            cInstanceId: u32,
        ) -> HRESULT;
        pub unsafe fn ExposeSnapshot(
            &self,
            SnapshotId: GUID,
            wszPathFromRoot: PCWSTR,
            lAttributes: i32,
            wszExpose: PCWSTR,
            pwszExposed: *mut *mut u16,
        ) -> HRESULT;
        pub unsafe fn RevertToSnapshot(&self, SnapshotId: GUID, bForceDismount: BOOL) -> HRESULT;
        pub unsafe fn QueryRevertStatus(
            &self,
            pwszVolume: PCWSTR,
            ppAsync: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
    }

    // The `vsbackup.h` entry points. The documented names are static-lib
    // (VssApi.lib) inline wrappers around these stable `*Internal` exports of
    // vssapi.dll; linking the DLL exports directly (raw-dylib, no import lib)
    // works on both the MSVC release target and the GNU cross-check target.
    #[link(name = "vssapi.dll", kind = "raw-dylib", modifiers = "+verbatim")]
    extern "system" {
        fn CreateVssBackupComponentsInternal(
            ppBackup: *mut *mut core::ffi::c_void,
        ) -> windows::core::HRESULT;
        fn VssFreeSnapshotPropertiesInternal(pProp: *mut VSS_SNAPSHOT_PROP);
    }

    /// Create the requester object.
    pub fn create_backup_components() -> anyhow::Result<IVssBackupComponents> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: the export fills `raw` with a new IVssBackupComponents on S_OK.
        unsafe {
            CreateVssBackupComponentsInternal(&mut raw)
                .ok()
                .map_err(|e| anyhow::anyhow!("CreateVssBackupComponents failed: {e}"))?;
            Ok(IVssBackupComponents::from_raw(raw))
        }
    }

    /// Free the strings inside a `VSS_SNAPSHOT_PROP` filled by
    /// `GetSnapshotProperties`.
    pub fn free_snapshot_properties(prop: &mut VSS_SNAPSHOT_PROP) {
        // SAFETY: `prop` was filled by GetSnapshotProperties on this thread.
        unsafe { VssFreeSnapshotPropertiesInternal(prop) }
    }
}
