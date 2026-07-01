//! Local-socket transport for the GUI <-> engine IPC.
//!
//! Uses a named pipe on Windows and a Unix domain socket elsewhere (via the
//! `interprocess` crate), so the same code runs on the target and is testable on
//! Linux. The wire format is newline-delimited JSON: the client sends one request
//! line, the server replies with a stream of reply lines and closes.
//!
//! The transport is generic over the request and reply message types
//! ([`serve_typed`] / [`send_request_typed`]), so each engine can define its own
//! protocol; [`serve`] / [`send_request`] are thin wrappers bound to the SQL/files
//! engine's [`crate::Request`] / [`crate::Reply`].

use std::future::Future;
use std::io;
use std::marker::PhantomData;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ListenerOptions, Name};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Default socket base name (the engine and GUI must agree on it).
///
/// The `-v<N>` suffix is the IPC protocol version: bump it on any incompatible
/// change to [`crate::protocol`]. A new GUI then uses a new pipe name, so it can
/// never silently connect to a leftover engine speaking the old protocol.
pub const DEFAULT_SOCKET: &str = "pbsgui-engine-v9";

/// A reply type that can carry a transport-level error (an unparseable request on
/// the server, or an unparseable reply on the client), so the generic transport
/// can surface those without knowing the concrete protocol.
pub trait ErrorReply {
    fn error(message: String) -> Self;
}

impl ErrorReply for crate::Reply {
    fn error(message: String) -> Self {
        crate::Reply::Error { message }
    }
}

/// Build a platform-appropriate socket name from a base string.
///
/// On Windows this is a named pipe (`\\.\pipe\<base>`); elsewhere a socket file
/// in the temp dir.
pub fn socket_name(base: &str) -> io::Result<Name<'static>> {
    if GenericNamespaced::is_supported() {
        format!("{base}.sock").to_ns_name::<GenericNamespaced>()
    } else {
        std::env::temp_dir()
            .join(format!("{base}.sock"))
            .to_fs_name::<GenericFilePath>()
    }
}

/// Sends reply messages back to the client over one connection. The reply type
/// defaults to the SQL/files engine's [`crate::Reply`].
pub struct Responder<Rep = crate::Reply> {
    write: Box<dyn AsyncWrite + Send + Unpin>,
    _marker: PhantomData<fn() -> Rep>,
}

impl<Rep: Serialize> Responder<Rep> {
    /// Send one reply.
    pub async fn send(&mut self, reply: &Rep) -> io::Result<()> {
        let mut line =
            serde_json::to_vec(reply).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push(b'\n');
        self.write.write_all(&line).await?;
        self.write.flush().await
    }
}

/// On Windows, give the pipe a DACL allowing only SYSTEM and Builtin
/// Administrators to connect, so the engine (which runs as LocalSystem and does
/// privileged work) can only be driven by an administrator. Falls back to the
/// default on error.
///
/// This requires the GUI to run *elevated*: under UAC a normally launched process
/// has a filtered token with the Administrators group deny-only, which would fail
/// this check. The release GUI therefore ships a `requireAdministrator` manifest
/// (see `src-tauri/build.rs`) so it is always elevated and can connect.
#[cfg(windows)]
fn with_pipe_security(options: ListenerOptions<'_>) -> ListenerOptions<'_> {
    use interprocess::os::windows::local_socket::ListenerOptionsExt;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::U16CString;

    // SYSTEM and Builtin Administrators only (the GUI runs elevated to match).
    const SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)";
    let wide = match U16CString::from_str(SDDL) {
        Ok(wide) => wide,
        Err(_) => return options,
    };
    match SecurityDescriptor::deserialize(&wide) {
        Ok(sd) => options.security_descriptor(sd),
        Err(e) => {
            tracing::warn!("could not set pipe security descriptor: {e}");
            options
        }
    }
}

/// Listen on `name` and dispatch each connection's request to `handler`, generic
/// over the request and reply types.
///
/// Each connection carries exactly one request; the handler streams replies via
/// the [`Responder`] and returns when done. Runs until the listener errors.
pub async fn serve_typed<Req, Rep, H, Fut>(name: Name<'static>, handler: H) -> io::Result<()>
where
    Req: DeserializeOwned + Send + 'static,
    Rep: ErrorReply + Serialize + Send + Sync + 'static,
    H: Fn(Req, Responder<Rep>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let options = ListenerOptions::new().name(name);
    #[cfg(windows)]
    let options = with_pipe_security(options);
    let listener = options.create_tokio()?;
    loop {
        let conn = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                continue;
            }
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(conn, handler).await {
                tracing::warn!("connection error: {e}");
            }
        });
    }
}

/// Listen and dispatch using the SQL/files engine's [`crate::Request`] /
/// [`crate::Reply`] protocol.
pub async fn serve<H, Fut>(name: Name<'static>, handler: H) -> io::Result<()>
where
    H: Fn(crate::Request, Responder) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    serve_typed::<crate::Request, crate::Reply, H, Fut>(name, handler).await
}

async fn handle_connection<Req, Rep, H, Fut>(conn: Stream, handler: H) -> io::Result<()>
where
    Req: DeserializeOwned,
    Rep: ErrorReply + Serialize,
    H: Fn(Req, Responder<Rep>) -> Fut,
    Fut: Future<Output = ()>,
{
    let (read_half, write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }

    let mut responder = Responder {
        write: Box::new(write_half),
        _marker: PhantomData,
    };
    match serde_json::from_str::<Req>(line.trim()) {
        Ok(request) => handler(request, responder).await,
        Err(e) => {
            let _ = responder
                .send(&Rep::error(format!("invalid request: {e}")))
                .await;
        }
    }
    Ok(())
}

/// Connect, send one request, and pass each reply to `on_reply`, generic over the
/// request and reply types.
///
/// Returns when the server closes the connection (after a terminal reply).
pub async fn send_request_typed<Req, Rep, F>(
    name: Name<'static>,
    request: &Req,
    mut on_reply: F,
) -> io::Result<()>
where
    Req: Serialize,
    Rep: ErrorReply + DeserializeOwned,
    F: FnMut(Rep),
{
    let conn = Stream::connect(name).await?;
    let (read_half, mut write_half) = tokio::io::split(conn);

    let mut line =
        serde_json::to_vec(request).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    write_half.write_all(&line).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut buf = String::new();
    loop {
        buf.clear();
        if reader.read_line(&mut buf).await? == 0 {
            break;
        }
        match serde_json::from_str::<Rep>(buf.trim()) {
            Ok(reply) => on_reply(reply),
            Err(e) => on_reply(Rep::error(format!("invalid reply: {e}"))),
        }
    }
    Ok(())
}

/// Send one request using the SQL/files engine's [`crate::Request`] /
/// [`crate::Reply`] protocol.
pub async fn send_request<F>(
    name: Name<'static>,
    request: &crate::Request,
    on_reply: F,
) -> io::Result<()>
where
    F: FnMut(crate::Reply),
{
    send_request_typed::<crate::Request, crate::Reply, F>(name, request, on_reply).await
}
