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

use pbs_client::session::{backup_dynamic_file, backup_fixed_image, ReaderClient, SessionParams};
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

#[tokio::test]
#[ignore = "requires a live PBS server; set PBS_REPOSITORY, PBS_PASSWORD, PBS_FINGERPRINT"]
async fn dynamic_dedup_backup_and_restore() {
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let backup_id = "pbsgui-dedup-test".to_string();
    let archive = "data.didx";

    let mut data: Vec<u8> = (0..16u32 * 1024 * 1024)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let tmp = std::env::temp_dir().join("pbsgui-dedup-test.bin");
    std::fs::write(&tmp, &data).unwrap();

    let p1 = SessionParams::from_repository(&repo, &secret, &fingerprint, "host", &backup_id, now)
        .unwrap();
    backup_dynamic_file(&p1, archive, &tmp, true)
        .await
        .expect("run 1");

    // Change the first chunk only; the rest must be reused.
    for b in data.iter_mut().take(50) {
        *b = b.wrapping_add(1);
    }
    std::fs::write(&tmp, &data).unwrap();
    let p2 =
        SessionParams::from_repository(&repo, &secret, &fingerprint, "host", &backup_id, now + 10)
            .unwrap();
    let stats = backup_dynamic_file(&p2, archive, &tmp, true)
        .await
        .expect("run 2");
    assert!(stats.reused > 0, "second run should reuse chunks via dedup");

    let mut reader = ReaderClient::connect(&p2).await.expect("reader");
    let restored = reader
        .restore_dynamic_archive(archive)
        .await
        .expect("restore");
    assert_eq!(
        restored, data,
        "restored archive must match the latest backup"
    );
}
