//! Live integration test against a real Proxmox Backup Server.
//!
//! Ignored by default (it needs a server). Run it explicitly with the same
//! environment as the `backup_image` example:
//!
//! ```sh
//! export PBS_REPOSITORY='user@pbs!token@host:8007:datastore'
//! export PBS_PASSWORD='<secret>'
//! export PBS_FINGERPRINT='AA:BB:..'
//! cargo test -p pbs-client --test live_roundtrip -- --ignored --nocapture
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use pbs_client::session::{backup_fixed_image, ReaderClient, SessionParams};
use pbs_client::Repository;

#[tokio::test]
#[ignore = "requires a live PBS server; set PBS_REPOSITORY, PBS_PASSWORD, PBS_FINGERPRINT"]
async fn fixed_image_backup_and_restore() {
    let (repo, secret, fingerprint) = match (
        std::env::var("PBS_REPOSITORY"),
        std::env::var("PBS_PASSWORD"),
        std::env::var("PBS_FINGERPRINT"),
    ) {
        (Ok(r), Ok(s), Ok(f)) => (r, s, f),
        _ => {
            eprintln!("skipping: PBS_REPOSITORY / PBS_PASSWORD / PBS_FINGERPRINT not all set");
            return;
        }
    };

    let repo: Repository = repo.parse().expect("valid repository");
    let backup_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let backup_id =
        std::env::var("PBS_BACKUP_ID").unwrap_or_else(|_| "pbsgui-spike-test".to_string());

    let image: Vec<u8> = (0..(8u32 * 1024 * 1024 + 7))
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
    )
    .expect("session params");

    backup_fixed_image(&params, archive, &image)
        .await
        .expect("backup");

    let mut reader = ReaderClient::connect(&params)
        .await
        .expect("reader connect");
    let restored = reader.restore_fixed_image(archive).await.expect("restore");

    assert_eq!(restored, image, "restored image must match the original");
}
