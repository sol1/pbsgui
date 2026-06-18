//! Demonstrate deduplicated dynamic backup against a real PBS.
//!
//! Backs up a ~20 MiB file, changes a little, backs it up again to the same
//! group, and reports how many chunks the second run reused (skipped uploading),
//! then restores the latest snapshot and verifies it.
//!
//! ```sh
//! export PBS_REPOSITORY='user@pbs!token@host:8007:datastore'
//! export PBS_PASSWORD='<secret>'
//! export PBS_FINGERPRINT='AA:BB:..'
//! cargo run -p pbs-client --example dynamic_backup
//! ```

use std::error::Error;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use pbs_client::session::{backup_dynamic_file, ReaderClient, SessionParams};
use pbs_client::{PbsError, Repository};

const ARCHIVE: &str = "data.didx";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let repo: Repository = env("PBS_REPOSITORY")?.parse()?;
    let secret = env("PBS_PASSWORD")?;
    let fingerprint = env("PBS_FINGERPRINT")?;
    let backup_id = std::env::var("PBS_BACKUP_ID").unwrap_or_else(|_| "pbsgui-dedup".to_string());

    let mut data: Vec<u8> = (0..20u32 * 1024 * 1024)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let tmp = std::env::temp_dir().join("pbsgui-dedup-sample.bin");
    write(&tmp, &data)?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    let p1 = params(&repo, &secret, &fingerprint, &backup_id, now)?;
    let s1 = backup_dynamic_file(&p1, ARCHIVE, &tmp, true).await?;
    println!(
        "run 1: {} chunks, {} uploaded, {} reused, {} bytes",
        s1.chunks, s1.uploaded, s1.reused, s1.bytes
    );

    // Change the first chunk only; the rest should be reused on the next run.
    for b in data.iter_mut().take(100) {
        *b = b.wrapping_add(1);
    }
    write(&tmp, &data)?;

    let p2 = params(&repo, &secret, &fingerprint, &backup_id, now + 10)?;
    let s2 = backup_dynamic_file(&p2, ARCHIVE, &tmp, true).await?;
    println!(
        "run 2: {} chunks, {} uploaded, {} reused, {} bytes",
        s2.chunks, s2.uploaded, s2.reused, s2.bytes
    );
    println!(
        "dedup: run 2 re-used {}/{} chunks (only changed chunks uploaded)",
        s2.reused, s2.chunks
    );

    let mut reader = ReaderClient::connect(&p2).await?;
    let restored = reader.restore_dynamic_archive(ARCHIVE).await?;
    if restored == data {
        println!(
            "OK: restored {} bytes match the latest backup",
            restored.len()
        );
        Ok(())
    } else {
        Err(format!(
            "MISMATCH: restored {} bytes, expected {}",
            restored.len(),
            data.len()
        )
        .into())
    }
}

fn params(
    repo: &Repository,
    secret: &str,
    fingerprint: &str,
    backup_id: &str,
    backup_time: i64,
) -> Result<SessionParams, PbsError> {
    SessionParams::from_repository(repo, secret, fingerprint, "host", backup_id, backup_time)
}

fn write(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::File::create(path)?.write_all(data)
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} must be set"))
}
