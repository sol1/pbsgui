//! Proxy-side relay server: accepts agent control connections (register,
//! keepalive, session outcomes) and per-session data connections, and hands
//! the backup path a blocking reader over a session's raw byte stream.
//!
//! The reader carries the ChannelReader truncation rule across the network: a
//! data stream that ends without a clean `End { ok: true }` frame surfaces as
//! an I/O error, never as end-of-file, so the PBS uploader can never commit a
//! snapshot from a stream the agent did not finish.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::io::ReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::proto::{read_frame, write_control, ControlMsg, Frame, RELAY_PROTO_VERSION};
use super::tls::ServerTls;

type TlsStream = tokio_rustls::server::TlsStream<TcpStream>;

/// How long a peer has to present its first frame (Register or Hello).
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the proxy waits for the agent's data connection after commanding
/// a session (covers the agent's dial + TLS + VDI device creation).
const DATA_CONN_TIMEOUT: Duration = Duration::from_secs(60);
/// Buffered data frames between the socket pump and the consuming reader.
const DATA_QUEUE: usize = 16;

/// A configured agent: its expected token.
#[derive(Debug, Clone)]
pub struct AgentAuth {
    pub name: String,
    pub token: String,
}

/// A connected agent, as shown to the UI.
#[derive(Debug, Clone)]
pub struct AgentStatus {
    pub name: String,
    pub host: String,
    pub version: String,
}

/// A connected agent's server-side handle.
struct AgentConn {
    host: String,
    version: String,
    control: mpsc::Sender<ControlMsg>,
}

/// A commanded session waiting for its data connection.
struct SessionSlot {
    expected_token: String,
    deliver: oneshot::Sender<TlsStream>,
}

struct Shared {
    /// Configured agents: name -> expected token.
    tokens: HashMap<String, String>,
    /// Currently connected agents.
    agents: Mutex<HashMap<String, AgentConn>>,
    /// Sessions whose data connection has not arrived yet.
    sessions: Mutex<HashMap<String, SessionSlot>>,
}

/// The relay listener and registry. Cheap to clone; all state is shared.
#[derive(Clone)]
pub struct RelayServer {
    shared: Arc<Shared>,
}

