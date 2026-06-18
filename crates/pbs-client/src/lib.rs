//! Clean-room Rust client for the Proxmox Backup Server (PBS) backup protocol.
//!
//! This crate talks to a PBS datastore over its documented HTTP API and backup
//! protocol. It is a clean-room implementation written from the protocol
//! documentation: it does not link or copy the AGPL Proxmox client code, which
//! keeps pbsgui free to ship under its own license.
//!
//! Why reimplement instead of embedding the official client:
//!   - the official Rust client crates are Unix bound (nix / proxmox-sys) and do
//!     not build for Windows;
//!   - the pxar archive format hard-encodes the Unix file model and cannot
//!     represent Windows ACLs / alternate data streams faithfully.
//!
//! Protocol summary (to be implemented in `session`):
//!   - Authenticate against the PBS API and pin the server by its TLS
//!     certificate SHA-256 fingerprint.
//!   - Open a backup session: `GET /api2/json/backup` with
//!     `UPGRADE: proxmox-backup-protocol-v1`, switching the connection to HTTP/2.
//!   - Upload data as one of two index types:
//!       * fixed index  (.fidx) for image / block streams split into equal sized
//!         chunks (used here for SQL VDI byte streams and raw volume images);
//!       * dynamic index (.didx) for file archive streams split into variable
//!         sized chunks by a Buzhash content-defined boundary.
//!   - Store small objects (manifests, our SQL metadata) as blobs (.blob).
//!   - Finalise the snapshot with `POST /finish`.
//!   - Read back via the reader protocol (`proxmox-backup-reader-protocol-v1`).
//!
//! Client side encryption (optional, a differentiator over existing Windows
//! clients) uses AES-256-GCM with a key derived for the datastore.

pub mod blob;
pub mod error;
pub mod index;
pub mod manifest;
pub mod repository;
pub mod session;

// Implemented incrementally as the protocol client comes online:
//
// pub mod chunker;   // Buzhash content-defined chunking for dynamic indexes
// pub mod crypto;    // AES-256-GCM client side encryption and key handling
// pub mod api_types; // serde types mirrored from the documented PBS API

pub use error::{PbsError, Result};
pub use index::{FixedIndex, FixedIndexBuilder};
pub use manifest::BackupManifest;
pub use repository::Repository;
pub use session::{BackupWriter, ReaderClient, SessionParams};
