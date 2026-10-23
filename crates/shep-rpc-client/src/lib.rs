//! Calling [`shep-rpc`](shep_rpc) servers.
//!
//! A [`Transport`] pipelines many calls over one TCP connection, matching
//! responses to calls by correlation id. A [`ReconnectTransport`] replaces a
//! failed connection, backing off between attempts, and a
//! [`ConnectionCache`] keeps one per peer.
//!
//! The client is single-threaded: a connection's reader and writer are tasks
//! spawned with `spawn_local`, so it runs on a
//! [`LocalRuntime`](tokio::runtime::LocalRuntime), and neither it nor its
//! calls' futures are `Send`. Each core keeps its own connections.

mod error;
mod reconnect;
mod transport;

pub use error::RpcError;
pub use reconnect::{ConnectionCache, ReconnectConfig, ReconnectTransport};
pub use transport::{Call, Reply, Transport, TransportConfig};
