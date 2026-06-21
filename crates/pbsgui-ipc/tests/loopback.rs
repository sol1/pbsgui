//! Round-trip the IPC transport over a real local socket on the test machine.

use std::time::Duration;

use pbsgui_ipc::transport::{self, Responder};
use pbsgui_ipc::{Job, JobDestination, JobSource, Reply, Request, Schedule};

async fn demo_handler(request: Request, mut responder: Responder) {
    match request {
        Request::Ping => {
            let _ = responder.send(&Reply::Pong).await;
        }
        Request::ListJobs => {
            let _ = responder.send(&Reply::Jobs { jobs: vec![] }).await;
        }
        Request::SaveJob { job } => {
            let _ = responder.send(&Reply::Saved { id: job.id }).await;
        }
        Request::DeleteJob { .. } => {
            let _ = responder.send(&Reply::Deleted).await;
        }
        Request::RunJob { id } => {
            let _ = responder.send(&Reply::Accepted { job_id: id }).await;
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
        Request::ListSnapshots { .. } => {
            let _ = responder
                .send(&Reply::Snapshots { snapshots: vec![] })
                .await;
        }
        Request::ListFiles { .. } => {
            let _ = responder.send(&Reply::Files { files: vec![] }).await;
        }
        Request::Restore { .. } => {
            let _ = responder
                .send(&Reply::Finished {
                    success: true,
                    message: "restored".into(),
                })
                .await;
        }
        Request::DiscoverSql { .. } => {
            let _ = responder
                .send(&Reply::SqlInstances { instances: vec![] })
                .await;
        }
        Request::ProbeSql { .. } => {
            let _ = responder
                .send(&Reply::Error {
                    message: "probe not supported in loopback test".into(),
                })
                .await;
        }
        Request::BackupSqlToFile { .. } => {
            let _ = responder
                .send(&Reply::Finished {
                    success: true,
                    message: "backed up".into(),
                })
                .await;
        }
        Request::CheckSql { .. } => {
            let _ = responder.send(&Reply::SqlChecks { checks: vec![] }).await;
        }
        Request::ListSqlSnapshots { .. } => {
            let _ = responder
                .send(&Reply::Snapshots { snapshots: vec![] })
                .await;
        }
        Request::RestoreSql { .. } => {
            let _ = responder
                .send(&Reply::Finished {
                    success: true,
                    message: "restored".into(),
                })
                .await;
        }
        Request::BackupSqlToPbs { .. } => {
            let _ = responder
                .send(&Reply::Finished {
                    success: true,
                    message: "backed up to pbs".into(),
                })
                .await;
        }
        Request::ListSqlConnections => {
            let _ = responder
                .send(&Reply::SqlConnections {
                    connections: vec![],
                })
                .await;
        }
        Request::SaveSqlConnection { connection, .. } => {
            let _ = responder.send(&Reply::Saved { id: connection.id }).await;
        }
        Request::DeleteSqlConnection { .. } => {
            let _ = responder.send(&Reply::Deleted).await;
        }
        Request::ListPbsServers => {
            let _ = responder.send(&Reply::PbsServers { servers: vec![] }).await;
        }
        Request::SavePbsServer { server, .. } => {
            let _ = responder.send(&Reply::Saved { id: server.id }).await;
        }
        Request::DeletePbsServer { .. } => {
            let _ = responder.send(&Reply::Deleted).await;
        }
        Request::GenerateEncryptionKey { .. }
        | Request::ImportEncryptionKey { .. }
        | Request::GetEncryptionKey { .. }
        | Request::ClearEncryptionKey { .. } => {
            let _ = responder.send(&Reply::EncryptionKey { info: None }).await;
        }
    }
}

async fn collect(base: &str, request: Request) -> Vec<Reply> {
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

fn sample_job() -> Job {
    Job {
        id: "job-1".into(),
        name: "Docs".into(),
        source: JobSource::Files {
            sources: vec!["C:/data".into()],
            excludes: vec![],
            change_detection: false,
        },
        destination: JobDestination::Pbs {
            server_id: "s".into(),
            backup_id: "myhost".into(),
        },
        schedule: Schedule::Daily {
            hour: 2,
            minute: 30,
        },
        pre_script: None,
        post_script: None,
        last_run: None,
        last_status: None,
        encrypted: false,
    }
}

#[tokio::test]
async fn ping_pong() {
    let base = "pbsgui-test-ping";
    let name = transport::socket_name(base).unwrap();
    tokio::spawn(async move {
        let _ = transport::serve(name, demo_handler).await;
    });
    assert_eq!(collect(base, Request::Ping).await, vec![Reply::Pong]);
}

#[tokio::test]
async fn save_then_run_streams_progress() {
    let base = "pbsgui-test-jobs";
    let name = transport::socket_name(base).unwrap();
    tokio::spawn(async move {
        let _ = transport::serve(name, demo_handler).await;
    });

    let saved = collect(base, Request::SaveJob { job: sample_job() }).await;
    assert_eq!(saved, vec![Reply::Saved { id: "job-1".into() }]);

    let run = collect(base, Request::RunJob { id: "job-1".into() }).await;
    assert!(matches!(run.first(), Some(Reply::Accepted { .. })));
    assert!(run.iter().any(|r| matches!(r, Reply::Progress { .. })));
    assert!(matches!(
        run.last(),
        Some(Reply::Finished { success: true, .. })
    ));
}
