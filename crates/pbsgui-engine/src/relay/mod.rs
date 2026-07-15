//! SQL backup relay: a thin agent on the SQL host runs VDI device sessions and
//! streams the raw backup bytes to a pbsgui proxy, which carries the CPU-heavy
//! work (chunking, hashing, compression, encryption, PBS upload) instead of the
//! database server. Design: research/notes/12-relay-design.md.
//!
//! Scaffolding stage: only the wire protocol exists so far. The agent and
//! server tasks land next and will consume everything here; the allow goes
//! away with them.
#![allow(dead_code)]

pub mod agent;
pub mod proto;
pub mod server;
pub mod tls;
