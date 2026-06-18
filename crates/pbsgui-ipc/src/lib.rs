//! Shared IPC protocol and transport for pbsgui.
//!
//! The unprivileged GUI talks to the backup engine over a local socket (a named
//! pipe on Windows). This crate defines the message types ([`Request`],
//! [`Reply`]) and the transport ([`serve`], [`send_request`]). It depends only on
//! serde and the socket library, so the GUI does not pull in the engine's backup
//! dependencies.

pub mod protocol;
pub mod transport;

pub use protocol::{Job, PbsDestination, Reply, Request, Schedule};
pub use transport::{send_request, serve, socket_name, Responder, DEFAULT_SOCKET};
