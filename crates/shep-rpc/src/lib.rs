//! A general-purpose request/response RPC protocol over TCP, after
//! Redpanda's internal RPC: the framing, shared by
//! [`shep-rpc-server`](../shep_rpc_server) and
//! [`shep-rpc-client`](../shep_rpc_client).
//!
//! Each call is one request frame and one response frame on a connection
//! that carries many calls at once, matched by correlation id. A frame is a
//! fixed 32-byte checksummed header ([`wire`]), a section of key/value
//! metadata ([`Metadata`]), and a body of opaque bytes.
//!
//! The transport owns sizes, checksums, compression, correlation ids and a
//! handful of statuses ([`Status`]). Everything else belongs to the service
//! using it: the meaning of the version byte, of method ids, of metadata
//! keys and values, of statuses from [`SERVICE_STATUS_MIN`] up, and the
//! encoding of bodies.
//!
//! Corruption the stream can't recover from (a bad header checksum, metadata
//! over [`MAX_METADATA`], unknown flags, a bad body checksum) closes the
//! connection on either end, failing everything in flight on it. Everything
//! else (malformed metadata under a good checksum, a body over the limit,
//! unsupported compression) fails one call and keeps the connection.
//!
//! Differences from Redpanda: the header is laid out differently to carry
//! metadata and flags; its checksum covers every header byte and the
//! metadata; method ids are explicit `(service, method)` numbers rather than
//! name hashes; and compression is not implemented.

mod connection;
mod metadata;
pub mod wire;

pub use connection::{Connection, Frame, Head, IO_BUFFER};
pub use metadata::{Metadata, MetadataBuilder, MetadataError};
pub use wire::{
    DEFAULT_MAX_PAYLOAD, MAX_METADATA, SERVICE_STATUS_MIN, Status, method_id, service_of,
};
