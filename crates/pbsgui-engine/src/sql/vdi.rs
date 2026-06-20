//! SQL Server backup over the Virtual Device Interface (VDI).
//!
//! We present ourselves to SQL Server as a virtual backup device on
//! `SQLVDI.dll`, issue `BACKUP DATABASE ... TO VIRTUAL_DEVICE` on a separate
//! connection, and read the backup byte stream out of shared memory. The COM
//! interfaces and the device command loop are Windows-only; the exact ABI is
//! documented in `research/notes/08-vdi-abi.md`.
//!
//! The same device loop feeds either a local `.bak` file (for validating the
//! device handshake) or, streamed through [`ChannelReader`], the PBS uploader as
//! a deduplicated dynamic-index snapshot.

use std::io::Read;
use std::sync::mpsc::Receiver;

#[cfg(not(windows))]
use pbs_client::{BackupStats, SessionParams};
#[cfg(not(windows))]
use pbsgui_ipc::SqlAuth;

/// Adapts a channel of byte buffers into a blocking [`Read`], so the VDI device
/// thread can hand its backup stream to the PBS uploader without staging to
/// disk. Reading returns EOF once every sender has been dropped.
pub(crate) struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    pub(crate) fn new(rx: Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        // Pull the next non-empty buffer, blocking until one arrives or the
        // senders drop (EOF).
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(next) => {
                    self.buf = next;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

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

/// Back up `database` over VDI, streaming it to PBS as a dynamic-index snapshot.
#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)]
pub async fn backup_database_to_pbs(
    _server: &str,
    _port: Option<u16>,
    _auth: &SqlAuth,
    _password: Option<&str>,
    _database: &str,
    _params: &SessionParams,
    _archive_name: &str,
) -> anyhow::Result<BackupStats> {
    anyhow::bail!("VDI backup is only available on Windows")
}

#[cfg(windows)]
pub use windows_impl::{backup_database_to_file, backup_database_to_pbs};

#[cfg(windows)]
mod windows_impl {
    // The COM interface methods keep their documented C names (GetCommand, etc.)
    // so the vtable order is easy to verify against vdi.h.
    #![allow(non_snake_case)]

    use std::ffi::c_void;
    use std::fs::File;
    use std::io::Write;
    use std::sync::mpsc::SyncSender;

    use anyhow::Context;
    use pbs_client::{BackupStats, SessionParams};
    use pbsgui_ipc::SqlAuth;
    use uuid::Uuid;
    use windows::core::{interface, IUnknown, IUnknown_Vtbl, Interface, HRESULT, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };

    use super::ChannelReader;
    use crate::sql::probe::{self, SqlClient};

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

    /// Where the device loop writes the backup stream.
    enum Sink {
        File(File),
        /// Bounded channel to the PBS uploader; full = backpressure onto SQL.
        Channel(SyncSender<Vec<u8>>),
    }

    impl Sink {
        fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
            match self {
                Sink::File(file) => file.write_all(data),
                Sink::Channel(tx) => tx.send(data.to_vec()).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "PBS upload stopped")
                }),
            }
        }

        fn flush(&mut self) {
            if let Sink::File(file) = self {
                let _ = file.flush();
            }
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Issue `BACKUP DATABASE ... TO VIRTUAL_DEVICE`. COPY_ONLY so an ad-hoc
    /// backup never disturbs the customer's differential/log chain.
    async fn issue_backup(
        client: &mut SqlClient,
        database: &str,
        set_name: &str,
    ) -> anyhow::Result<()> {
        let escaped = database.replace(']', "]]");
        let sql =
            format!("BACKUP DATABASE [{escaped}] TO VIRTUAL_DEVICE = '{set_name}' WITH COPY_ONLY");
        client.simple_query(sql).await?.into_results().await?;
        Ok(())
    }

    pub async fn backup_database_to_file(
        server: &str,
        port: Option<u16>,
        auth: &SqlAuth,
        password: Option<&str>,
        database: &str,
        output_path: &str,
    ) -> anyhow::Result<u64> {
        let mut client = probe::connect(server, port, auth, password).await?;
        let set_name = format!("pbsgui-{}", Uuid::new_v4());

        if let Some(parent) = std::path::Path::new(output_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = File::create(output_path)
            .with_context(|| format!("creating backup file {output_path}"))?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        let loop_name = set_name.clone();
        let device_loop = tokio::task::spawn_blocking(move || {
            run_device_loop(&loop_name, Sink::File(file), ready_tx)
        });

        ready_rx
            .await
            .map_err(|_| anyhow::anyhow!("VDI thread exited before signaling readiness"))??;

        let backup = issue_backup(&mut client, database, &set_name).await;
        let device_result = device_loop
            .await
            .map_err(|e| anyhow::anyhow!("VDI thread panicked: {e}"))?;

        match (backup, device_result) {
            (Ok(()), Ok(bytes)) => Ok(bytes),
            (Err(sql), _) => Err(sql.context("BACKUP DATABASE failed")),
            (Ok(()), Err(device)) => Err(device),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn backup_database_to_pbs(
        server: &str,
        port: Option<u16>,
        auth: &SqlAuth,
        password: Option<&str>,
        database: &str,
        params: &SessionParams,
        archive_name: &str,
    ) -> anyhow::Result<BackupStats> {
        let mut client = probe::connect(server, port, auth, password).await?;
        let set_name = format!("pbsgui-{}", Uuid::new_v4());

        // Bounded so SQL throttles to the PBS upload rate rather than buffering
        // the whole database in memory.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        let loop_name = set_name.clone();
        let device_loop = tokio::task::spawn_blocking(move || {
            run_device_loop(&loop_name, Sink::Channel(tx), ready_tx)
        });

        ready_rx
            .await
            .map_err(|_| anyhow::anyhow!("VDI thread exited before signaling readiness"))??;

        // BACKUP (pushes bytes into the device) and the PBS upload (drains them)
        // run concurrently; the device loop bridges the two on its own thread.
        let backup_fut = issue_backup(&mut client, database, &set_name);
        let upload_fut = pbs_client::backup_dynamic_reader(
            params,
            archive_name,
            true,
            ChannelReader::new(rx),
            0,
            None,
            |_done, _total| {},
        );
        let (backup, upload) = tokio::join!(backup_fut, upload_fut);

        let device_result = device_loop
            .await
            .map_err(|e| anyhow::anyhow!("VDI thread panicked: {e}"))?;

        match (backup, upload, device_result) {
            (Ok(()), Ok(stats), _) => Ok(stats),
            (Err(sql), _, _) => Err(sql.context("BACKUP DATABASE failed")),
            (Ok(()), Err(pbs), _) => Err(anyhow::Error::new(pbs).context("PBS upload failed")),
        }
    }

    /// Run the COM device-set lifecycle on the current (blocking) thread,
    /// draining the device into `sink`. Signals `ready` once the device set is
    /// created (so the caller can issue BACKUP).
    fn run_device_loop(
        set_name: &str,
        sink: Sink,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    ) -> anyhow::Result<u64> {
        unsafe {
            // S_FALSE (already initialized on this thread) is not a failure.
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .context("CoInitializeEx failed")?;
        }
        let result = run_device_loop_inner(set_name, sink, ready);
        unsafe { CoUninitialize() };
        result
    }

    fn run_device_loop_inner(
        set_name: &str,
        mut sink: Sink,
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

        let outcome = drain_device(&vds, set_name, &mut sink);
        unsafe { vds.Close().ok().ok() };
        outcome
    }

    fn drain_device(
        vds: &IClientVirtualDeviceSet2,
        set_name: &str,
        sink: &mut Sink,
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
                    match sink.write(data) {
                        Ok(()) => {
                            bytes_transferred = cmd.size;
                            total += u64::from(cmd.size);
                            ERROR_SUCCESS
                        }
                        Err(_) => ERROR_WRITE_FAULT,
                    }
                }
                VDC_FLUSH => {
                    sink.flush();
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
                anyhow::bail!("failed writing backup data to the sink");
            }
        }

        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::ChannelReader;
    use std::io::Read;

    #[test]
    fn channel_reader_concatenates_buffers_and_ends_on_drop() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        tx.send(b"hello ".to_vec()).unwrap();
        tx.send(Vec::new()).unwrap(); // empty buffers are skipped
        tx.send(b"world".to_vec()).unwrap();
        drop(tx); // signals EOF

        let mut out = Vec::new();
        ChannelReader::new(rx).read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello world");
    }
}
