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

// The device loop and its helpers (ChannelReader, backup_statement, the result
// combiners) are exercised by the Windows VDI path and the tests; on a non-Windows
// build they are compiled but unused, so allow that here only.
#![cfg_attr(not(windows), allow(dead_code))]

use std::io::Read;
use std::sync::mpsc::Receiver;

use pbs_client::BackupStats;
#[cfg(not(windows))]
use pbs_client::{BackupProgress, CryptConfig, SessionParams};
#[cfg(not(windows))]
use pbsgui_ipc::SqlAuth;
use pbsgui_ipc::SqlBackupType;
#[cfg(not(windows))]
use tokio_util::sync::CancellationToken;

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
        (Err(sql), Ok(_)) => Err(sql.context("BACKUP failed")),
        (Err(sql), Err(pbs)) => Err(anyhow::anyhow!(
            "PBS upload failed: {:#}; BACKUP also failed: {sql:#}",
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
        (Err(sql), _) => Err(sql.context("BACKUP failed")),
        (Ok(()), Err(device)) => Err(device),
    }
}

/// Build the `BACKUP` statement for a VDI device set.
///
/// Full backups may be copy-only (the default) so they do not disturb another
/// tool's differential/log chain. Log backups are never copy-only: their purpose
/// is to back up and truncate the transaction log so it does not grow without
/// bound (a copy-only log backup would not truncate). Cross-platform so the SQL
/// text is typechecked and unit-tested on every build.
fn backup_statement(
    database: &str,
    set_name: &str,
    backup_type: SqlBackupType,
    copy_only: bool,
) -> anyhow::Result<String> {
    let db = database.replace(']', "]]");
    Ok(match backup_type {
        SqlBackupType::Full => {
            let copy = if copy_only { " WITH COPY_ONLY" } else { "" };
            format!("BACKUP DATABASE [{db}] TO VIRTUAL_DEVICE = '{set_name}'{copy}")
        }
        SqlBackupType::Log => {
            format!("BACKUP LOG [{db}] TO VIRTUAL_DEVICE = '{set_name}'")
        }
        SqlBackupType::Differential => {
            anyhow::bail!("differential SQL backups are not supported yet")
        }
    })
}

/// Adapts a channel of byte buffers into a blocking [`Read`], so the VDI device
/// thread can hand its backup stream to the PBS uploader without staging to disk.
///
/// When the data channel closes (every sender dropped) the stream has ended, but
/// that alone does not say whether it ended cleanly: a mid-stream `BACKUP` failure
/// closes the device the same way a successful one does. So end of stream is
/// reported only when `done` confirms the `BACKUP` succeeded; a `false` verdict, or
/// a dropped verdict sender (the backup future was cancelled), surfaces an I/O
/// error instead, which fails the upload before it can commit a truncated snapshot.
pub(crate) struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    done: Receiver<bool>,
    buf: Vec<u8>,
    pos: usize,
    /// The `BACKUP` verdict, cached once the data channel closes so repeated reads
    /// past the end stay consistent.
    verdict: Option<bool>,
}

