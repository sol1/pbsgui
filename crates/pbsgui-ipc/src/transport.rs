//! Local-socket transport for the GUI <-> engine IPC.
//!
//! Uses a named pipe on Windows and a Unix domain socket elsewhere (via the
//! `interprocess` crate), so the same code runs on the target and is testable on
//! Linux. The wire format is newline-delimited JSON: the client sends one
//! [`Request`] line, the server replies with a stream of [`Reply`] lines and
//! closes.

use std::future::Future;
use std::io;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ListenerOptions, Name};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Default socket base name (the engine and GUI must agree on it).
///
/// The `-v<N>` suffix is the IPC protocol version: bump it on any incompatible
/// change to [`crate::protocol`]. A new GUI then uses a new pipe name, so it can
/// never silently connect to a leftover engine speaking the old protocol.
pub const DEFAULT_SOCKET: &str = "pbsgui-engine-v4";

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

/// Sends [`Reply`] messages back to the client over one connection.
pub struct Responder {
    write: Box<dyn AsyncWrite + Send + Unpin>,
}

impl Responder {
    /// Send one reply.
    pub async fn send(&mut self, reply: &crate::Reply) -> io::Result<()> {
        let mut line =
            serde_json::to_vec(reply).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push(b'\n');
        self.write.write_all(&line).await?;
        self.write.flush().await
    }
}

/// On Windows, give the pipe a DACL allowing SYSTEM, Administrators, and
/// authenticated users to connect, so the unprivileged GUI can reach the engine
/// even when it runs as a LocalSystem service. Falls back to the default on error.
#[cfg(windows)]
fn with_pipe_security(options: ListenerOptions<'_>) -> ListenerOptions<'_> {
    use interprocess::os::windows::local_socket::ListenerOptionsExt;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::U16CString;

    // SYSTEM, Builtin Administrators, and Authenticated Users: full access.
    const SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;AU)";
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

/// Listen on `name` and dispatch each connection's request to `handler`.
///
/// Each connection carries exactly one request; the handler streams replies via
/// the [`Responder`] and returns when done. Runs until the listener errors.
pub async fn serve<H, Fut>(name: Name<'static>, handler: H) -> io::Result<()>
where
    H: Fn(crate::Request, Responder) -> Fut + Clone + Send + Sync + 'static,
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

async fn handle_connection<H, Fut>(conn: Stream, handler: H) -> io::Result<()>
where
    H: Fn(crate::Request, Responder) -> Fut,
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
    };
    match serde_json::from_str::<crate::Request>(line.trim()) {
        Ok(request) => handler(request, responder).await,
        Err(e) => {
            let _ = responder
                .send(&crate::Reply::Error {
                    message: format!("invalid request: {e}"),
                })
                .await;
        }
    }
    Ok(())
}

/// Connect to the engine, send one request, and pass each reply to `on_reply`.
///
/// Returns when the engine closes the connection (after a terminal reply).
pub async fn send_request<F>(
    name: Name<'static>,
    request: &crate::Request,
    mut on_reply: F,
) -> io::Result<()>
where
    F: FnMut(crate::Reply),
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
        match serde_json::from_str::<crate::Reply>(buf.trim()) {
            Ok(reply) => on_reply(reply),
            Err(e) => on_reply(crate::Reply::Error {
                message: format!("invalid reply: {e}"),
            }),
        }
    }
    Ok(())
}
