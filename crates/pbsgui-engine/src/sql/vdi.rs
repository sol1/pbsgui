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

use pbs_client::BackupStats;
#[cfg(not(windows))]
use pbs_client::SessionParams;
#[cfg(not(windows))]
use pbsgui_ipc::SqlAuth;

/// Combine the two halves of a VDI-to-PBS backup into one result.
///
/// `backup` is the `BACKUP DATABASE` statement outcome; `upload` is the PBS
/// upload outcome. A PBS upload failure breaks the device stream, which in turn
/// makes `BACKUP` fail, so when both fail the PBS error is usually the root
/// cause: report it first and include the `BACKUP` error too. Kept
/// cross-platform (not inside the Windows-only module) so this logic is
/// typechecked on every build.
fn combine_pbs_result(
    backup: anyhow::Result<()>,
    upload: pbs_client::Result<BackupStats>,
) -> anyhow::Result<BackupStats> {
    match (backup, upload) {
        (Ok(()), Ok(stats)) => Ok(stats),
        (Ok(()), Err(pbs)) => Err(anyhow::Error::new(pbs).context("PBS upload failed")),
        (Err(sql), Ok(_)) => Err(sql.context("BACKUP DATABASE failed")),
        (Err(sql), Err(pbs)) => Err(anyhow::anyhow!(
            "PBS upload failed: {:#}; BACKUP DATABASE also failed: {sql:#}",
            anyhow::Error::new(pbs)
        )),
    }
}

/// Combine the two halves of a VDI-to-file backup. `bytes` is the device-loop
/// byte count. A failed `BACKUP` is the source of truth even if the device
/// drained cleanly.
fn combine_file_result(
    backup: anyhow::Result<()>,
    device: anyhow::Result<u64>,
) -> anyhow::Result<u64> {
    match (backup, device) {
        (Ok(()), Ok(bytes)) => Ok(bytes),
        (Err(sql), _) => Err(sql.context("BACKUP DATABASE failed")),
        (Ok(()), Err(device)) => Err(device),
    }
}

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

/// Restore `target_database` over VDI from a previously-downloaded native backup
/// stream (`image`). The connection issuing `RESTORE` must be `sysadmin`.
#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)]
pub async fn restore_database_from_image(
    _server: &str,
    _port: Option<u16>,
    _auth: &SqlAuth,
    _password: Option<&str>,
    _source_database: &str,
    _target_database: &str,
    _image: Vec<u8>,
) -> anyhow::Result<()> {
    anyhow::bail!("VDI restore is only available on Windows")
}

