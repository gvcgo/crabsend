//! LocalSend v2 protocol: discovery, file-transfer server and client.
//!
//! The crate is transport-complete on its own — it has no dependency on Tauri,
//! so the wire behaviour can be exercised directly from tests and tools.

pub mod client;
pub mod crypto;
pub mod discovery;
pub mod fs_util;
pub mod model;
pub mod server;
pub mod tls;
