//! The frame header, transport statuses, and method ids.
//!
//! Every request and response is one frame: a fixed 32-byte little-endian
//! header, then `metadata_size` bytes of metadata (see
//! [`Metadata`](crate::Metadata)), then `payload_size` bytes of body.
//!
//! ```text
//! byte  0        1        2        3        4        5        6        7
//!      +--------+--------+--------+--------+--------+--------+--------+--------+
//!    0 |version |compress|  metadata_size  |          header_checksum          |
//!      +--------+--------+--------+--------+--------+--------+--------+--------+
//!    8 |           payload_size            |         method_or_status          |
//!      +--------+--------+--------+--------+--------+--------+--------+--------+
//!   16 |          correlation_id           |               flags               |
//!      +--------+--------+--------+--------+--------+--------+--------+--------+
//!   24 |                           payload_checksum                            |
//!      +--------+--------+--------+--------+--------+--------+--------+--------+
//! ```
//!
//! The transport owns sizes, checksums, compression, correlation ids and
//! flags. `version` and the method id belong to the service and pass through
//! untouched; so do statuses from [`SERVICE_STATUS_MIN`] up. The layout is
//! fixed: the framing has no version of its own, and changes to it can only
//! come through `flags`, which receivers require to be zero today.

use xxhash_rust::xxh64::xxh64;

/// Bytes in an encoded [`Header`].
pub const HEADER_SIZE: usize = 32;

/// The most metadata a frame may carry. A receiver closes the connection on
/// a header claiming more.
pub const MAX_METADATA: usize = 4096;

/// The default limit on a body, in either direction. Servers and clients
/// can set their own.
pub const DEFAULT_MAX_PAYLOAD: usize = 32 << 20;

/// No compression.
pub const COMPRESSION_NONE: u8 = 0;

/// zstd. Assigned but not implemented: a server answers
/// [`Status::UnsupportedCompression`], and a client fails the call.
pub const COMPRESSION_ZSTD: u8 = 1;

/// The lowest status a service may use. Statuses below it are the
/// transport's.
pub const SERVICE_STATUS_MIN: u32 = 1000;

/// The method id for `method` of `service`: `(service << 16) | method`.
///
/// A convention, not a rule: the transport carries any `u32`. Splitting it
/// lets one listener host several services, each numbering its own methods.
/// Service numbers need only be unique among the services sharing a
/// listener.
pub const fn method_id(service: u16, method: u16) -> u32 {
    ((service as u32) << 16) | method as u32
}

/// The service number of a method id built with [`method_id`].
pub const fn service_of(method: u32) -> u16 {
    (method >> 16) as u16
}

/// The statuses the transport itself defines. HTTP codes, for readability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Status {
    /// The service handled the request; the body is its response.
    Success = 200,
    /// The request's metadata was malformed.
    BadRequest = 400,
    /// The server gave up on the request without running it to completion,
    /// typically because it is shutting down.
    Abandoned = 408,
    /// The request's body was larger than the server accepts.
    PayloadTooLarge = 413,
    /// The request was compressed in a way the server doesn't support.
    UnsupportedCompression = 415,
    /// The handler panicked, or its response was too large to send.
    ServerError = 500,
}

impl Status {
    pub fn from_u32(code: u32) -> Option<Status> {
        Some(match code {
            200 => Status::Success,
            400 => Status::BadRequest,
            408 => Status::Abandoned,
            413 => Status::PayloadTooLarge,
            415 => Status::UnsupportedCompression,
            500 => Status::ServerError,
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
    /// The service's protocol version.
    pub version: u8,
    pub compression: u8,
    pub metadata_size: u16,
    /// CRC32C over the encoded header with this field zeroed, followed by the
    /// metadata section.
    pub header_checksum: u32,
    /// Body bytes as sent.
    pub payload_size: u32,
    /// The method id on a request, the status on a response.
    pub method_or_status: u32,
    /// Chosen by the client; the response echoes it so the client can match
    /// it to the waiting call.
    pub correlation_id: u32,
    pub flags: u32,
    /// XXH64 (seed 0) over the body as sent.
    pub payload_checksum: u64,
}

impl Header {
    /// A complete header for an uncompressed frame with `metadata` and a body
    /// of `payload_size` bytes hashing to `payload_checksum`, its header
    /// checksum filled in.
    ///
    /// # Panics
    ///
    /// If `metadata` is longer than [`MAX_METADATA`] or `payload_size` doesn't
    /// fit in a `u32`.
    pub fn new(
        version: u8,
        method_or_status: u32,
        correlation_id: u32,
        metadata: &[u8],
        payload_size: usize,
        payload_checksum: u64,
    ) -> Header {
        assert!(metadata.len() <= MAX_METADATA, "metadata too large");
        let mut header = Header {
            version,
            compression: COMPRESSION_NONE,
            metadata_size: metadata.len() as u16,
            header_checksum: 0,
            payload_size: u32::try_from(payload_size).expect("body larger than u32::MAX"),
            method_or_status,
            correlation_id,
            flags: 0,
            payload_checksum,
        };
        header.header_checksum = header.compute_checksum(metadata);
        header
    }

    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut out = [0; HEADER_SIZE];
        out[0] = self.version;
        out[1] = self.compression;
        out[2..4].copy_from_slice(&self.metadata_size.to_le_bytes());
        out[4..8].copy_from_slice(&self.header_checksum.to_le_bytes());
        out[8..12].copy_from_slice(&self.payload_size.to_le_bytes());
        out[12..16].copy_from_slice(&self.method_or_status.to_le_bytes());
        out[16..20].copy_from_slice(&self.correlation_id.to_le_bytes());
        out[20..24].copy_from_slice(&self.flags.to_le_bytes());
        out[24..32].copy_from_slice(&self.payload_checksum.to_le_bytes());
        out
    }

    /// Reads the fields as they are. Check [`Header::checksum_ok`] before
    /// trusting any of them.
    pub fn decode(bytes: &[u8; HEADER_SIZE]) -> Header {
        let u16_at = |i: usize| u16::from_le_bytes(bytes[i..i + 2].try_into().unwrap());
        let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        Header {
            version: bytes[0],
            compression: bytes[1],
            metadata_size: u16_at(2),
            header_checksum: u32_at(4),
            payload_size: u32_at(8),
            method_or_status: u32_at(12),
            correlation_id: u32_at(16),
            flags: u32_at(20),
            payload_checksum: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        }
    }

    /// What `header_checksum` should be for the other fields and `metadata`,
    /// the section that follows the header.
    pub fn compute_checksum(&self, metadata: &[u8]) -> u32 {
        let mut bytes = self.encode();
        bytes[4..8].fill(0);
        crc32c::crc32c_append(crc32c::crc32c(&bytes), metadata)
    }

    pub fn checksum_ok(&self, metadata: &[u8]) -> bool {
        self.header_checksum == self.compute_checksum(metadata)
    }
}

/// XXH64, seed 0, of a body as it is on the wire.
pub fn payload_checksum(body: &[u8]) -> u64 {
    xxh64(body, 0)
}
