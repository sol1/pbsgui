//! Round-trip the generic transport with a custom protocol (not the SQL/files
//! one), proving serve_typed/send_request_typed work for a second engine such as
//! the Active Directory one.

use std::time::Duration;

use pbsgui_ipc::transport;
use pbsgui_ipc::{ErrorReply, Responder};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Req {
    Echo { text: String },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Rep {
    Echoed { text: String },
    Error { message: String },
}

impl ErrorReply for Rep {
    fn error(message: String) -> Self {
        Rep::Error { message }
    }
}

async fn handler(req: Req, mut responder: Responder<Rep>) {
    match req {
        Req::Echo { text } => {
            let _ = responder.send(&Rep::Echoed { text }).await;
        }
    }
}

async fn collect(base: &str, request: Req) -> Vec<Rep> {
    let mut last_err = None;
    for _ in 0..50 {
        let name = transport::socket_name(base).unwrap();
        let mut replies = Vec::new();
        match transport::send_request_typed::<Req, Rep, _>(name, &request, |r| replies.push(r))
            .await
        {
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
async fn generic_echo_round_trip() {
    let base = "pbsgui-test-generic-echo";
    let name = transport::socket_name(base).unwrap();
    tokio::spawn(async move {
        let _ = transport::serve_typed::<Req, Rep, _, _>(name, handler).await;
    });
    let replies = collect(base, Req::Echo { text: "hi".into() }).await;
    assert_eq!(replies, vec![Rep::Echoed { text: "hi".into() }]);
}
