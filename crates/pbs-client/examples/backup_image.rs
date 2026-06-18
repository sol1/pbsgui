//! End-to-end spike: back up an in-memory image to a real PBS as a fixed-index
//! archive, then restore it through the reader protocol and verify the bytes.
//!
//! Run against a real Proxmox Backup Server with an API token:
//!
//! ```sh
//! export PBS_REPOSITORY='user@pbs!token@pbs.example.com:8007:datastore'
//! export PBS_PASSWORD='<api-token-secret>'
//! export PBS_FINGERPRINT='AA:BB:..:FF'   # server cert SHA-256
//! cargo run -p pbs-client --example backup_image
//! ```
//!
//! The token's user must be able to write to the datastore (a sysadmin or a
//! datastore owner / DatastoreBackup role).

use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};

use pbs_client::session::{backup_fixed_image, ReaderClient, SessionParams};
use pbs_client::Repository;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let repo: Repository = env("PBS_REPOSITORY")?.parse()?;
    let secret = env("PBS_PASSWORD")?;
    let fingerprint = env("PBS_FINGERPRINT")?;
    let backup_id = std::env::var("PBS_BACKUP_ID").unwrap_or_else(|_| "pbsgui-spike".to_string());
    let backup_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    // Sample image: a few 4 MiB chunks plus a partial tail.
    let image: Vec<u8> = (0..(10u32 * 1024 * 1024 + 12_345))
        .map(|i| (i % 251) as u8)
        .collect();
    let archive = "data.img.fidx";

    let params = SessionParams::from_repository(
        &repo,
        &secret,
        &fingerprint,
        "host",
        &backup_id,
        backup_time,
    )?;

    println!(
        "backing up {} bytes to {} (snapshot host/{}/{})",
        image.len(),
        repo,
        backup_id,
        backup_time
    );
    let csum = backup_fixed_image(&params, archive, &image).await?;
    println!("backup finished, index csum {}", hex::encode(csum));

    println!("restoring and verifying...");
    let mut reader = ReaderClient::connect(&params).await?;
    let restored = reader.restore_fixed_image(archive).await?;

    if restored == image {
        println!("OK: restored {} bytes match the original", restored.len());
        Ok(())
    } else {
        Err(format!(
            "MISMATCH: restored {} bytes, expected {}",
            restored.len(),
            image.len()
        )
        .into())
    }
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} must be set"))
}
