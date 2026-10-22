//! The frame header, status codes, and method ids.
//!
//! Every request and response is one frame: a fixed 26-byte little-endian
//! header followed by `payload_size` bytes of body. The layout is Redpanda's
//! `rpc::header`, field for field:
//!
//! | offset | size | field              |
//! |--------|------|--------------------|
//! | 0      | 1    | `version`          |
//! | 1      | 4    | `header_checksum`  |
//! | 5      | 1    | `compression`      |
//! | 6      | 4    | `payload_size`     |
//! | 10     | 4    | `meta`             |
//! | 14     | 4    | `correlation_id`   |
//! | 18     | 8    | `payload_checksum` |
//!
//! `meta` is the method id on a request and the [`Status`] on a response.

use xxhash_rust::xxh64::xxh64;

/// The only protocol version this crate speaks.
pub const VERSION: u8 = 1;

/// Bytes in an encoded [`Header`].
pub const HEADER_SIZE: usize = 26;

/// The largest body a frame may carry, in either direction: room for the
/// largest Raft entry with headroom. Senders refuse bigger bodies, and a
/// reader that sees a header claiming one closes the connection without
/// allocating for it.
pub const MAX_PAYLOAD: usize = 32 << 20;

/// No compression. The only body encoding implemented.
pub const COMPRESSION_NONE: u8 = 0;

/// Reserved for zstd, as in Redpanda. Not implemented: a frame carrying it
/// closes the connection.
pub const COMPRESSION_ZSTD: u8 = 1;

/// The method id for `method` of `service`: `(service << 16) | method`.
///
/// Services number themselves and their methods explicitly rather than
/// hashing names (Redpanda uses a crc of the service and method name). An
/// explicit number cannot change when something is renamed, and two methods
/// can only collide if someone writes the same pair twice.
pub const fn method_id(service: u16, method: u16) -> u32 {
    ((service as u32) << 16) | method as u32
}

/// A response's status, carried in `meta`. HTTP codes, for readability, as
/// in Redpanda.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Status {
    Success = 200,
    /// The server has no handler for the method id.
    MethodNotFound = 404,
    /// The server gave up on the request without running it to completion,
    /// typically because it is shutting down.
    RequestTimeout = 408,
    /// The handler failed.
    ServerError = 500,
    /// The server is up but not yet able to serve this method; retry.
    ServiceUnavailable = 503,
    /// The request's `version` is not one the server speaks.
    VersionNotSupported = 505,
}

impl Status {
    pub fn from_u32(code: u32) -> Option<Status> {
        Some(match code {
            200 => Status::Success,
            404 => Status::MethodNotFound,
            408 => Status::RequestTimeout,
            500 => Status::ServerError,
            503 => Status::ServiceUnavailable,
            505 => Status::VersionNotSupported,
            _ => return None,
        })
    }

    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

/// A frame header, decoded but not yet validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    /// crc32c over `version` and bytes 5..26 of the encoded header.
    pub header_checksum: u32,
    pub compression: u8,
    pub payload_size: u32,
    /// The method id on a request, the status on a response.
    pub meta: u32,
    /// Chosen by the client; the response echoes it so the client can match
    /// it to the waiting call.
    pub correlation_id: u32,
    /// XXH64 (seed 0) over the body as sent.
    pub payload_checksum: u64,
}

impl Header {
    /// A complete header for an uncompressed `body`, both checksums filled
    /// in. `body` must fit in a `u32`.
    pub fn new(meta: u32, correlation_id: u32, body: &[u8]) -> Header {
        Header::with_payload_checksum(meta, correlation_id, body.len(), payload_checksum(body))
    }

    /// Like `new`, with the body's checksum already computed, so callers can
    /// hash outside a lock.
    pub(crate) fn with_payload_checksum(
        meta: u32,
        correlation_id: u32,
        len: usize,
        payload_checksum: u64,
    ) -> Header {
        let mut header = Header {
            version: VERSION,
            header_checksum: 0,
            compression: COMPRESSION_NONE,
            payload_size: u32::try_from(len).expect("body larger than u32::MAX"),
            meta,
            correlation_id,
            payload_checksum,
        };
        header.header_checksum = header.compute_checksum();
        header
    }

    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0; HEADER_SIZE];
        out[0] = self.version;
        out[1..5].copy_from_slice(&self.header_checksum.to_le_bytes());
        out[5] = self.compression;
        out[6..10].copy_from_slice(&self.payload_size.to_le_bytes());
        out[10..14].copy_from_slice(&self.meta.to_le_bytes());
        out[14..18].copy_from_slice(&self.correlation_id.to_le_bytes());
        out[18..26].copy_from_slice(&self.payload_checksum.to_le_bytes());
        out
    }

    /// Reads the fields as they are. Check [`Header::checksum_ok`] before
    /// trusting any of them.
    pub fn decode(bytes: &[u8; HEADER_SIZE]) -> Header {
        let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        Header {
            version: bytes[0],
            header_checksum: u32_at(1),
            compression: bytes[5],
            payload_size: u32_at(6),
            meta: u32_at(10),
            correlation_id: u32_at(14),
            payload_checksum: u64::from_le_bytes(bytes[18..26].try_into().unwrap()),
        }
    }

    /// What `header_checksum` should be for the other fields.
    ///
    /// Unlike Redpanda, the version byte is covered too: a flipped version bit
    /// would otherwise go unnoticed and be answered with
    /// `VersionNotSupported` instead of closing a connection whose stream can
    /// no longer be trusted.
    pub fn compute_checksum(&self) -> u32 {
        let bytes = self.encode();
        crc32c::crc32c_append(crc32c::crc32c(&bytes[..1]), &bytes[5..])
    }

    pub fn checksum_ok(&self) -> bool {
        self.header_checksum == self.compute_checksum()
    }
}

/// XXH64, seed 0, of a body as it is on the wire.
pub fn payload_checksum(body: &[u8]) -> u64 {
    xxh64(body, 0)
}