impl RelayServer {
    pub fn new(agents: Vec<AgentAuth>) -> Self {
        Self {
            shared: Arc::new(Shared {
                tokens: agents.into_iter().map(|a| (a.name, a.token)).collect(),
                agents: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The currently connected agents.
    pub fn agents(&self) -> Vec<AgentStatus> {
        self.shared
            .agents
            .lock()
            .unwrap()
            .iter()
            .map(|(name, a)| AgentStatus {
                name: name.clone(),
                host: a.host.clone(),
                version: a.version.clone(),
            })
            .collect()
    }

    /// Accept relay connections until `shutdown` trips.
    pub async fn run(&self, listener: TcpListener, tls: ServerTls, shutdown: CancellationToken) {
        loop {
            let (tcp, peer) = tokio::select! {
                r = listener.accept() => match r {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("relay accept failed: {e}");
                        continue;
                    }
                },
                _ = shutdown.cancelled() => return,
            };
            let acceptor = tls.acceptor.clone();
            let shared = self.shared.clone();
            tokio::spawn(async move {
                let stream = match acceptor.accept(tcp).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!("relay TLS handshake from {peer} failed: {e}");
                        return;
                    }
                };
                if let Err(e) = handle_conn(shared, stream).await {
                    tracing::debug!("relay connection from {peer} ended: {e:#}");
                }
            });
        }
    }

    /// Start a backup device session on `agent` and return a blocking reader
    /// over its byte stream (feed it to the PBS uploader). The caller issues
    /// the BACKUP statement itself once this returns; `set_name` is the VDI
    /// device set the statement must name.
    pub async fn backup_stream(
        &self,
        agent: &str,
        instance: Option<String>,
        set_name: &str,
    ) -> anyhow::Result<RelayRead> {
        let control = {
            let agents = self.shared.agents.lock().unwrap();
            agents
                .get(agent)
                .map(|a| a.control.clone())
                .with_context(|| format!("relay agent '{agent}' is not connected"))?
        };
        let expected_token = self
            .shared
            .tokens
            .get(agent)
            .cloned()
            .with_context(|| format!("relay agent '{agent}' is not configured"))?;

        let session_id = Uuid::new_v4().to_string();
        let (deliver, arrival) = oneshot::channel();
        self.shared.sessions.lock().unwrap().insert(
            session_id.clone(),
            SessionSlot {
                expected_token,
                deliver,
            },
        );

        let start = ControlMsg::StartBackup {
            session_id: session_id.clone(),
            instance,
            set_name: set_name.to_string(),
        };
        if control.send(start).await.is_err() {
            self.shared.sessions.lock().unwrap().remove(&session_id);
            anyhow::bail!("relay agent '{agent}' disconnected");
        }

        let stream = match tokio::time::timeout(DATA_CONN_TIMEOUT, arrival).await {
            Ok(Ok(stream)) => stream,
            _ => {
                self.shared.sessions.lock().unwrap().remove(&session_id);
                anyhow::bail!(
                    "relay agent '{agent}' did not open a data connection within {}s",
                    DATA_CONN_TIMEOUT.as_secs()
                );
            }
        };

        // Pump frames into a bounded channel (network backpressure) and keep
        // the verdict on a separate channel, exactly like the local VDI path.
        let (data_tx, data_rx) = mpsc::channel::<Vec<u8>>(DATA_QUEUE);
        let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();
        tokio::spawn(pump_data(stream, data_tx, done_tx));
        Ok(RelayRead {
            rx: data_rx,
            done: done_rx,
            buf: Vec::new(),
            pos: 0,
            verdict: None,
        })
    }
}

/// Read a session's data connection, forwarding payloads until `End` (whose
/// verdict goes to `done`) or a broken stream (drop `done` unfired = failure).
async fn pump_data(
    stream: TlsStream,
    data_tx: mpsc::Sender<Vec<u8>>,
    done_tx: std::sync::mpsc::Sender<bool>,
) {
    let (mut rd, _wr) = tokio::io::split(stream);
    loop {
        match read_frame(&mut rd).await {
            Ok(Some(Frame::Data(bytes))) => {
                if data_tx.send(bytes).await.is_err() {
                    return; // consumer stopped (upload failed/cancelled)
                }
            }
            Ok(Some(Frame::Control(ControlMsg::End { ok, error }))) => {
                if let Some(error) = error {
                    tracing::warn!("relay data stream ended with an error: {error}");
                }
                let _ = done_tx.send(ok);
                // Returning drops the stream: the FIN tells the agent it can
                // stop draining and close its side.
                return;
            }
            // Unexpected frame, EOF without End, or a transport error: the
            // dropped `done_tx` marks the stream truncated.
            Ok(Some(f)) => {
                tracing::warn!("relay data stream: unexpected frame {f:?}");
                return;
            }
            Ok(None) => {
                tracing::warn!("relay data stream closed without an End frame");
                return;
            }
            Err(e) => {
                tracing::warn!("relay data stream read failed: {e}");
                return;
            }
        }
    }
}

/// Blocking reader over a relay data stream, for the PBS uploader. End of
/// stream is reported only after a clean `End { ok: true }`; anything else is
/// an error, so a truncated stream can never commit (ChannelReader's rule).
pub struct RelayRead {
    rx: mpsc::Receiver<Vec<u8>>,
    done: std::sync::mpsc::Receiver<bool>,
    buf: Vec<u8>,
    pos: usize,
    verdict: Option<bool>,
}

impl Read for RelayRead {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.blocking_recv() {
                Some(next) => {
                    self.buf = next;
                    self.pos = 0;
                }
                None => {
                    let ok = match self.verdict {
                        Some(v) => v,
                        None => {
                            let v = self.done.recv().unwrap_or(false);
                            self.verdict = Some(v);
                            v
                        }
                    };
                    if ok {
                        return Ok(0);
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the relay data stream did not complete; refusing to treat a \
                         truncated backup as whole",
                    ));
                }
            }
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Classify and serve one inbound connection by its first frame.
async fn handle_conn(shared: Arc<Shared>, stream: TlsStream) -> anyhow::Result<()> {
    let mut stream = stream;
    let first = tokio::time::timeout(FIRST_FRAME_TIMEOUT, read_frame(&mut stream))
        .await
        .context("timed out waiting for the first frame")??
        .context("connection closed before the first frame")?;
    match first {
        Frame::Control(ControlMsg::Register {
            agent_name,
            host,
            token,
            version,
            proto,
        }) => serve_control(shared, stream, agent_name, host, token, version, proto).await,
        Frame::Control(ControlMsg::Hello { session_id, token }) => {
            let slot = shared.sessions.lock().unwrap().remove(&session_id);
            match slot {
                Some(slot) if slot.expected_token == token => {
                    // Hand the socket to the waiting backup_stream call.
                    let _ = slot.deliver.send(stream);
                    Ok(())
                }
                Some(_) => anyhow::bail!("data connection for session {session_id}: bad token"),
                None => anyhow::bail!("data connection for unknown session {session_id}"),
            }
        }
        other => anyhow::bail!("unexpected first frame: {other:?}"),
    }
}

/// Serve a registered agent's control connection until it drops.
#[allow(clippy::too_many_arguments)]
async fn serve_control(
    shared: Arc<Shared>,
    stream: TlsStream,
    agent_name: String,
    host: String,
    token: String,
    version: String,
    proto: u32,
) -> anyhow::Result<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);

    let refusal = if proto != RELAY_PROTO_VERSION {
        Some(format!(
            "protocol mismatch: agent speaks v{proto}, this proxy v{RELAY_PROTO_VERSION} \
             (update the older side)"
        ))
    } else {
        match shared.tokens.get(&agent_name) {
            Some(expected) if *expected == token => None,
            Some(_) => Some("bad token".to_string()),
            None => Some(format!(
                "agent '{agent_name}' is not configured on this proxy"
            )),
        }
    };
    if let Some(message) = refusal {
        write_control(
            &mut wr,
            &ControlMsg::Registered {
                ok: false,
                message: message.clone(),
            },
        )
        .await?;
        anyhow::bail!("refused agent '{agent_name}': {message}");
    }
    write_control(
        &mut wr,
        &ControlMsg::Registered {
            ok: true,
            message: "welcome".to_string(),
        },
    )
    .await?;

    // Session-start commands (from backup_stream) and Pings funnel through a
    // channel to this connection's writer task.
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
    let this_conn = ctl_tx.clone();
    shared.agents.lock().unwrap().insert(
        agent_name.clone(),
        AgentConn {
            host,
            version,
            control: ctl_tx,
        },
    );
    tracing::info!("relay agent '{agent_name}' connected");

    let writer = tokio::spawn(async move {
        while let Some(msg) = ctl_rx.recv().await {
            if write_control(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });

    let result = read_control_loop(&mut rd).await;

    // Only deregister if the registry still points at this connection (a
    // reconnect may already have replaced the entry with a fresh one).
    let mut agents = shared.agents.lock().unwrap();
    if agents
        .get(&agent_name)
        .is_some_and(|a| a.control.same_channel(&this_conn))
    {
        agents.remove(&agent_name);
    }
    drop(agents);
    writer.abort();
    tracing::info!("relay agent '{agent_name}' disconnected");
    result
}

/// Consume an agent's control frames (session outcomes, keepalive replies).
async fn read_control_loop(rd: &mut ReadHalf<TlsStream>) -> anyhow::Result<()> {
    loop {
        match read_frame(rd).await? {
            Some(Frame::Control(ControlMsg::SessionDone {
                session_id,
                ok,
                error,
                bytes,
            })) => {
                // Informational: the data stream's End frame is authoritative
                // for truncation; this reports the device loop's own outcome.
                if ok {
                    tracing::debug!("relay session {session_id} finished: {bytes} bytes");
                } else {
                    tracing::warn!(
                        "relay session {session_id} failed on the agent: {}",
                        error.as_deref().unwrap_or("unknown error")
                    );
                }
            }
            Some(Frame::Control(ControlMsg::Pong)) => {}
            Some(other) => anyhow::bail!("unexpected frame from the agent: {other:?}"),
            None => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::agent::{run_agent, AgentConfig, DeviceRunner};

    /// A fake device: emits a deterministic byte pattern, or fails mid-stream.
    struct FakeDevice {
        chunks: usize,
        fail_after: Option<usize>,
    }

    impl DeviceRunner for FakeDevice {
        fn run_backup(
            &self,
            _instance: Option<&str>,
            set_name: &str,
            tx: mpsc::Sender<Vec<u8>>,
            _cancel: &CancellationToken,
        ) -> anyhow::Result<u64> {
            let mut total = 0u64;
            for i in 0..self.chunks {
                if self.fail_after == Some(i) {
                    anyhow::bail!("device exploded mid-stream");
                }
                let buf = vec![(i % 251) as u8; 8192];
                total += buf.len() as u64;
                if tx.blocking_send(buf).is_err() {
                    anyhow::bail!("proxy stopped consuming");
                }
            }
            // The set name reaching the device intact is part of the contract.
            assert!(
                set_name.starts_with("set-"),
                "unexpected set name {set_name}"
            );
            Ok(total)
        }
    }

    /// Bring up a real server + agent pair over pinned TLS on loopback.
    async fn start_pair(
        agents: Vec<AgentAuth>,
        agent_cfg_token: &str,
        runner: Arc<dyn DeviceRunner>,
    ) -> (RelayServer, CancellationToken) {
        let dir = std::env::temp_dir().join(format!(
            "pbsgui-relay-pair-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let tls = crate::relay::tls::server_tls(&dir).unwrap();
        let fingerprint = tls.fingerprint.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = RelayServer::new(agents);
        let shutdown = CancellationToken::new();
        tokio::spawn({
            let server = server.clone();
            let shutdown = shutdown.clone();
            async move { server.run(listener, tls, shutdown).await }
        });
        tokio::spawn(run_agent(
            AgentConfig {
                proxy_addr: addr.to_string(),
                proxy_fingerprint: fingerprint,
                agent_name: "a1".into(),
                token: agent_cfg_token.into(),
            },
            runner,
            shutdown.clone(),
        ));
        (server, shutdown)
    }

    async fn wait_for_agent(server: &RelayServer) -> bool {
        for _ in 0..100 {
            if !server.agents().is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backup_stream_relays_all_bytes() {
        let auth = vec![AgentAuth {
            name: "a1".into(),
            token: "tok".into(),
        }];
        let device = Arc::new(FakeDevice {
            chunks: 40,
            fail_after: None,
        });
        let (server, shutdown) = start_pair(auth, "tok", device).await;
        assert!(wait_for_agent(&server).await, "agent never registered");

        let reader = server.backup_stream("a1", None, "set-1").await.unwrap();
        let bytes = tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let mut out = Vec::new();
            reader.read_to_end(&mut out).map(|_| out)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(bytes.len(), 40 * 8192);
        // Spot-check the pattern survived the trip.
        assert!(bytes[..8192].iter().all(|b| *b == 0));
        assert!(bytes[8192..16384].iter().all(|b| *b == 1));
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_device_truncates_the_stream() {
        let auth = vec![AgentAuth {
            name: "a1".into(),
            token: "tok".into(),
        }];
        let device = Arc::new(FakeDevice {
            chunks: 10,
            fail_after: Some(4),
        });
        let (server, shutdown) = start_pair(auth, "tok", device).await;
        assert!(wait_for_agent(&server).await, "agent never registered");

        let reader = server.backup_stream("a1", None, "set-2").await.unwrap();
        let err = tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let mut out = Vec::new();
            reader.read_to_end(&mut out).map(|_| out.len())
        })
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        shutdown.cancel();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrong_token_is_refused() {
        let auth = vec![AgentAuth {
            name: "a1".into(),
            token: "tok".into(),
        }];
        let device = Arc::new(FakeDevice {
            chunks: 1,
            fail_after: None,
        });
        let (server, shutdown) = start_pair(auth, "WRONG", device).await;
        // The agent retries with backoff but must never appear registered.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(server.agents().is_empty());
        // And a session against an unconnected agent fails fast.
        assert!(server.backup_stream("a1", None, "set-3").await.is_err());
        shutdown.cancel();
    }
}
