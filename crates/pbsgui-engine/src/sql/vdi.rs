//! SQL Server backup over the Virtual Device Interface (VDI).
//!
//! We present ourselves to SQL Server as a virtual backup device on
//! `SQLVDI.dll`, issue `BACKUP DATABASE ... TO VIRTUAL_DEVICE` on a separate
//! connection, and read the backup byte stream out of shared memory. The COM
//! interfaces and the device command loop are Windows-only; the exact ABI is
//! documented in `research/notes/08-vdi-abi.md`.
//!
//! Stage 1 (this module) streams the backup to a local `.bak` file to validate
//! the device handshake on real hardware. Stage 2 will replace the file sink
//! with the PBS fixed-index uploader.

use pbsgui_ipc::SqlAuth;

/// Back up `database` over VDI, writing the native backup stream to
/// `output_path`. The connection issuing `BACKUP` must be `sysadmin`.
#[cfg(not(windows))]
pub async fn backup_database_to_file(
    _server: &str,
    _port: Option<u16>,
    _auth: &SqlAuth,
    _password: Option<&str>,
    _database: &str,
    _output_path: &str,
) -> anyhow::Result<u64> {
    anyhow::bail!("VDI backup is only available on Windows")
}

#[cfg(windows)]
pub use windows_impl::backup_database_to_file;

#[cfg(windows)]
mod windows_impl {
    // The COM interface methods keep their documented C names (GetCommand, etc.)
    // so the vtable order is easy to verify against vdi.h.
    #![allow(non_snake_case)]

    use std::ffi::c_void;
    use std::fs::File;
    use std::io::Write;

    use anyhow::Context;
    use pbsgui_ipc::SqlAuth;
    use uuid::Uuid;
    use windows::core::{interface, IUnknown, IUnknown_Vtbl, Interface, HRESULT, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };

    // The coclass CLSID equals IID_IClientVirtualDeviceSet (see vdiguid.h /
    // research/notes/08-vdi-abi.md).
    const CLSID_CLIENT_VIRTUAL_DEVICE_SET: windows::core::GUID =
        windows::core::GUID::from_u128(0x40700425_0080_11d2_851f_00c04fc21759);

    // VDI HRESULTs (vdierror.h).
    const VD_E_TIMEOUT: HRESULT = HRESULT(0x8077_0003u32 as i32);
    const VD_E_CLOSE: HRESULT = HRESULT(0x8077_000eu32 as i32);

    // Command codes (enum VDCommands).
    const VDC_READ: u32 = 1;
    const VDC_WRITE: u32 = 2;
    const VDC_CLEAR_ERROR: u32 = 3;
    const VDC_FLUSH: u32 = 12;

    // Win32 completion codes returned to the device.
    const ERROR_SUCCESS: u32 = 0;
    const ERROR_NOT_SUPPORTED: u32 = 50;
    const ERROR_WRITE_FAULT: u32 = 29;

    /// Negotiated configuration of a virtual device set (vdi.h, pack 8).
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VDConfig {
        device_count: u32,
        features: u32,
        prefix_zone_size: u32,
        alignment: u32,
        soft_file_mark_block_size: u32,
        eom_warning_size: u32,
        server_time_out: u32,
        block_size: u32,
        max_io_depth: u32,
        max_transfer_size: u32,
        buffer_area_size: u32,
    }

    /// A command queued to a virtual device (vdi.h, pack 8).
    #[repr(C)]
    struct VDCCommand {
        command_code: u32,
        size: u32,
        position: u64,
        buffer: *mut u8,
    }

