//! SQL backup relay: a thin agent on the SQL host runs VDI device sessions and
//! streams the raw backup bytes to a pbsgui proxy, which carries the CPU-heavy
//! work (chunking, hashing, compression, encryption, PBS upload) instead of the
//! database server. Design: research/notes/12-relay-design.md.

pub mod agent;
pub mod backup;
pub mod proto;
pub mod server;
pub mod setup;
pub mod tls;