#[cfg(windows)]
pub use windows_impl::{
    backup_database_to_file, backup_database_to_pbs, restore_database_from_image,
};

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
    use tiberius::Row;
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
    const ERROR_HANDLE_EOF: u32 = 38;
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

        super::combine_file_result(backup, device_result)
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

        // The device-loop result only matters when it is the lone failure; the
        // BACKUP and PBS errors are more informative when present.
        let _ = device_result;
        super::combine_pbs_result(backup, upload)
    }

    /// An in-memory restore source: the downloaded native backup stream, served
    /// sequentially to the device's `VDC_Read` requests.
    struct ImageSource {
        data: Vec<u8>,
        pos: usize,
    }

    impl ImageSource {
        /// Copy up to `out.len()` bytes into `out`; returns how many (fewer only
        /// at end of stream).
        fn read_into(&mut self, out: &mut [u8]) -> usize {
            let n = (self.data.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            n
        }
    }

    /// One file inside a backup (from `RESTORE FILELISTONLY`).
    struct BackupFile {
        logical: String,
        physical: String,
        is_log: bool,
    }

    /// Run a statement that reads its input from a VDI device fed by `image`
    /// (`RESTORE ... FROM VIRTUAL_DEVICE`), returning the statement's result sets.
    /// `make_sql` builds the statement given the device set name.
    async fn run_vdi_read_stmt<F>(
        client: &mut SqlClient,
        image: Vec<u8>,
        make_sql: F,
    ) -> anyhow::Result<Vec<Vec<Row>>>
    where
        F: FnOnce(&str) -> String,
    {
        let set_name = format!("pbsgui-{}", Uuid::new_v4());
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        let loop_name = set_name.clone();
        let device_loop = tokio::task::spawn_blocking(move || {
            let mut source = ImageSource {
                data: image,
                pos: 0,
            };
            run_device_session(&loop_name, ready_tx, |vds| {
                fill_device(vds, &loop_name, &mut source)
            })
        });

        ready_rx
            .await
            .map_err(|_| anyhow::anyhow!("VDI thread exited before signaling readiness"))??;

        let sql = make_sql(&set_name);
        let stmt = async {
            let stream = client.simple_query(sql).await?;
            stream.into_results().await
        }
        .await;

        let device_result = device_loop
            .await
            .map_err(|e| anyhow::anyhow!("VDI thread panicked: {e}"))?;

        // A statement failure breaks the device read, so it is the root cause.
        match (stmt, device_result) {
            (Ok(results), _) => Ok(results),
            (Err(sql), _) => Err(anyhow::Error::new(sql).context("the RESTORE statement failed")),
        }
    }

    pub async fn restore_database_from_image(
        server: &str,
        port: Option<u16>,
        auth: &SqlAuth,
        password: Option<&str>,
        source_database: &str,
        target_database: &str,
        image: Vec<u8>,
    ) -> anyhow::Result<()> {
        let mut client = probe::connect(server, port, auth, password).await?;
        let target = target_database.replace(']', "]]");

        if source_database.eq_ignore_ascii_case(target_database) {
            // In-place restore: keep the backup's own file paths.
            run_vdi_read_stmt(&mut client, image, |set| {
                format!(
                    "RESTORE DATABASE [{target}] FROM VIRTUAL_DEVICE = '{set}' \
                     WITH REPLACE, RECOVERY"
                )
            })
            .await?;
            return Ok(());
        }

        // Restoring under a new name: relocate each file so it does not collide
        // with the still-existing source database (otherwise error 1834).
        let files = backup_filelist(&mut client, image.clone()).await?;
        let (data_dir, log_dir) = default_dirs(&mut client).await?;
        let moves = move_clause(&files, &data_dir, &log_dir, target_database);
        run_vdi_read_stmt(&mut client, image, |set| {
            format!(
                "RESTORE DATABASE [{target}] FROM VIRTUAL_DEVICE = '{set}' \
                 WITH REPLACE, RECOVERY{moves}"
            )
        })
        .await?;
        Ok(())
    }

    /// Read the backup's file list via `RESTORE FILELISTONLY` over VDI.
    async fn backup_filelist(
        client: &mut SqlClient,
        image: Vec<u8>,
    ) -> anyhow::Result<Vec<BackupFile>> {
        let results = run_vdi_read_stmt(client, image, |set| {
            format!("RESTORE FILELISTONLY FROM VIRTUAL_DEVICE = '{set}'")
        })
        .await?;
        let rows = results.into_iter().next().unwrap_or_default();
        let files: Vec<BackupFile> = rows
            .into_iter()
            .map(|row| {
                let ftype = row.get::<&str, _>("Type").unwrap_or("D");
                BackupFile {
                    logical: row
                        .get::<&str, _>("LogicalName")
                        .unwrap_or_default()
                        .to_string(),
                    physical: row
                        .get::<&str, _>("PhysicalName")
                        .unwrap_or_default()
                        .to_string(),
                    is_log: ftype.eq_ignore_ascii_case("L"),
                }
            })
            .collect();
        if files.is_empty() {
            anyhow::bail!("the backup file list was empty");
        }
        Ok(files)
    }

    /// The instance's default data and log directories (empty if unavailable).
    async fn default_dirs(client: &mut SqlClient) -> anyhow::Result<(String, String)> {
        let row = client
            .simple_query(
                "SELECT CAST(SERVERPROPERTY('InstanceDefaultDataPath') AS nvarchar(260)), \
                        CAST(SERVERPROPERTY('InstanceDefaultLogPath') AS nvarchar(260))",
            )
            .await?
            .into_row()
            .await?
            .context("default-paths query returned no row")?;
        Ok((
            row.get::<&str, _>(0).unwrap_or_default().to_string(),
            row.get::<&str, _>(1).unwrap_or_default().to_string(),
        ))
    }

    /// Build the `, MOVE N'logical' TO N'newpath'` clauses that relocate each
    /// file into the default dir under a target-prefixed name.
    fn move_clause(files: &[BackupFile], data_dir: &str, log_dir: &str, target: &str) -> String {
        let tgt = sanitize_file(target);
        let mut clause = String::new();
        for f in files {
            let base = f.physical.rsplit(['\\', '/']).next().unwrap_or(&f.physical);
            let chosen = if f.is_log { log_dir } else { data_dir };
            let dir = if chosen.is_empty() {
                parent_dir(&f.physical)
            } else {
                chosen.to_string()
            };
            let dir = ensure_trailing_sep(&dir);
            let new_path = format!("{dir}{tgt}_{base}");
            clause.push_str(&format!(
                ", MOVE N'{}' TO N'{}'",
                esc(&f.logical),
                esc(&new_path)
            ));
        }
        clause
    }

    /// Escape a T-SQL single-quoted string literal.
    fn esc(value: &str) -> String {
        value.replace('\'', "''")
    }

    /// Keep only filename-safe characters for the target-name prefix.
    fn sanitize_file(value: &str) -> String {
        value
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn parent_dir(physical: &str) -> String {
        match physical.rfind(['\\', '/']) {
            Some(i) => physical[..i].to_string(),
            None => String::new(),
        }
    }

    fn ensure_trailing_sep(dir: &str) -> String {
        if dir.is_empty() || dir.ends_with('\\') || dir.ends_with('/') {
            dir.to_string()
        } else {
            format!("{dir}\\")
        }
    }

    /// Run the COM device-set lifecycle on the current (blocking) thread:
    /// initialize COM, create the device set, signal `ready` (so the caller can
    /// issue the BACKUP/RESTORE statement), run `body`, then close.
    fn run_device_loop(
        set_name: &str,
        sink: Sink,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    ) -> anyhow::Result<u64> {
        let mut sink = sink;
        run_device_session(set_name, ready, |vds| {
            drain_device(vds, set_name, &mut sink)
        })
    }

    fn run_device_session<F>(
        set_name: &str,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
        body: F,
    ) -> anyhow::Result<u64>
    where
        F: FnOnce(&IClientVirtualDeviceSet2) -> anyhow::Result<u64>,
    {
        unsafe {
            // S_FALSE (already initialized on this thread) is not a failure.
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .context("CoInitializeEx failed")?;
        }
        let result = run_device_session_inner(set_name, ready, body);
        unsafe { CoUninitialize() };
        result
    }

    fn run_device_session_inner<F>(
        set_name: &str,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
        body: F,
    ) -> anyhow::Result<u64>
    where
        F: FnOnce(&IClientVirtualDeviceSet2) -> anyhow::Result<u64>,
    {
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
        // The device set exists; the caller may now issue the statement. Anything
        // past here must close the set so a failed statement does not hang it.
        let _ = ready.send(Ok(()));

        let outcome = body(&vds);
        unsafe { vds.Close().ok().ok() };
        outcome
    }

    /// Wait for SQL Server to attach to the set, then open the single device.
    fn open_device(
        vds: &IClientVirtualDeviceSet2,
        set_name: &str,
    ) -> anyhow::Result<IClientVirtualDevice> {
        let mut config: VDConfig = unsafe { std::mem::zeroed() };
        let hr = unsafe { vds.GetConfiguration(30_000, &mut config) };
        if hr == VD_E_TIMEOUT {
            anyhow::bail!(
                "timed out waiting for SQL Server to attach to the virtual device \
                 (did the BACKUP/RESTORE start, and is the connection sysadmin?)"
            );
        }
        hr.ok().context("VDI GetConfiguration failed")?;

        let name_w = wide(set_name);
        let mut device_ptr: *mut c_void = std::ptr::null_mut();
        unsafe { vds.OpenDevice(PCWSTR(name_w.as_ptr()), &mut device_ptr) }
            .ok()
            .context("VDI OpenDevice failed")?;
        Ok(unsafe { IClientVirtualDevice::from_raw(device_ptr) })
    }

    /// Backup direction: handle the device's `VDC_Write` commands by writing the
    /// backup bytes to `sink`, until the device closes.
    fn drain_device(
        vds: &IClientVirtualDeviceSet2,
        set_name: &str,
        sink: &mut Sink,
    ) -> anyhow::Result<u64> {
        let device = open_device(vds, set_name)?;
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

    /// Restore direction: satisfy the device's `VDC_Read` commands from `source`,
    /// reporting end of stream when it runs out, until the device closes.
    fn fill_device(
        vds: &IClientVirtualDeviceSet2,
        set_name: &str,
        source: &mut ImageSource,
    ) -> anyhow::Result<u64> {
        let device = open_device(vds, set_name)?;
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
                VDC_READ => {
                    let out =
                        unsafe { std::slice::from_raw_parts_mut(cmd.buffer, cmd.size as usize) };
                    let n = source.read_into(out);
                    bytes_transferred = n as u32;
                    total += n as u64;
                    // A short read is end of stream, mirroring the VDI sample.
                    if n as u32 == cmd.size {
                        ERROR_SUCCESS
                    } else {
                        ERROR_HANDLE_EOF
                    }
                }
                VDC_FLUSH | VDC_CLEAR_ERROR => ERROR_SUCCESS,
                _ => ERROR_NOT_SUPPORTED,
            };

            unsafe { device.CompleteCommand(cmd_ptr, completion_code, bytes_transferred, 0) }
                .ok()
                .context("VDI CompleteCommand failed")?;
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::ChannelReader;
    use std::io::Read;

    #[test]
    fn combine_results_pick_the_informative_error() {
        // Both succeed.
        assert!(super::combine_file_result(Ok(()), Ok(42)).is_ok());
        // BACKUP failed (device may be anything) -> BACKUP error.
        let e = super::combine_file_result(Err(anyhow::anyhow!("sql boom")), Ok(0)).unwrap_err();
        assert!(format!("{e:#}").contains("sql boom"));
        // Device failed alone -> device error.
        let e = super::combine_file_result(Ok(()), Err(anyhow::anyhow!("disk full"))).unwrap_err();
        assert!(format!("{e:#}").contains("disk full"));
    }

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
