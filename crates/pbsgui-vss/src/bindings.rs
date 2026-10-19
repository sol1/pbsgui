//! Hand-written bindings for the `vsbackup.h` requester API.
//!
//! The requester API (`IVssBackupComponents` and friends) is absent from the
//! `windows` crate: Microsoft ships it as a C++ static library (VssApi.lib), so it
//! has no COM registration metadata. The interfaces are declared here by hand,
//! transcribed from the mingw-w64 `vsbackup.h` (which carries the exact vtable
//! order and IIDs), and the entry points are the `*Internal` exports of
//! `vssapi.dll` that the static-lib wrappers call. The vtable must list every
//! method in declaration order, including ones pbsgui never calls, or later
//! methods would dispatch to the wrong slots. ABI notes: `boolean` is one byte
//! (u8), `WINBOOL` is four (BOOL); `VSS_ID` (GUID) parameters pass by value.

#![allow(
    non_snake_case,
    dead_code,
    clippy::too_many_arguments,
    clippy::missing_safety_doc
)]

use windows::core::{interface, IUnknown, IUnknown_Vtbl, Interface, BSTR, GUID, HRESULT, PCWSTR};
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
    pub unsafe fn GetFile(&self, iFile: u32, ppFiledesc: *mut *mut core::ffi::c_void) -> HRESULT;
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
