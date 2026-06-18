//! IPC request handler: runs backup jobs and streams progress to the GUI.

use std::time::{SystemTime, UNIX_EPOCH};

use pbs_client::index::{self, FixedIndexBuilder, DEFAULT_CHUNK_SIZE};
use pbs_client::manifest::{BackupManifest, FileEntry, MANIFEST_BLOB_NAME};
use pbs_client::session::{BackupWriter, SessionParams};
use pbs_client::{blob, Repository};
use pbsgui_ipc::{BackupRequest, PbsDestination, Reply, Request, Responder, Target};

/// Archive name used for a filesystem image in this skeleton.
const ARCHIVE_NAME: &str = "data.img.fidx";
/// Append at most this many index entries per PUT.
const APPEND_BATCH: usize = 64;

/// Handle one IPC request.
pub async fn handle(request: Request, mut responder: Responder) {
    match request {
        Request::Ping => {
            let _ = responder.send(&Reply::Pong).await;
        }
        Request::StartBackup { destination, job } => {
            let job_id = format!("job-{}", unix_now());
            if responder
                .send(&Reply::Accepted {
                    job_id: job_id.clone(),
                })
                .await
                .is_err()
            {
                return;
            }
            let reply = match run_backup(&destination, &job, &mut responder).await {
                Ok(message) => Reply::Finished {
                    success: true,
                    message,
                },
                Err(e) => Reply::Finished {
                    success: false,
                    message: e.to_string(),
                },
            };
            let _ = responder.send(&reply).await;
        }
    }
}

async fn run_backup(
    destination: &PbsDestination,
    job: &BackupRequest,
    responder: &mut Responder,
) -> anyhow::Result<String> {
    let paths = match &job.target {
        Target::Filesystem { paths } => paths,
        Target::SqlDatabase { .. } => {
            anyhow::bail!("SQL Server backup is not implemented yet")
        }
    };
    let path = paths
        .first()
        .ok_or_else(|| anyhow::anyhow!("no path to back up"))?;

    log(responder, format!("reading {path}")).await;
    let data = tokio::fs::read(path)
        .await
        .map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
    if data.is_empty() {
        anyhow::bail!("{path} is empty");
    }
    let size = data.len() as u64;

    let repo: Repository = destination.repository.parse()?;
    let backup_time = unix_now();
    let params = SessionParams::from_repository(
        &repo,
        &destination.secret,
        &destination.fingerprint,
        "host",
        &destination.backup_id,
        backup_time,
    )?;

    log(
        responder,
        format!("connecting to {}", destination.repository),
    )
    .await;
    let mut writer = BackupWriter::connect(&params).await?;
    let wid = writer.create_fixed_index(ARCHIVE_NAME, size).await?;

    let chunk_size = DEFAULT_CHUNK_SIZE as usize;
    let total = data.len().div_ceil(chunk_size);
    let mut builder = FixedIndexBuilder::new(size, DEFAULT_CHUNK_SIZE, backup_time, random_uuid());
    let mut batch_digests = Vec::new();
    let mut batch_offsets = Vec::new();
    let mut offset = 0u64;

    for (i, chunk) in data.chunks(chunk_size).enumerate() {
        let digest = index::chunk_digest(chunk);
        let encoded = blob::encode_uncompressed(chunk);
        writer
            .upload_chunk(wid, &digest, chunk.len() as u64, &encoded)
            .await?;
        builder.push_digest(digest);
        batch_digests.push(digest);
        batch_offsets.push(offset);
        offset += chunk.len() as u64;
        if batch_digests.len() >= APPEND_BATCH {
            writer
                .append_fixed_index(wid, &batch_digests, &batch_offsets)
                .await?;
            batch_digests.clear();
            batch_offsets.clear();
        }
        let _ = responder
            .send(&Reply::Progress {
                fraction: (i + 1) as f32 / total as f32,
                message: format!("uploaded chunk {}/{}", i + 1, total),
            })
            .await;
    }
    if !batch_digests.is_empty() {
        writer
            .append_fixed_index(wid, &batch_digests, &batch_offsets)
            .await?;
    }

    let csum = builder.index_csum();
    writer
        .close_fixed_index(wid, builder.chunk_count() as u64, size, &csum)
        .await?;

    let entry = FileEntry::fixed_image(ARCHIVE_NAME, size, &csum);
    let manifest = BackupManifest::new("host", &destination.backup_id, backup_time, vec![entry]);
    let manifest_blob = blob::encode_uncompressed(&manifest.to_json_bytes()?);
    writer
        .upload_blob(MANIFEST_BLOB_NAME, &manifest_blob)
        .await?;
    writer.finish().await?;

    Ok(format!(
        "backed up {size} bytes as host/{}/{backup_time}",
        destination.backup_id
    ))
}

async fn log(responder: &mut Responder, line: String) {
    tracing::info!("{line}");
    let _ = responder.send(&Reply::Log { line }).await;
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn random_uuid() -> [u8; 16] {
    *uuid::Uuid::new_v4().as_bytes()
}
