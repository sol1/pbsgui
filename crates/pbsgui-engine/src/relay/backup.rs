//! Proxy-side SQL backup through a relay agent: the agent runs the VDI device
//! on the SQL host and streams raw bytes here; this machine issues the BACKUP
//! statement over TDS and carries the chunk/compress/encrypt/upload work.
//!
//! Cross-platform: nothing here touches SQLVDI, so a Linux dev build compiles
//! and the proxy role could in principle run anywhere the engine runs.

use pbs_client::{BackupProgress, BackupStats, CryptConfig, SessionParams};
use pbsgui_ipc::{SqlAuth, SqlBackupType};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::sql::probe;
use crate::sql::vdi;

/// Back up `database` to PBS through the given relay agent. The mirror of
/// `vdi::backup_database_to_pbs` with the byte source swapped: the relay
/// reader (which enforces the End-verdict truncation rule) replaces the local
/// device loop's channel. Gating, metadata capture, and grouping stay with
/// the caller, exactly as for the local path.
#[allow(clippy::too_many_arguments)]
pub async fn backup_database_to_pbs<F: FnMut(&BackupProgress) + Send + 'static>(
    agent: &str,
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
    let relay = super::server::global().ok_or_else(|| {
        anyhow::anyhow!(
            "this machine has no relay listener configured; run `pbsgui-engine relay add-agent` \
             here first (the proxy is the machine that runs the backup jobs)"
        )
    })?;

    let mut client = probe::connect(server, port, auth, password).await?;
    let set_name = format!("pbsgui-{}", Uuid::new_v4());

    // Command the device session on the agent; when this returns, the device
    // set exists on the SQL host and its byte stream is wired to `reader`.
    let instance = vdi::instance_of(server).map(str::to_string);
    let reader = relay
        .backup_stream(agent, instance, &set_name)
        .await
        .map_err(|e| e.context(format!("starting the relay session on agent '{agent}'")))?;

    // BACKUP (pushes bytes into the device on the SQL host) and the PBS upload
    // (drains the relayed stream) run concurrently, exactly like the local
    // path. After BACKUP yields its LSNs, ship the chain metadata to the
    // uploader so point-in-time restore can order the chain.
    let (meta_tx, meta_rx) = tokio::sync::oneshot::channel::<(String, Vec<u8>)>();
    let backup_and_meta = async {
        let result = vdi::issue_backup(&mut client, database, &set_name, backup_type, copy_only)
            .await
            .map_err(|e| e.context("the BACKUP statement failed on the remote instance"));
        if result.is_ok() {
            match vdi::query_backup_meta(&mut client, database).await {
                Ok(meta) => {
                    let _ =
                        meta_tx.send((crate::sql::backupmeta::META_BLOB_NAME.to_string(), meta));
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
        reader,
        total_estimate,
        Some(meta_rx),
        crypt,
        on_progress,
    );

    // Cancellation: the caller's select drops this whole future; dropping the
    // upload closes the relay reader, the agent's sends fail, and its device
    // loop faults the BACKUP on the SQL side - nothing half-commits. The
    // explicit check here just makes an early cancel fail fast.
    if cancel.is_cancelled() {
        anyhow::bail!("cancelled by request");
    }
    let (backup, upload) = tokio::join!(backup_and_meta, upload_fut);

    vdi::combine_pbs_result(backup, upload)
}