    #[interface("40700425-0080-11d2-851f-00c04fc21759")]
    unsafe trait IClientVirtualDeviceSet: IUnknown {
        unsafe fn Create(&self, name: PCWSTR, cfg: *const VDConfig) -> HRESULT;
        unsafe fn GetConfiguration(&self, timeout: u32, cfg: *mut VDConfig) -> HRESULT;
        unsafe fn OpenDevice(&self, name: PCWSTR, device: *mut *mut c_void) -> HRESULT;
        unsafe fn Close(&self) -> HRESULT;
        unsafe fn SignalAbort(&self) -> HRESULT;
        unsafe fn OpenInSecondary(&self, set_name: PCWSTR) -> HRESULT;
        unsafe fn GetBufferHandle(&self, buffer: *mut u8, handle: *mut u32) -> HRESULT;
        unsafe fn MapBufferHandle(&self, buffer: u32, address: *mut *mut u8) -> HRESULT;
    }

    #[interface("d0e6eb07-7a62-11d2-8573-00c04fc21759")]
    unsafe trait IClientVirtualDeviceSet2: IClientVirtualDeviceSet {
        unsafe fn CreateEx(&self, instance: PCWSTR, name: PCWSTR, cfg: *const VDConfig) -> HRESULT;
        unsafe fn OpenInSecondaryEx(&self, instance: PCWSTR, set_name: PCWSTR) -> HRESULT;
    }

    #[interface("40700424-0080-11d2-851f-00c04fc21759")]
    unsafe trait IClientVirtualDevice: IUnknown {
        unsafe fn GetCommand(&self, timeout: u32, command: *mut *mut VDCCommand) -> HRESULT;
        unsafe fn CompleteCommand(
            &self,
            command: *mut VDCCommand,
            completion_code: u32,
            bytes_transferred: u32,
            position: u64,
        ) -> HRESULT;
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub async fn backup_database_to_file(
        server: &str,
        port: Option<u16>,
        auth: &SqlAuth,
        password: Option<&str>,
        database: &str,
        output_path: &str,
    ) -> anyhow::Result<u64> {
        let mut client = super::probe::connect(server, port, auth, password).await?;

        let set_name = format!("pbsgui-{}", Uuid::new_v4());

        // The COM device loop is blocking; run it on a dedicated thread. It does
        // CreateEx, signals readiness, then drains the device into the file.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        let loop_name = set_name.clone();
        let loop_out = output_path.to_string();
        let device_loop =
            tokio::task::spawn_blocking(move || run_device_loop(&loop_name, &loop_out, ready_tx));

        // Wait until the device set exists before issuing BACKUP, so SQL can find it.
        ready_rx
            .await
            .map_err(|_| anyhow::anyhow!("VDI thread exited before signaling readiness"))??;

        // COPY_ONLY so an ad-hoc backup never disturbs the customer's diff/log chain.
        let escaped = database.replace(']', "]]");
        let backup_sql =
            format!("BACKUP DATABASE [{escaped}] TO VIRTUAL_DEVICE = '{set_name}' WITH COPY_ONLY");
        let backup = client.simple_query(backup_sql).await;
        let backup = match backup {
            Ok(stream) => stream.into_results().await.map(|_| ()),
            Err(e) => Err(e),
        };

        let bytes = device_loop
            .await
            .map_err(|e| anyhow::anyhow!("VDI thread panicked: {e}"))?;

        // The BACKUP statement is the source of truth: a device-side success with a
        // failed BACKUP still means no usable backup.
        backup.context("BACKUP DATABASE failed")?;
        bytes
    }

    /// Run the full COM device-set lifecycle on the current (blocking) thread,
    /// writing the backup stream to `output_path`. Signals `ready` once the
    /// device set is created (so the caller can issue BACKUP).
    fn run_device_loop(
        set_name: &str,
        output_path: &str,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    ) -> anyhow::Result<u64> {
        unsafe {
            // S_FALSE (already initialized on this thread) is not a failure.
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .context("CoInitializeEx failed")?;
        }
        let result = run_device_loop_inner(set_name, output_path, ready);
        unsafe { CoUninitialize() };
        result
    }

