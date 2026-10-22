//! Request/response RPC between Shep nodes, after Redpanda's internal RPC.
//!
//! Each frame is a 26-byte checksummed header (see [`wire`]) and a body of
//! opaque bytes; encoding bodies is the caller's business. A [`Transport`]
//! pipelines many calls over one TCP connection, matching responses to calls
//! by correlation id. A [`Server`] reads requests off a connection in order,
//! runs their handlers concurrently, and writes each response as its handler
//! finishes.
//!
//! Corruption is not survivable: a header or body that fails its checksum
//! closes the connection on either end, failing everything in flight on it.
//!
//! Differences from Redpanda: the header checksum also covers the version
//! byte, method ids are explicit `(service, method)` numbers rather than name
//! hashes, and compression is not implemented.

mod error;
mod frame;
mod reconnect;
mod server;
mod transport;
pub mod wire;

pub use error::RpcError;
pub use reconnect::{ConnectionCache, ReconnectConfig, ReconnectTransport};
pub use server::{Request, Response, Server, ServerConfig};
pub use transport::Transport;
pub use wire::{Status, method_id};