impl ChannelReader {
    pub(crate) fn new(rx: Receiver<Vec<u8>>, done: Receiver<bool>) -> Self {
        Self {
            rx,
            done,
            buf: Vec::new(),
            pos: 0,
            verdict: None,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        // Pull the next non-empty buffer, blocking until one arrives or the
        // senders drop (end of stream).
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(next) => {
                    self.buf = next;
                    self.pos = 0;
                }
                Err(_) => {
                    // The data channel closed. Treat it as a clean end of stream
                    // only if BACKUP confirmed success; otherwise the stream was
                    // truncated (BACKUP failed or was cancelled) and we must not let
                    // the uploader commit a partial snapshot.
                    let ok = match self.verdict {
                        Some(v) => v,
                        None => {
                            let v = self.done.recv().unwrap_or(false);
                            self.verdict = Some(v);
                            v
                        }
                    };
                    if ok {
                        return Ok(0);
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the SQL Server BACKUP did not complete; refusing to upload a \
                         truncated backup",
                    ));
                }
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
pub async fn backup_database_to_pbs<F: FnMut(&BackupProgress) + Send>(
    _server: &str,
    _port: Option<u16>,
    _auth: &SqlAuth,
    _password: Option<&str>,
    _database: &str,
    _params: &SessionParams,
    _archive_name: &str,
    _crypt: Option<CryptConfig>,
    _compress: bool,
    _total_estimate: u64,
    _on_progress: F,
    _cancel: CancellationToken,
    _backup_type: SqlBackupType,
    _copy_only: bool,
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

/// Restore a chain of backups (full first, then logs) to a point in time.
#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)]
pub async fn restore_chain(
    _server: &str,
    _port: Option<u16>,
    _auth: &SqlAuth,
    _password: Option<&str>,
    _source_database: &str,
    _target_database: &str,
    _steps: Vec<(bool, Vec<u8>)>,
    _target_unix: i64,
) -> anyhow::Result<()> {
    anyhow::bail!("VDI restore is only available on Windows")
}

/// Restore a database by streaming its backup from PBS straight into SQL Server
/// (bounded memory). See the Windows implementation.
#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)]
pub async fn restore_database_streamed(
    _server: &str,
    _port: Option<u16>,
    _auth: &SqlAuth,
    _password: Option<&str>,
    _source_database: &str,
    _target_database: &str,
    _pbs: &SessionParams,
    _archive: &str,
    _crypt: Option<CryptConfig>,
    _files: &[crate::sql::backupmeta::SqlBackupFile],
) -> anyhow::Result<()> {
    anyhow::bail!("VDI restore is only available on Windows")
}

/// Restore a chain (full then logs) to a point in time by streaming each backup
/// from PBS into SQL Server (bounded memory). See the Windows implementation.
#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)]
pub async fn restore_chain_streamed(
    _server: &str,
    _port: Option<u16>,
    _auth: &SqlAuth,
    _password: Option<&str>,
    _source_database: &str,
    _target_database: &str,
    _steps: Vec<(bool, SessionParams, String)>,
    _crypt: Option<CryptConfig>,
    _files: &[crate::sql::backupmeta::SqlBackupFile],
    _target_unix: i64,
) -> anyhow::Result<()> {
    anyhow::bail!("VDI restore is only available on Windows")
}

#[cfg(windows)]
pub use windows_impl::{
    backup_database_to_file, backup_database_to_pbs, restore_chain, restore_chain_streamed,
    restore_database_from_image, restore_database_streamed,
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
    use pbs_client::{BackupProgress, BackupStats, CryptConfig, SessionParams};
    use pbsgui_ipc::{SqlAuth, SqlBackupType};
    use tiberius::Row;
    use tokio_util::sync::CancellationToken;
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

    /// Issue the `BACKUP` statement for `backup_type` against the VDI device set
    /// (see [`super::backup_statement`] for the copy-only / log truncation rules).
    async fn issue_backup(
        client: &mut SqlClient,
        database: &str,
        set_name: &str,
        backup_type: SqlBackupType,
        copy_only: bool,
    ) -> anyhow::Result<()> {
        let sql = super::backup_statement(database, set_name, backup_type, copy_only)?;
        client.simple_query(sql).await?.into_results().await?;
        Ok(())
    }

    /// Read the just-written backup's chain metadata from `msdb.dbo.backupset`
    /// (the newest row for the database) as JSON, for the snapshot's meta blob.
    async fn query_backup_meta(client: &mut SqlClient, database: &str) -> anyhow::Result<Vec<u8>> {
        let db = database.replace('\'', "''");
        let sql = format!(
            "SELECT TOP 1 CONVERT(varchar(40), first_lsn) AS first_lsn, \
             CONVERT(varchar(40), last_lsn) AS last_lsn, \
             CONVERT(varchar(40), database_backup_lsn) AS db_backup_lsn, \
             [type] AS btype, \
             DATEDIFF_BIG(SECOND, '1970-01-01', backup_finish_date) \
               - (DATEDIFF(MINUTE, GETUTCDATE(), GETDATE()) * 60) AS finish_unix \
             FROM msdb.dbo.backupset WHERE database_name = N'{db}' ORDER BY backup_set_id DESC"
        );
        let row = client
            .simple_query(sql)
            .await?
            .into_row()
            .await?
            .context("no msdb.dbo.backupset row for the database")?;
        let btype = row.get::<&str, _>("btype").unwrap_or("D");
        let kind = if btype.eq_ignore_ascii_case("L") {
            "log"
        } else {
            "full"
        }
        .to_string();
        let backup_time = row.get::<i64, _>("finish_unix").unwrap_or(0);
        let first_lsn = row
            .get::<&str, _>("first_lsn")
            .unwrap_or_default()
            .to_string();
        let last_lsn = row
            .get::<&str, _>("last_lsn")
            .unwrap_or_default()
            .to_string();
        let database_backup_lsn = row
            .get::<&str, _>("db_backup_lsn")
            .unwrap_or_default()
            .to_string();
        // Drop the borrow of `client` from the row before the next query.
        drop(row);
        let files = query_db_files(client, database).await.unwrap_or_default();
        let meta = crate::sql::backupmeta::SqlBackupMeta {
            kind,
            backup_time,
            first_lsn,
            last_lsn,
            database_backup_lsn,
            files,
        };
        Ok(serde_json::to_vec(&meta)?)
    }

    /// The database's logical files at backup time (name, physical path, and whether
    /// it is a log file), so a renamed restore can relocate them without a second
    /// read of the backup.
    async fn query_db_files(
        client: &mut SqlClient,
        database: &str,
    ) -> anyhow::Result<Vec<crate::sql::backupmeta::SqlBackupFile>> {
        let db = database.replace('\'', "''");
        let sql = format!(
            "SELECT name, physical_name, type_desc FROM sys.master_files \
             WHERE database_id = DB_ID(N'{db}')"
        );
        let results = client.simple_query(sql).await?.into_results().await?;
        let rows = results.into_iter().next().unwrap_or_default();
        Ok(rows
            .into_iter()
            .map(|row| crate::sql::backupmeta::SqlBackupFile {
                logical: row.get::<&str, _>("name").unwrap_or_default().to_string(),
                physical: row
                    .get::<&str, _>("physical_name")
                    .unwrap_or_default()
                    .to_string(),
                is_log: row
                    .get::<&str, _>("type_desc")
                    .map(|t| t.eq_ignore_ascii_case("LOG"))
                    .unwrap_or(false),
            })
            .collect())
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
        // The to-file validation path is short and not user-cancellable.
        let cancel = CancellationToken::new();
        let device_loop = tokio::task::spawn_blocking(move || {
            run_device_loop(&loop_name, Sink::File(file), ready_tx, cancel)
        });

        ready_rx
            .await
            .map_err(|_| anyhow::anyhow!("VDI thread exited before signaling readiness"))??;

        // The ad-hoc "to file" path is always a full, copy-only backup.
        let backup =
            issue_backup(&mut client, database, &set_name, SqlBackupType::Full, true).await;
        let device_result = device_loop
            .await
            .map_err(|e| anyhow::anyhow!("VDI thread panicked: {e}"))?;

        super::combine_file_result(backup, device_result)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn backup_database_to_pbs<F: FnMut(&BackupProgress) + Send>(
        server: &str,
        port: Option<u16>,
        auth: &SqlAuth,
        password: Option<&str>,
        database: &str,
        params: &SessionParams,
        archive_name: &str,
        crypt: Option<CryptConfig>,
        compress: bool,
        total_estimate: u64,
        on_progress: F,
        cancel: CancellationToken,
        backup_type: SqlBackupType,
        copy_only: bool,
    ) -> anyhow::Result<BackupStats> {
        let mut client = probe::connect(server, port, auth, password).await?;
        let set_name = format!("pbsgui-{}", Uuid::new_v4());

        // Bounded so SQL throttles to the PBS upload rate rather than buffering
        // the whole database in memory.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
        // Carries the BACKUP statement's verdict to the reader so it commits only a
        // complete stream (see ChannelReader). A dropped sender (a cancelled run)
        // reads as failure, so a cancelled backup never commits a partial snapshot.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        let loop_name = set_name.clone();
        let loop_cancel = cancel.clone();
        let device_loop = tokio::task::spawn_blocking(move || {
            run_device_loop(&loop_name, Sink::Channel(tx), ready_tx, loop_cancel)
        });

        ready_rx
            .await
            .map_err(|_| anyhow::anyhow!("VDI thread exited before signaling readiness"))??;

        // BACKUP (pushes bytes into the device) and the PBS upload (drains them)
        // run concurrently; the device loop bridges the two on its own thread.
        // After BACKUP yields its LSNs, send the chain metadata to the uploader so
        // it lands in the snapshot before finalisation (point-in-time restore reads
        // it to order the chain). A dropped sender (BACKUP failed) means no blob.
        let (meta_tx, meta_rx) = tokio::sync::oneshot::channel::<(String, Vec<u8>)>();
        let backup_and_meta = async {
            // Own the verdict sender here so a dropped (cancelled) future drops it,
            // which the reader treats as a truncated stream.
            let done_tx = done_tx;
            let result =
                issue_backup(&mut client, database, &set_name, backup_type, copy_only).await;
            // Report the verdict before anything else, so even a metadata-query
            // failure on an otherwise successful backup still commits.
            let _ = done_tx.send(result.is_ok());
            if result.is_ok() {
                match query_backup_meta(&mut client, database).await {
                    Ok(meta) => {
                        let _ = meta_tx
                            .send((crate::sql::backupmeta::META_BLOB_NAME.to_string(), meta));
                    }
                    Err(e) => tracing::warn!("could not read backup metadata: {e:#}"),
                }
            }
            result
        };
        let upload_fut = pbs_client::backup_dynamic_reader(
            params,
            archive_name,
            true,
            compress,
            ChannelReader::new(rx, done_rx),
            total_estimate,
            Some(meta_rx),
            crypt,
            on_progress,
        );
        let (backup, upload) = tokio::join!(backup_and_meta, upload_fut);

        let device_result = device_loop
            .await
            .map_err(|e| anyhow::anyhow!("VDI thread panicked: {e}"))?;

        // The device-loop result only matters when it is the lone failure; the
        // BACKUP and PBS errors are more informative when present.
        let _ = device_result;
        super::combine_pbs_result(backup, upload)
    }

    /// A source of bytes for a RESTORE: fill `out` as much as possible, returning
    /// fewer than `out.len()` only at true end of stream, so the device loop can
    /// treat a short read as EOF (mirroring the VDI sample).
    trait RestoreSource {
        fn read_into(&mut self, out: &mut [u8]) -> usize;
    }

    /// An in-memory restore source: a fully-downloaded native backup stream, served
    /// sequentially to the device's `VDC_Read` requests (the buffered restore path).
    struct ImageSource {
        data: Vec<u8>,
        pos: usize,
    }

    impl RestoreSource for ImageSource {
        fn read_into(&mut self, out: &mut [u8]) -> usize {
            let n = (self.data.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            n
        }
    }

    /// A streaming restore source fed by an async producer over a bounded channel:
    /// the device thread pulls bytes with `blocking_recv`, blocking until the next
    /// chunk or the senders drop (end of stream). It fills the whole request unless
    /// the stream has ended, so the device loop's short-read EOF rule still holds.
    struct StreamReader {
        rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
        buf: Vec<u8>,
        pos: usize,
    }

    impl StreamReader {
        fn new(rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Self {
            Self {
                rx,
                buf: Vec::new(),
                pos: 0,
            }
        }
    }

    impl RestoreSource for StreamReader {
        fn read_into(&mut self, out: &mut [u8]) -> usize {
            let mut filled = 0;
            while filled < out.len() {
                if self.pos >= self.buf.len() {
                    match self.rx.blocking_recv() {
                        Some(next) => {
                            self.buf = next;
                            self.pos = 0;
                        }
                        None => break, // every sender dropped: end of stream
                    }
                    continue;
                }
                let n = (self.buf.len() - self.pos).min(out.len() - filled);
                out[filled..filled + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                filled += n;
            }
            filled
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

    /// Restore a chain of backup images (full first, then logs) to a point in
    /// time. The full restores `WITH NORECOVERY` (relocating files for a new
    /// name), each log `WITH STOPAT, NORECOVERY`, then the database is brought
    /// online `WITH RECOVERY` at the recovered point. Each image is fed to its own
    /// VDI device, in order.
    #[allow(clippy::too_many_arguments)]
    pub async fn restore_chain(
        server: &str,
        port: Option<u16>,
        auth: &SqlAuth,
        password: Option<&str>,
        source_database: &str,
        target_database: &str,
        steps: Vec<(bool, Vec<u8>)>,
        target_unix: i64,
    ) -> anyhow::Result<()> {
        if steps.is_empty() {
            anyhow::bail!("empty restore chain");
        }
        let mut client = probe::connect(server, port, auth, password).await?;
        let target = target_database.replace(']', "]]");
        let new_name = !source_database.eq_ignore_ascii_case(target_database);

        // STOPAT must be a datetime in the server's local time; convert the UTC
        // target using the server's current UTC offset.
        let offset_min: i32 = client
            .simple_query("SELECT DATEDIFF(MINUTE, GETUTCDATE(), GETDATE())")
            .await?
            .into_row()
            .await?
            .and_then(|r| r.get::<i32, _>(0))
            .unwrap_or(0);
        let local_unix = target_unix + (offset_min as i64) * 60;
        let stopat = chrono::DateTime::from_timestamp(local_unix, 0)
            .map(|d| d.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default();

        // MOVE clauses (new name only), read from the full = the first image.
        let moves = if new_name {
            let files = backup_filelist(&mut client, steps[0].1.clone()).await?;
            let (data_dir, log_dir) = default_dirs(&mut client).await?;
            move_clause(&files, &data_dir, &log_dir, target_database)
        } else {
            String::new()
        };

        for (i, (_is_log, image)) in steps.into_iter().enumerate() {
            let target = target.clone();
            let moves = moves.clone();
            let stopat = stopat.clone();
            run_vdi_read_stmt(&mut client, image, move |set| {
                if i == 0 {
                    format!(
                        "RESTORE DATABASE [{target}] FROM VIRTUAL_DEVICE = '{set}' \
                         WITH REPLACE, NORECOVERY{moves}"
                    )
                } else {
                    format!(
                        "RESTORE LOG [{target}] FROM VIRTUAL_DEVICE = '{set}' \
                         WITH STOPAT = N'{stopat}', NORECOVERY"
                    )
                }
            })
            .await?;
        }

        // Bring the database online at the recovered point.
        client
            .simple_query(format!("RESTORE DATABASE [{target}] WITH RECOVERY"))
            .await?
            .into_results()
            .await?;
        Ok(())
    }

    /// Run one RESTORE statement whose backup is streamed from PBS instead of held
    /// in memory. A background task streams the archive into a bounded channel; the
    /// device thread serves the statement's reads from it. `make_sql` builds the
    /// statement from the device set name. Only one chunk is buffered at a time.
    async fn restore_one_streamed<F>(
        client: &mut SqlClient,
        pbs: &SessionParams,
        archive: &str,
        crypt: Option<&CryptConfig>,
        make_sql: F,
    ) -> anyhow::Result<()>
    where
        F: FnOnce(&str) -> String,
    {
        let set_name = format!("pbsgui-{}", Uuid::new_v4());
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        let loop_name = set_name.clone();
        let device = tokio::task::spawn_blocking(move || {
            let mut source = StreamReader::new(rx);
            run_device_session(&loop_name, ready_tx, |vds| {
                fill_device(vds, &loop_name, &mut source)
            })
        });
        ready_rx
            .await
            .map_err(|_| anyhow::anyhow!("VDI thread exited before signaling readiness"))??;

        // Background task: stream the archive from PBS into the channel. Dropping the
        // sender at the end signals end of stream to the device.
        let pbs = pbs.clone();
        let archive = archive.to_string();
        let crypt = crypt.cloned();
        let download = tokio::spawn(async move {
            let mut reader = pbs_client::session::ReaderClient::connect(&pbs).await?;
            reader
                .restore_dynamic_archive_streamed(&archive, crypt.as_ref(), |chunk| {
                    let tx = tx.clone();
                    async move {
                        tx.send(chunk).await.map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "restore consumer stopped",
                            )
                        })
                    }
                })
                .await?;
            Ok::<(), anyhow::Error>(())
        });

        // Issue the RESTORE; it reads from the device, which the download feeds.
        let sql = make_sql(&set_name);
        let restore_result: anyhow::Result<()> = async {
            client.simple_query(sql).await?.into_results().await?;
            Ok(())
        }
        .await;

        let download_result = download.await;
        let device_result = device.await;

        // The RESTORE error is the most informative; then the download; then device.
        restore_result.context("the RESTORE statement failed")?;
        match download_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e.context("streaming the backup from PBS failed")),
            Err(e) => return Err(anyhow::anyhow!("PBS download task panicked: {e}")),
        }
        match device_result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e.context("the VDI device loop failed")),
            Err(e) => return Err(anyhow::anyhow!("VDI thread panicked: {e}")),
        }
        Ok(())
    }

    /// MOVE clauses for a renamed restore, built from the file list captured at
    /// backup time (so no `RESTORE FILELISTONLY` pass over the streamed backup).
    async fn moves_from_files(
        client: &mut SqlClient,
        files: &[crate::sql::backupmeta::SqlBackupFile],
        target_database: &str,
    ) -> anyhow::Result<String> {
        let (data_dir, log_dir) = default_dirs(client).await?;
        let converted: Vec<BackupFile> = files
            .iter()
            .map(|f| BackupFile {
                logical: f.logical.clone(),
                physical: f.physical.clone(),
                is_log: f.is_log,
            })
            .collect();
        Ok(move_clause(
            &converted,
            &data_dir,
            &log_dir,
            target_database,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn restore_database_streamed(
        server: &str,
        port: Option<u16>,
        auth: &SqlAuth,
        password: Option<&str>,
        source_database: &str,
        target_database: &str,
        pbs: &SessionParams,
        archive: &str,
        crypt: Option<CryptConfig>,
        files: &[crate::sql::backupmeta::SqlBackupFile],
    ) -> anyhow::Result<()> {
        let mut client = probe::connect(server, port, auth, password).await?;
        let target = target_database.replace(']', "]]");
        let moves = if source_database.eq_ignore_ascii_case(target_database) {
            String::new()
        } else {
            moves_from_files(&mut client, files, target_database).await?
        };
        restore_one_streamed(&mut client, pbs, archive, crypt.as_ref(), move |set| {
            format!(
                "RESTORE DATABASE [{target}] FROM VIRTUAL_DEVICE = '{set}' \
                 WITH REPLACE, RECOVERY{moves}"
            )
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn restore_chain_streamed(
        server: &str,
        port: Option<u16>,
        auth: &SqlAuth,
        password: Option<&str>,
        source_database: &str,
        target_database: &str,
        steps: Vec<(bool, SessionParams, String)>,
        crypt: Option<CryptConfig>,
        files: &[crate::sql::backupmeta::SqlBackupFile],
        target_unix: i64,
    ) -> anyhow::Result<()> {
        if steps.is_empty() {
            anyhow::bail!("empty restore chain");
        }
        let mut client = probe::connect(server, port, auth, password).await?;
        let target = target_database.replace(']', "]]");
        let new_name = !source_database.eq_ignore_ascii_case(target_database);

        // STOPAT must be a datetime in the server's local time.
        let offset_min: i32 = client
            .simple_query("SELECT DATEDIFF(MINUTE, GETUTCDATE(), GETDATE())")
            .await?
            .into_row()
            .await?
            .and_then(|r| r.get::<i32, _>(0))
            .unwrap_or(0);
        let local_unix = target_unix + (offset_min as i64) * 60;
        let stopat = chrono::DateTime::from_timestamp(local_unix, 0)
            .map(|d| d.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default();

        let moves = if new_name {
            moves_from_files(&mut client, files, target_database).await?
        } else {
            String::new()
        };

        for (i, (_is_log, pbs, archive)) in steps.iter().enumerate() {
            let target = target.clone();
            let moves = moves.clone();
            let stopat = stopat.clone();
            restore_one_streamed(&mut client, pbs, archive, crypt.as_ref(), move |set| {
                if i == 0 {
                    format!(
                        "RESTORE DATABASE [{target}] FROM VIRTUAL_DEVICE = '{set}' \
                         WITH REPLACE, NORECOVERY{moves}"
                    )
                } else {
                    format!(
                        "RESTORE LOG [{target}] FROM VIRTUAL_DEVICE = '{set}' \
                         WITH STOPAT = N'{stopat}', NORECOVERY"
                    )
                }
            })
            .await?;
        }

        client
            .simple_query(format!("RESTORE DATABASE [{target}] WITH RECOVERY"))
            .await?
            .into_results()
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
        cancel: CancellationToken,
    ) -> anyhow::Result<u64> {
        let mut sink = sink;
        run_device_session(set_name, ready, |vds| {
            drain_device(vds, set_name, &mut sink, &cancel)
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

    /// How long `GetCommand` waits before returning so the cancel token can be
    /// rechecked. SQL issues commands continuously during a backup, so this only
    /// adds a periodic wake-up when the device is momentarily idle.
    const CANCEL_POLL_MS: u32 = 1000;

    /// Backup direction: handle the device's `VDC_Write` commands by writing the
    /// backup bytes to `sink`, until the device closes. Polls `cancel` between
    /// commands; on cancellation it calls `SignalAbort` (the documented VDI abort,
    /// which fails the in-flight `BACKUP`) and bails.
    fn drain_device(
        vds: &IClientVirtualDeviceSet2,
        set_name: &str,
        sink: &mut Sink,
        cancel: &CancellationToken,
    ) -> anyhow::Result<u64> {
        let device = open_device(vds, set_name)?;
        let mut total: u64 = 0;
        loop {
            if cancel.is_cancelled() {
                // Abort the device set so SQL Server's BACKUP fails promptly and
                // the COM device thread is released (best effort).
                let _ = unsafe { vds.SignalAbort() };
                anyhow::bail!("backup cancelled");
            }
            let mut cmd_ptr: *mut VDCCommand = std::ptr::null_mut();
            let hr = unsafe { device.GetCommand(CANCEL_POLL_MS, &mut cmd_ptr) };
            if hr == VD_E_TIMEOUT {
                continue; // re-check the cancel token, then wait again
            }
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
        source: &mut dyn RestoreSource,
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
    use super::{backup_statement, ChannelReader};
    use pbsgui_ipc::SqlBackupType;
    use std::io::Read;

    #[test]
    fn backup_statement_full_log_and_escaping() {
        let full = backup_statement("MyDb", "pbsgui-1", SqlBackupType::Full, true).unwrap();
        assert_eq!(
            full,
            "BACKUP DATABASE [MyDb] TO VIRTUAL_DEVICE = 'pbsgui-1' WITH COPY_ONLY"
        );
        // Non-copy-only full omits the clause (resets the chain base).
        let base = backup_statement("MyDb", "s", SqlBackupType::Full, false).unwrap();
        assert_eq!(base, "BACKUP DATABASE [MyDb] TO VIRTUAL_DEVICE = 's'");
        // Log backups are always BACKUP LOG and never copy-only (so they truncate).
        let log = backup_statement("MyDb", "s", SqlBackupType::Log, true).unwrap();
        assert_eq!(log, "BACKUP LOG [MyDb] TO VIRTUAL_DEVICE = 's'");
        // A `]` in the name is doubled to stay inside the identifier.
        let weird = backup_statement("we]rd", "s", SqlBackupType::Full, false).unwrap();
        assert!(weird.contains("[we]]rd]"));
        // Differential is rejected for now.
        assert!(backup_statement("D", "s", SqlBackupType::Differential, false).is_err());
    }

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
    fn channel_reader_concatenates_buffers_and_ends_on_a_good_backup() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();
        tx.send(b"hello ".to_vec()).unwrap();
        tx.send(Vec::new()).unwrap(); // empty buffers are skipped
        tx.send(b"world".to_vec()).unwrap();
        drop(tx); // end of the data stream
        done_tx.send(true).unwrap(); // BACKUP succeeded: a clean end of stream

        let mut out = Vec::new();
        ChannelReader::new(rx, done_rx)
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn channel_reader_errors_when_the_backup_did_not_complete() {
        // A failed BACKUP (verdict false) must surface as an error, not EOF, so the
        // uploader does not commit the truncated bytes it already received.
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();
        tx.send(b"partial".to_vec()).unwrap();
        drop(tx);
        done_tx.send(false).unwrap();

        let mut out = Vec::new();
        let err = ChannelReader::new(rx, done_rx)
            .read_to_end(&mut out)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn channel_reader_errors_when_the_verdict_sender_is_dropped() {
        // A cancelled run drops the verdict sender without a value; that must read
        // as a truncated stream, never a clean end.
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();
        tx.send(b"partial".to_vec()).unwrap();
        drop(tx);
        drop(done_tx);

        let mut out = Vec::new();
        assert!(ChannelReader::new(rx, done_rx)
            .read_to_end(&mut out)
            .is_err());
    }
}
