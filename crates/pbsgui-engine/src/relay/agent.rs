//! The relay agent: the thin end of the SQL backup relay, running on the SQL
//! Server host. It keeps one outbound control connection to its proxy (the SQL
//! host never listens), runs VDI device sessions when commanded, and streams
//! each session's raw bytes over a dedicated data connection. It holds no SQL
//! or PBS credentials; the proxy issues all T-SQL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_util::sync::CancellationToken;

use super::proto::{read_frame, write_control, write_data, ControlMsg, Frame, RELAY_PROTO_VERSION};

/// How the agent reaches and authenticates to its proxy.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// `host:port` of the proxy's relay listener.
    pub proxy_addr: String,
    /// SHA-256 fingerprint of the proxy's relay certificate (pinned).
    pub proxy_fingerprint: String,
    /// This agent's name in the proxy's registry.
    pub agent_name: String,
    /// The token the proxy issued for this agent.
    pub token: String,
}

/// Runs the actual device sessions. Injectable so the relay logic is
/// loopback-testable off Windows; the real implementation drives SQLVDI
/// (vdi.rs, `relay_backup_device`).
pub trait DeviceRunner: Send + Sync + 'static {
    /// Backup direction, blocking (called on a blocking thread): create the
    /// VDI device set `set_name` on `instance` (None = default), signal
    /// `ready` once the set exists (the proxy must not issue BACKUP before a
    /// device set it can attach to exists), then drain the device's writes
    /// into `tx` (use `blocking_send`; a closed channel means the proxy
    /// stopped consuming) and return the byte total when the device closes
    /// cleanly. Poll `cancel` and abort the device set when it trips.
    fn run_backup(
        &self,
        instance: Option<&str>,
        set_name: &str,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
        tx: mpsc::Sender<Vec<u8>>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<u64>;

    /// Restore direction, blocking: create the VDI device set `set_name` on
    /// `instance`, signal `ready`, then fill the device from `rx` (the image
    /// bytes the proxy streams over the data connection; a closed channel is
    /// end of stream) and return the byte total. The proxy issues RESTORE.
    fn run_restore(
        &self,
        instance: Option<&str>,
        set_name: &str,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
        rx: mpsc::Receiver<Vec<u8>>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<u64>;
}

/// The real device runner: SQL Server VDI on the local machine.
#[cfg(windows)]
pub struct VdiDeviceRunner;

#[cfg(windows)]
impl DeviceRunner for VdiDeviceRunner {
    fn run_backup(
        &self,
        instance: Option<&str>,
        set_name: &str,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
        tx: mpsc::Sender<Vec<u8>>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<u64> {
        crate::sql::vdi::relay_backup_device(instance, set_name, ready, tx, cancel)
    }

    fn run_restore(
        &self,
        instance: Option<&str>,
        set_name: &str,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
        rx: mpsc::Receiver<Vec<u8>>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<u64> {
        crate::sql::vdi::relay_restore_device(instance, set_name, ready, rx, cancel)
    }
}

/// Off Windows there is no SQLVDI; an agent configured here can register but
/// every session fails with a clear message (keeps dev builds honest).
#[cfg(not(windows))]
pub struct VdiDeviceRunner;

#[cfg(not(windows))]
impl DeviceRunner for VdiDeviceRunner {
    fn run_backup(
        &self,
        _instance: Option<&str>,
        _set_name: &str,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
        _tx: mpsc::Sender<Vec<u8>>,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<u64> {
        let _ = ready.send(Err(anyhow::anyhow!(
            "SQL VDI device sessions are only available on Windows"
        )));
        anyhow::bail!("SQL VDI device sessions are only available on Windows")
    }

    fn run_restore(
        &self,
        _instance: Option<&str>,
        _set_name: &str,
        ready: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
        _rx: mpsc::Receiver<Vec<u8>>,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<u64> {
        let _ = ready.send(Err(anyhow::anyhow!(
            "SQL VDI device sessions are only available on Windows"
        )));
        anyhow::bail!("SQL VDI device sessions are only available on Windows")
    }
}

/// Delay between reconnect attempts to the proxy.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Buffered data frames between the device thread and the data socket.
const DATA_QUEUE: usize = 16;

/// Run the agent until `shutdown` trips: connect, register, serve commands,
/// and reconnect with a fixed backoff whenever the control connection drops.
pub async fn run_agent(
    cfg: AgentConfig,
    runner: Arc<dyn DeviceRunner>,
    shutdown: CancellationToken,
) {
    loop {
        let attempt = connect_and_serve(&cfg, runner.clone(), &shutdown);
        tokio::select! {
            r = attempt => match r {
                Ok(()) => tracing::info!("relay control connection closed; reconnecting"),
                Err(e) => tracing::warn!("relay connection failed: {e:#}"),
            },
            _ = shutdown.cancelled() => return,
        }
        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            _ = shutdown.cancelled() => return,
        }
    }
}

/// Dial the proxy and TLS-handshake against its pinned certificate.
async fn dial(cfg: &AgentConfig) -> anyhow::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(&cfg.proxy_addr)
        .await
        .with_context(|| format!("connecting to the relay proxy at {}", cfg.proxy_addr))?;
    // The pinned verifier ignores the name; parse the host part so IPs and
    // names both work, with a fixed fallback.
    let host = cfg
        .proxy_addr
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(&cfg.proxy_addr);
    let name = ServerName::try_from(host.to_string())
        .or_else(|_| ServerName::try_from("pbsgui-relay".to_string()))
        .context("building the TLS server name")?;
    let connector = super::tls::connector(&cfg.proxy_fingerprint)?;
    connector
        .connect(name, tcp)
        .await
        .context("TLS handshake with the relay proxy failed (check the fingerprint)")
}

/// One control-connection lifetime: register, then serve commands until the
/// connection drops or `shutdown` trips.
async fn connect_and_serve(
    cfg: &AgentConfig,
    runner: Arc<dyn DeviceRunner>,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    let stream = dial(cfg).await?;
    let (mut rd, mut wr) = tokio::io::split(stream);

    write_control(
        &mut wr,
        &ControlMsg::Register {
            agent_name: cfg.agent_name.clone(),
            host: hostname(),
            token: cfg.token.clone(),
            version: option_env!("PBSGUI_BUILD").unwrap_or("dev").to_string(),
            proto: RELAY_PROTO_VERSION,
        },
    )
    .await?;
    match read_frame(&mut rd).await? {
        Some(Frame::Control(ControlMsg::Registered { ok: true, .. })) => {}
        Some(Frame::Control(ControlMsg::Registered { ok: false, message })) => {
            anyhow::bail!("the proxy refused this agent: {message}")
        }
        other => anyhow::bail!("expected a Registered reply, got {other:?}"),
    }
    tracing::info!(
        "relay agent '{}' registered with {}",
        cfg.agent_name,
        cfg.proxy_addr
    );

    // Control replies (Pong, SessionDone) come from this loop and from session
    // tasks, so writes go through a channel to a single writer task.
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<ControlMsg>(16);
    let writer = tokio::spawn(async move {
        while let Some(msg) = ctl_rx.recv().await {
            if write_control(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });

    // In-flight sessions, for Cancel routing.
    let sessions: Arc<Mutex<HashMap<String, CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let serve = async {
        loop {
            let frame = match read_frame(&mut rd).await? {
                Some(f) => f,
                None => return Ok(()), // proxy closed cleanly
            };
            let msg = match frame {
                Frame::Control(msg) => msg,
                Frame::Data(_) => anyhow::bail!("data frame on the control connection"),
            };
            match msg {
                ControlMsg::Ping => {
                    let _ = ctl_tx.send(ControlMsg::Pong).await;
                }
                ControlMsg::StartBackup {
                    session_id,
                    instance,
                    set_name,
                } => {
                    spawn_session(
                        cfg,
                        &runner,
                        &ctl_tx,
                        &sessions,
                        session_id,
                        Direction::Backup,
                        instance,
                        set_name,
                    );
                }
                ControlMsg::StartRestore {
                    session_id,
                    instance,
                    set_name,
                } => {
                    spawn_session(
                        cfg,
                        &runner,
                        &ctl_tx,
                        &sessions,
                        session_id,
                        Direction::Restore,
                        instance,
                        set_name,
                    );
                }
                ControlMsg::Cancel { session_id } => {
                    if let Some(c) = sessions.lock().unwrap().get(&session_id) {
                        c.cancel();
                    }
                }
                other => anyhow::bail!("unexpected control message from the proxy: {other:?}"),
            }
        }
    };

    let result = tokio::select! {
        r = serve => r,
        _ = shutdown.cancelled() => Ok(()),
    };
    // Cancel any sessions the dead control connection was managing, then stop
    // the writer.
    for (_, c) in sessions.lock().unwrap().drain() {
        c.cancel();
    }
    writer.abort();
    result
}

/// Which way the bytes flow for a device session.
#[derive(Clone, Copy)]
enum Direction {
    Backup,
    Restore,
}

/// Spawn a device session (backup or restore) and report its outcome as
/// `SessionDone`. Registers the session's cancel token for `Cancel` routing and
/// clears it when done.
#[allow(clippy::too_many_arguments)]
fn spawn_session(
    cfg: &AgentConfig,
    runner: &Arc<dyn DeviceRunner>,
    ctl_tx: &mpsc::Sender<ControlMsg>,
    sessions: &Arc<Mutex<HashMap<String, CancellationToken>>>,
    session_id: String,
    direction: Direction,
    instance: Option<String>,
    set_name: String,
) {
    let cancel = CancellationToken::new();
    sessions
        .lock()
        .unwrap()
        .insert(session_id.clone(), cancel.clone());
    let cfg = cfg.clone();
    let runner = runner.clone();
    let ctl_tx = ctl_tx.clone();
    let sessions = sessions.clone();
    tokio::spawn(async move {
        let result = match direction {
            Direction::Backup => {
                run_backup_session(&cfg, runner, &session_id, instance, set_name, cancel).await
            }
            Direction::Restore => {
                run_restore_session(&cfg, runner, &session_id, instance, set_name, cancel).await
            }
        };
        sessions.lock().unwrap().remove(&session_id);
        let (ok, error, bytes) = match result {
            Ok(bytes) => (true, None, bytes),
            Err(e) => (false, Some(format!("{e:#}")), 0),
        };
        let _ = ctl_tx
            .send(ControlMsg::SessionDone {
                session_id,
                ok,
                error,
                bytes,
            })
            .await;
    });
}

/// One backup device session: dial the data connection, bind it with Hello,
/// bridge the device runner's buffers into data frames, and close with the
/// End verdict. Returns the device loop's byte count.
async fn run_backup_session(
    cfg: &AgentConfig,
    runner: Arc<dyn DeviceRunner>,
    session_id: &str,
    instance: Option<String>,
    set_name: String,
    cancel: CancellationToken,
) -> anyhow::Result<u64> {
    // Start the device first and wait for the set to exist: the proxy issues
    // BACKUP as soon as our data connection arrives, and the statement fails
    // if the named device set is not there to attach to. The runner is
    // blocking (a COM thread on Windows); buffers flow through a bounded
    // channel so SQL Server throttles to the network rate.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(DATA_QUEUE);
    let device = tokio::task::spawn_blocking({
        let runner = runner.clone();
        let set_name = set_name.clone();
        let cancel = cancel.clone();
        move || runner.run_backup(instance.as_deref(), &set_name, ready_tx, tx, &cancel)
    });
    ready_rx
        .await
        .map_err(|_| anyhow::anyhow!("the device runner exited before signaling readiness"))?
        .context("creating the VDI device set")?;

    let stream = dial(cfg).await.context("dialing the data connection")?;
    let (mut rd, mut wr) = tokio::io::split(stream);
    write_control(
        &mut wr,
        &ControlMsg::Hello {
            session_id: session_id.to_string(),
            token: cfg.token.clone(),
        },
    )
    .await?;

    let mut send_error: Option<std::io::Error> = None;
    while let Some(buf) = rx.recv().await {
        if let Err(e) = write_data(&mut wr, &buf).await {
            // Stop consuming; the runner sees the closed channel and aborts.
            send_error = Some(e);
            break;
        }
    }
    drop(rx);

    let device_result = device
        .await
        .map_err(|e| anyhow::anyhow!("device thread panicked: {e}"))?;

    // The End verdict tells the proxy whether the stream it received is the
    // complete device output; only a clean device close may say ok.
    let (ok, error) = match (&device_result, &send_error) {
        (Ok(_), None) => (true, None),
        (Ok(_), Some(e)) => (false, Some(format!("sending backup data failed: {e}"))),
        (Err(e), _) => (false, Some(format!("{e:#}"))),
    };
    let _ = write_control(
        &mut wr,
        &ControlMsg::End {
            ok,
            error: error.clone(),
        },
    )
    .await;

    // Orderly teardown: this side never reads during the session, so unread
    // bytes (rustls post-handshake session tickets) sit in the receive buffer,
    // and dropping the socket now would RST it, discarding the in-flight End
    // frame. Shut down our write half and drain until the proxy closes.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _ = wr.shutdown().await;
    let mut sink = [0u8; 1024];
    let drain = async { while matches!(rd.read(&mut sink).await, Ok(n) if n > 0) {} };
    let _ = tokio::time::timeout(Duration::from_secs(10), drain).await;

    if let Some(e) = send_error {
        return Err(anyhow::Error::new(e).context("sending backup data to the proxy"));
    }
    device_result
}

/// One restore device session: create the device, dial the data connection, and
/// feed the image bytes the proxy streams into the device until the proxy's End
/// frame. Returns the device loop's byte count. The proxy issues RESTORE.
async fn run_restore_session(
    cfg: &AgentConfig,
    runner: Arc<dyn DeviceRunner>,
    session_id: &str,
    instance: Option<String>,
    set_name: String,
    cancel: CancellationToken,
) -> anyhow::Result<u64> {
    // Create the device set first (as for backup): the proxy issues RESTORE as
    // soon as the data connection arrives, and it needs the set to attach to.
    // The device pulls image bytes from `dev_rx`, which this task fills from the
    // data connection.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    let (dev_tx, dev_rx) = mpsc::channel::<Vec<u8>>(DATA_QUEUE);
    let device = tokio::task::spawn_blocking({
        let runner = runner.clone();
        let set_name = set_name.clone();
        let cancel = cancel.clone();
        move || runner.run_restore(instance.as_deref(), &set_name, ready_tx, dev_rx, &cancel)
    });
    ready_rx
        .await
        .map_err(|_| anyhow::anyhow!("the device runner exited before signaling readiness"))?
        .context("creating the VDI device set")?;

    let stream = dial(cfg).await.context("dialing the data connection")?;
    let (mut rd, mut wr) = tokio::io::split(stream);
    write_control(
        &mut wr,
        &ControlMsg::Hello {
            session_id: session_id.to_string(),
            token: cfg.token.clone(),
        },
    )
    .await?;

    // Read image bytes from the proxy and feed the device until End (or the
    // connection breaks). A closed `dev_tx` is end of stream to the device.
    let mut stream_ok = false;
    loop {
        match read_frame(&mut rd).await {
            Ok(Some(Frame::Data(bytes))) => {
                if dev_tx.send(bytes).await.is_err() {
                    // The device stopped reading (RESTORE finished or failed).
                    break;
                }
            }
            Ok(Some(Frame::Control(ControlMsg::End { ok, .. }))) => {
                stream_ok = ok;
                break;
            }
            Ok(Some(_)) | Ok(None) | Err(_) => break, // truncated
        }
    }
    drop(dev_tx); // end of stream to the device

    let device_result = device
        .await
        .map_err(|e| anyhow::anyhow!("device thread panicked: {e}"))?;

    // Orderly teardown mirrors the backup path (avoid an RST dropping frames).
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _ = wr.shutdown().await;
    let mut sink = [0u8; 1024];
    let drain = async { while matches!(rd.read(&mut sink).await, Ok(n) if n > 0) {} };
    let _ = tokio::time::timeout(Duration::from_secs(10), drain).await;

    // A device error is the real failure; otherwise a stream that ended without
    // a clean End means the proxy's download broke (RESTORE will have failed).
    let bytes = device_result?;
    if !stream_ok {
        anyhow::bail!("the restore image stream from the proxy did not complete");
    }
    Ok(bytes)
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}
