//! Round-trip the IPC transport over a real local socket on the test machine.

use std::time::Duration;

use pbsgui_ipc::transport::{self, Responder};
use pbsgui_ipc::{BackupKind, BackupRequest, PbsDestination, Reply, Request, Target};

async fn demo_handler(request: Request, mut responder: Responder) {
    match request {
        Request::Ping => {
            let _ = responder.send(&Reply::Pong).await;
        }
        Request::StartBackup { .. } => {
            let _ = responder
                .send(&Reply::Accepted {
                    job_id: "job-1".into(),
                })
                .await;
            let _ = responder
                .send(&Reply::Progress {
                    fraction: 0.5,
                    message: "halfway".into(),
                })
                .await;
            let _ = responder
                .send(&Reply::Finished {
                    success: true,
                    message: "done".into(),
                })
                .await;
        }
    }
}

async fn collect(base: &str, request: Request) -> Vec<Reply> {
    // Retry connect until the listener is up.
    let mut last_err = None;
    for _ in 0..50 {
        let name = transport::socket_name(base).unwrap();
        let mut replies = Vec::new();
        match transport::send_request(name, &request, |r| replies.push(r)).await {
            Ok(()) => return replies,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    panic!("could not connect: {last_err:?}");
}

#[tokio::test]
async fn ping_pong() {
    let base = "pbsgui-test-ping";
    let name = transport::socket_name(base).unwrap();
    tokio::spawn(async move {
        let _ = transport::serve(name, demo_handler).await;
    });

    let replies = collect(base, Request::Ping).await;
    assert_eq!(replies, vec![Reply::Pong]);
}

#[tokio::test]
async fn backup_streams_progress_then_finished() {
    let base = "pbsgui-test-backup";
    let name = transport::socket_name(base).unwrap();
    tokio::spawn(async move {
        let _ = transport::serve(name, demo_handler).await;
    });

    let request = Request::StartBackup {
        destination: PbsDestination {
            repository: "u@pbs!t@host:8007:store".into(),
            secret: "s".into(),
            fingerprint: "ab".repeat(32),
            backup_id: "myhost".into(),
        },
        job: BackupRequest {
            target: Target::Filesystem {
                paths: vec!["C:/data".into()],
            },
            kind: BackupKind::FilesystemFull,
            copy_only: false,
        },
    };

    let replies = collect(base, request).await;
    assert!(matches!(replies.first(), Some(Reply::Accepted { .. })));
    assert!(replies.iter().any(|r| matches!(r, Reply::Progress { .. })));
    assert!(matches!(
        replies.last(),
        Some(Reply::Finished { success: true, .. })
    ));
}
