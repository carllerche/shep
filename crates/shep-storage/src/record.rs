//! Records and their on-disk framing.
//!
//! ```text
//! header_crc: u32    crc32c of the remaining 25 header bytes
//! payload_size: u32
//! index: u64
//! term: u64
//! kind: u8
//! payload_crc: u32   crc32c of the payload
//! payload: [u8; payload_size]
//! ```
//!
//! All little-endian. Records are concatenated; an all-zero header marks the
//! end of the log, which is what preallocated space reads as.

use bytes::Bytes;

/// Bytes of framing before each payload.
pub const HEADER_LEN: usize = 29;

/// One entry of a Raft log. The payload is opaque to storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub index: u64,
    pub term: u64,
    pub kind: u8,
    pub payload: Bytes,
}

impl Record {
    pub fn new(index: u64, term: u64, kind: u8, payload: impl Into<Bytes>) -> Record {
        Record {
            index,
            term,
            kind,
            payload: payload.into(),
        }
    }

    /// Bytes the record occupies on disk.
    pub fn disk_len(&self) -> usize {
        HEADER_LEN + self.payload.len()
    }

    pub(crate) fn header(&self) -> [u8; HEADER_LEN] {
        let mut h = [0; HEADER_LEN];
        h[4..8].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        h[8..16].copy_from_slice(&self.index.to_le_bytes());
        h[16..24].copy_from_slice(&self.term.to_le_bytes());
        h[24] = self.kind;
        h[25..29].copy_from_slice(&crc32c::crc32c(&self.payload).to_le_bytes());
        let crc = crc32c::crc32c(&h[4..]);
        h[0..4].copy_from_slice(&crc.to_le_bytes());
        h
    }

    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.header());
        out.extend_from_slice(&self.payload);
    }
}

/// A decoded header whose CRC checked out.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    pub payload_size: u32,
    pub index: u64,
    pub term: u64,
    pub kind: u8,
    pub payload_crc: u32,
}

impl Header {
    pub(crate) fn record_len(&self) -> usize {
        HEADER_LEN + self.payload_size as usize
    }
}

/// What the bytes at a position hold.
#[derive(Debug)]
pub(crate) enum Parsed {
    /// A whole, checksummed record and its length on disk.
    Record(Record, usize),
    /// Fewer bytes than the record needs; at least this many are needed.
    Short(usize),
    /// An all-zero header: the end of the log.
    End,
    /// A header or payload that fails its checksum.
    Corrupt(&'static str),
}

pub(crate) fn parse_header(buf: &[u8]) -> Result<Option<Header>, &'static str> {
    debug_assert!(buf.len() >= HEADER_LEN);
    let h = &buf[..HEADER_LEN];
    if h.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    let crc = u32::from_le_bytes(h[0..4].try_into().unwrap());
    if crc32c::crc32c(&h[4..]) != crc {
        return Err("header checksum mismatch");
    }
    Ok(Some(Header {
        payload_size: u32::from_le_bytes(h[4..8].try_into().unwrap()),
        index: u64::from_le_bytes(h[8..16].try_into().unwrap()),
        term: u64::from_le_bytes(h[16..24].try_into().unwrap()),
        kind: h[24],
        payload_crc: u32::from_le_bytes(h[25..29].try_into().unwrap()),
    }))
}

/// Parses the record at the start of `buf`, sharing its payload.
pub(crate) fn parse(buf: &Bytes) -> Parsed {
    if buf.len() < HEADER_LEN {
        return Parsed::Short(HEADER_LEN);
    }
    let h = match parse_header(buf) {
        Ok(Some(h)) => h,
        Ok(None) => return Parsed::End,
        Err(e) => return Parsed::Corrupt(e),
    };
    let len = h.record_len();
    if buf.len() < len {
        return Parsed::Short(len);
    }
    let payload = buf.slice(HEADER_LEN..len);
    if crc32c::crc32c(&payload) != h.payload_crc {
        return Parsed::Corrupt("payload checksum mismatch");
    }
    Parsed::Record(
        Record {
            index: h.index,
            term: h.term,
            kind: h.kind,
            payload,
        },
        len,
    )
}
