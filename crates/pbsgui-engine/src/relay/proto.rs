//! Wire protocol between a pbsgui proxy and its SQL-host relay agents.
//!
//! Two connection kinds share one frame format, `u8 kind | u32 len LE | payload`:
//!
//! - The agent's persistent **control connection** to the proxy carries JSON
//!   [`ControlMsg`] frames only: register/keepalive, and the proxy's
//!   start/cancel commands for device sessions.
//! - A **data connection** per device session (dialed by the agent, correlated
//!   by the `Hello` frame's session id) carries the raw VDI byte stream as
//!   `Data` frames, opened by `Hello` and closed by `End`.
//!
//! `End { ok }` mirrors the local `ChannelReader` verdict rule: a data stream
//! that ends any way other than a clean `End { ok: true }` is truncated, and
//! the proxy must not commit the snapshot it was uploading from it. The
//! authoritative BACKUP verdict is still the proxy's own T-SQL statement
//! result; this frame only protects the byte stream.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Protocol revision, sent in `Register`; the proxy refuses agents that do not
/// match. Bump on any incompatible frame or message change.
pub const RELAY_PROTO_VERSION: u32 = 1;

/// The proxy's default relay listener port.
pub const DEFAULT_RELAY_PORT: u16 = 8317;

/// Upper bound on a frame payload. VDI transfer blocks are at most a few MiB
/// (max_transfer_size), so anything larger is a corrupt or hostile stream.
pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

const KIND_CONTROL: u8 = 0;
const KIND_DATA: u8 = 1;

/// One frame on a relay connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Control(ControlMsg),
    /// A slice of the raw VDI byte stream (data connections only).
    Data(Vec<u8>),
}

/// Control messages. One enum for both directions and both connection kinds:
/// the receiving side rejects messages that make no sense for its role, and a
/// single namespace keeps the codec and its tests simple.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    // --- agent -> proxy, control connection ---
    /// First message after connecting: authenticate and describe the agent.
    Register {
        agent_name: String,
        host: String,
        token: String,
        version: String,
        proto: u32,
    },
    /// A device session finished on the agent (its device-loop outcome; the
    /// byte count lets the proxy cross-check the stream it received).
    SessionDone {
        session_id: String,
        ok: bool,
        error: Option<String>,
        bytes: u64,
    },
    Pong,

    // --- proxy -> agent, control connection ---
    /// The proxy's verdict on `Register`.
    Registered {
        ok: bool,
        message: String,
    },
    /// Create a VDI device set named `set_name` on `instance` (None = default
    /// instance), drain its writes, and stream them over a new data connection
    /// for `session_id`. The proxy issues the BACKUP statement itself once the
    /// agent's data connection arrives.
    StartBackup {
        session_id: String,
        instance: Option<String>,
        set_name: String,
    },
    /// As `StartBackup`, but the agent serves the device's reads from the data
    /// connection (the proxy streams the backup image and issues RESTORE).
    StartRestore {
        session_id: String,
        instance: Option<String>,
        set_name: String,
    },
    /// Abort a session's device set (`SignalAbort`), failing the statement.
    Cancel {
        session_id: String,
    },
    Ping,

    // --- data connection, either direction ---
    /// First frame on a data connection: bind it to a session.
    Hello {
        session_id: String,
        token: String,
    },
    /// Last frame of a data stream. Anything short of `ok: true` marks the
    /// stream truncated.
    End {
        ok: bool,
        error: Option<String>,
    },
}

/// Write one frame. Production code writes with the typed helpers below; this
/// generic form round-trips whole `Frame` values in the codec tests.
#[cfg_attr(not(test), allow(dead_code))]
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> std::io::Result<()> {
    let (kind, payload) = match frame {
        Frame::Control(msg) => (KIND_CONTROL, serde_json::to_vec(msg)?),
        Frame::Data(bytes) => (KIND_DATA, bytes.clone()),
    };
    write_raw(w, kind, &payload).await
}

/// Write a data frame without copying the payload into a `Frame` first (the
/// device loop calls this once per VDI transfer block).
pub async fn write_data<W: AsyncWrite + Unpin>(w: &mut W, payload: &[u8]) -> std::io::Result<()> {
    write_raw(w, KIND_DATA, payload).await
}