    fn run_device_loop_inner(
        set_name: &str,
        output_path: &str,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    ) -> anyhow::Result<u64> {
        let vds: IClientVirtualDeviceSet2 = unsafe {
            CoCreateInstance(&CLSID_CLIENT_VIRTUAL_DEVICE_SET, None, CLSCTX_INPROC_SERVER)
        }
        .context("CoCreateInstance(SQLVDI) failed; is SQLVDI.dll registered?")?;

        let mut config: VDConfig = unsafe { std::mem::zeroed() };
        config.device_count = 1;

        let name_w = wide(set_name);
        // Default instance: NULL instance name (no machine prefix).
        let create = unsafe { vds.CreateEx(PCWSTR::null(), PCWSTR(name_w.as_ptr()), &config) };
        if let Err(e) = create.ok() {
            let _ = ready.send(Err(anyhow::anyhow!("VDI CreateEx failed: {e}")));
            return Err(anyhow::anyhow!("VDI CreateEx failed: {e}"));
        }
        // The device set exists; the caller may now issue BACKUP. Anything past
        // here must close the set so a failed BACKUP does not hang the caller.
        let _ = ready.send(Ok(()));

        let outcome = drain_device(&vds, set_name, output_path);
        unsafe { vds.Close().ok().ok() };
        outcome
    }

    fn drain_device(
        vds: &IClientVirtualDeviceSet2,
        set_name: &str,
        output_path: &str,
    ) -> anyhow::Result<u64> {
        let mut config: VDConfig = unsafe { std::mem::zeroed() };
        // Wait for SQL Server to attach to the set (it does so as BACKUP starts).
        let hr = unsafe { vds.GetConfiguration(30_000, &mut config) };
        if hr == VD_E_TIMEOUT {
            anyhow::bail!(
                "timed out waiting for SQL Server to attach to the virtual device \
                 (did BACKUP start, and is the connection sysadmin?)"
            );
        }
        hr.ok().context("VDI GetConfiguration failed")?;

        let name_w = wide(set_name);
        let mut device_ptr: *mut c_void = std::ptr::null_mut();
        unsafe { vds.OpenDevice(PCWSTR(name_w.as_ptr()), &mut device_ptr) }
            .ok()
            .context("VDI OpenDevice failed")?;
        let device = unsafe { IClientVirtualDevice::from_raw(device_ptr) };

        let mut file = File::create(output_path)
            .with_context(|| format!("creating backup file {output_path}"))?;
        let mut total: u64 = 0;

        loop {
            let mut cmd_ptr: *mut VDCCommand = std::ptr::null_mut();
            let hr = unsafe { device.GetCommand(u32::MAX, &mut cmd_ptr) };
            if hr == VD_E_CLOSE {
                break; // normal end of stream
            }
            hr.ok().context("VDI GetCommand failed")?;

            let cmd = unsafe { &*cmd_ptr };
            let mut bytes_transferred: u32 = 0;
            let completion_code = match cmd.command_code {
                VDC_WRITE => {
                    let data = unsafe { std::slice::from_raw_parts(cmd.buffer, cmd.size as usize) };
                    match file.write_all(data) {
                        Ok(()) => {
                            bytes_transferred = cmd.size;
                            total += u64::from(cmd.size);
                            ERROR_SUCCESS
                        }
                        Err(_) => ERROR_WRITE_FAULT,
                    }
                }
                VDC_FLUSH => {
                    let _ = file.flush();
                    ERROR_SUCCESS
                }
                VDC_CLEAR_ERROR => ERROR_SUCCESS,
                // Restore-only and tape/snapshot commands are not used here.
                VDC_READ => ERROR_NOT_SUPPORTED,
                _ => ERROR_NOT_SUPPORTED,
            };

            unsafe { device.CompleteCommand(cmd_ptr, completion_code, bytes_transferred, 0) }
                .ok()
                .context("VDI CompleteCommand failed")?;

            if completion_code == ERROR_WRITE_FAULT {
                anyhow::bail!("failed writing backup data to {output_path}");
            }
        }

        file.flush().ok();
        Ok(total)
    }
}
