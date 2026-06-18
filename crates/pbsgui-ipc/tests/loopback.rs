//! Round-trip the IPC transport over a real local socket on the test machine.

use std::time::Duration;

use pbsgui_ipc::transport::{self, Responder};
use pbsgui_ipc::{Job, PbsDestination, Reply, Request, Schedule};

async fn demo_handler(request: Request, mut responder: Responder) {
    match request {
        Request::Ping => {
            let _ = responder.send(&Reply::Pong).await;
        }
        Request::ListJobs => {
            let _ = responder.send(&Reply::Jobs { jobs: vec![] }).await;
        }
        Request::SaveJob { job, .. } => {
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
        destination: PbsDestination {
            repository: "u@pbs!t@host:8007:store".into(),
            fingerprint: "ab".repeat(32),
            backup_id: "myhost".into(),
        },
        sources: vec!["C:/data".into()],
        excludes: vec![],
        schedule: Schedule::Daily {
            hour: 2,
            minute: 30,
        },
        last_run: None,
        last_status: None,
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

    let saved = collect(
        base,
        Request::SaveJob {
            job: sample_job(),
            secret: Some("s".into()),
        },
    )
    .await;
    assert_eq!(saved, vec![Reply::Saved { id: "job-1".into() }]);

    let run = collect(base, Request::RunJob { id: "job-1".into() }).await;
    assert!(matches!(run.first(), Some(Reply::Accepted { .. })));
    assert!(run.iter().any(|r| matches!(r, Reply::Progress { .. })));
    assert!(matches!(
        run.last(),
        Some(Reply::Finished { success: true, .. })
    ));
}