/// Write a control frame.
pub async fn write_control<W: AsyncWrite + Unpin>(
    w: &mut W,
    msg: &ControlMsg,
) -> std::io::Result<()> {
    write_raw(w, KIND_CONTROL, &serde_json::to_vec(msg)?).await
}

async fn write_raw<W: AsyncWrite + Unpin>(
    w: &mut W,
    kind: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|l| *l <= MAX_FRAME)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "relay frame too large")
        })?;
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..5].copy_from_slice(&len.to_le_bytes());
    w.write_all(&header).await?;
    w.write_all(payload).await?;
    w.flush().await
}

/// Read one frame, or `None` at a clean end of stream (the peer closed between
/// frames). A close mid-frame is an error, as is an oversized or unknown frame.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Frame>> {
    let mut header = [0u8; 5];
    // Distinguish "closed between frames" from "closed mid-header".
    match r.read(&mut header[..1]).await? {
        0 => return Ok(None),
        _ => r.read_exact(&mut header[1..]).await.map(|_| ())?,
    }
    let kind = header[0];
    let len = u32::from_le_bytes(header[1..5].try_into().unwrap());
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("relay frame of {len} bytes exceeds the {MAX_FRAME} limit"),
        ));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    match kind {
        KIND_CONTROL => {
            let msg = serde_json::from_slice(&payload).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("bad relay control frame: {e}"),
                )
            })?;
            Ok(Some(Frame::Control(msg)))
        }
        KIND_DATA => Ok(Some(Frame::Data(payload))),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unknown relay frame kind {other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn round_trip(frames: Vec<Frame>) -> Vec<Frame> {
        let (mut a, mut b) = tokio::io::duplex(1024 * 1024);
        let writer = async {
            for f in &frames {
                write_frame(&mut a, f).await.unwrap();
            }
            drop(a); // clean close between frames
        };
        let reader = async {
            let mut out = Vec::new();
            while let Some(f) = read_frame(&mut b).await.unwrap() {
                out.push(f);
            }
            out
        };
        let ((), out) = tokio::join!(writer, reader);
        out
    }

    #[tokio::test]
    async fn frames_round_trip_and_stream_ends_cleanly() {
        let frames = vec![
            Frame::Control(ControlMsg::Register {
                agent_name: "stanley-agent".into(),
                host: "STANLEY".into(),
                token: "t0k3n".into(),
                version: "0.2.0".into(),
                proto: RELAY_PROTO_VERSION,
            }),
            Frame::Data(vec![0u8; 65536]),
            Frame::Data(Vec::new()), // empty data frames are legal
            Frame::Control(ControlMsg::End {
                ok: true,
                error: None,
            }),
        ];
        assert_eq!(round_trip(frames.clone()).await, frames);
    }

    #[tokio::test]
    async fn oversized_and_unknown_frames_are_rejected() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // write_raw refuses to emit an oversized frame at all.
        let big = vec![0u8; (MAX_FRAME + 1) as usize];
        let err = write_data(&mut a, &big).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

        // A forged oversized header is rejected before any allocation.
        let mut header = vec![KIND_DATA];
        header.extend_from_slice(&(MAX_FRAME + 1).to_le_bytes());
        tokio::io::AsyncWriteExt::write_all(&mut a, &header)
            .await
            .unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        // An unknown frame kind is rejected too.
        let (mut a, mut b) = tokio::io::duplex(64);
        tokio::io::AsyncWriteExt::write_all(&mut a, &[9u8, 0, 0, 0, 0])
            .await
            .unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_close_mid_frame_is_an_error_not_eof() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // A header promising 100 bytes, then the peer vanishes.
        let mut header = vec![KIND_DATA];
        header.extend_from_slice(&100u32.to_le_bytes());
        tokio::io::AsyncWriteExt::write_all(&mut a, &header)
            .await
            .unwrap();
        drop(a);
        assert!(read_frame(&mut b).await.is_err());
    }

    #[test]
    fn control_messages_serialize_tagged() {
        // The tag format is part of the wire contract (agents and proxies of
        // the same proto version must agree even across engine builds).
        let json = serde_json::to_string(&ControlMsg::StartBackup {
            session_id: "s1".into(),
            instance: None,
            set_name: "pbsgui-abc".into(),
        })
        .unwrap();
        assert!(json.contains(r#""type":"start_backup""#), "{json}");
        let back: ControlMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ControlMsg::StartBackup { .. }));
    }
}
