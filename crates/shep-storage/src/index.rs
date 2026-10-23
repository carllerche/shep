//! A segment's sparse index, its `.index` file, and the recovery scan that
//! rebuilds both from the data.
//!
//! The index maps a record index to the byte position of its record, for the
//! first record and then roughly every [`INDEX_INTERVAL`] bytes, so a read
//! seeks to the nearest entry at or below the index it wants and scans
//! forward at most that far.
//!
//! `<base:020>-<term>.index`, all little-endian:
//!
//! ```text
//! magic: b"SHIX"  version: u32  base: u64  term: u64  last: u64  size: u64
//! count: u32  [index: u64, position: u64] * count  crc32c: u32
//! ```
//!
//! `size` is the segment file's length when the index was written; an index
//! whose size doesn't match the file is stale and is rebuilt by scanning.

use crate::io::{File, StorageIo};
use shep_smp_io::FileIo;
use std::io;

use crate::record::{self, HEADER_LEN};

/// Bytes of data between index entries.
pub(crate) const INDEX_INTERVAL: u64 = 32 * 1024;

const MAGIC: &[u8; 4] = b"SHIX";
const VERSION: u32 = 1;

/// Bytes read per I/O while scanning.
const SCAN_BLOCK: usize = 1 << 20;

/// Sparse record-to-byte offsets used to bound the scan needed for a point lookup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SparseIndex {
    /// `(record index, byte offset)` pairs, both increasing.
    entries: Vec<(u64, u64)>,
}

impl SparseIndex {
    /// Notes that the record `index` starts at `pos`, if it's due an entry.
    pub(crate) fn maybe_add(&mut self, index: u64, pos: u64) {
        if self
            .entries
            .last()
            .is_none_or(|&(_, p)| pos >= p + INDEX_INTERVAL)
        {
            self.entries.push((index, pos));
        }
    }

    /// The last entry at or below `index`.
    pub(crate) fn nearest(&self, index: u64) -> Option<(u64, u64)> {
        let n = self.entries.partition_point(|&(i, _)| i <= index);
        n.checked_sub(1).map(|n| self.entries[n])
    }

    /// Drops the entries for `index` and above.
    pub(crate) fn truncate(&mut self, index: u64) {
        let n = self.entries.partition_point(|&(i, _)| i < index);
        self.entries.truncate(n);
    }
}

/// What an `.index` file records about a closed segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SegmentMeta {
    /// First record index, also encoded in the segment filename.
    pub base: u64,
    /// Raft term encoded in the segment filename and validated during recovery.
    pub term: u64,
    /// Inclusive last record index in this closed segment.
    pub last: u64,
    /// Logical data length in bytes, excluding padding and preallocation.
    pub size: u64,
    /// Sparse record-start offsets used to accelerate later reads.
    pub index: SparseIndex,
}

impl SegmentMeta {
    /// Encode closed-segment metadata and sparse offsets with a checksum for torn-write detection.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(48 + self.index.entries.len() * 16);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&VERSION.to_le_bytes());
        for v in [self.base, self.term, self.last, self.size] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&(self.index.entries.len() as u32).to_le_bytes());
        for &(i, p) in &self.index.entries {
            b.extend_from_slice(&i.to_le_bytes());
            b.extend_from_slice(&p.to_le_bytes());
        }
        let crc = crc32c::crc32c(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    /// `None` if the bytes are torn or not an index.
    pub(crate) fn decode(b: &[u8]) -> Option<SegmentMeta> {
        let (body, crc) = b.split_at_checked(b.len().checked_sub(4)?)?;
        if crc32c::crc32c(body) != u32::from_le_bytes(crc.try_into().ok()?) {
            return None;
        }
        let mut r = Reader(body);
        if r.take(4)? != MAGIC || r.u32()? != VERSION {
            return None;
        }
        let (base, term, last, size) = (r.u64()?, r.u64()?, r.u64()?, r.u64()?);
        let count = r.u32()? as usize;
        let mut entries = Vec::with_capacity(count.min(body.len() / 16));
        for _ in 0..count {
            entries.push((r.u64()?, r.u64()?));
        }
        r.0.is_empty().then_some(SegmentMeta {
            base,
            term,
            last,
            size,
            index: SparseIndex { entries },
        })
    }
}

/// Borrowed decoding cursor shared by index and KV formats; short input returns None.
pub(crate) struct Reader<'a>(pub &'a [u8]);

impl<'a> Reader<'a> {
    /// Consume an exact byte slice, leaving the cursor unchanged if the input is short.
    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (a, b) = self.0.split_at_checked(n)?;
        self.0 = b;
        Some(a)
    }

    /// Consume one byte, or report truncated input.
    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    /// Consume a little-endian 32-bit integer, or report truncated input.
    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    /// Consume a little-endian 64-bit integer, or report truncated input.
    pub(crate) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

/// The outcome of scanning a segment's records from some position.
#[derive(Debug)]
pub(crate) struct Scan {
    /// The last good record's index, if any.
    pub last: Option<u64>,
    /// Where the last good record ends.
    pub end: u64,
    /// Why the scan stopped short of the file's end, if it did.
    pub stop: Option<&'static str>,
}

/// Scans records from `pos`, where record `index` should start, checking
/// checksums and that indexes count up by one in `term`. Stops at the first
/// bad record, at an all-zero header, at the end of the file, or before
/// record `until`. Adds entries to `sparse` along the way.
pub(crate) async fn scan(
    io: &StorageIo,
    file: &File,
    mut pos: u64,
    mut index: u64,
    term: u64,
    until: Option<u64>,
    sparse: &mut SparseIndex,
) -> io::Result<Scan> {
    let file_len = file.len().await?;
    let mut buf = Buffered {
        file,
        io,
        file_len,
        buf: Vec::new(),
        at: pos,
    };
    let mut last = None;
    let stop = loop {
        if until == Some(index) {
            break None;
        }
        let Some(h) = buf.get(pos, HEADER_LEN).await? else {
            break (pos < file_len).then_some("short header");
        };
        let h = match record::parse_header(h) {
            Ok(Some(h)) => h,
            Ok(None) => break Some("zero header"),
            Err(e) => break Some(e),
        };
        if h.index != index {
            break Some("index discontinuity");
        }
        if h.term != term {
            break Some("term differs from the segment's");
        }
        let len = h.record_len();
        let Some(rec) = buf.get(pos, len).await? else {
            break Some("short payload");
        };
        if crc32c::crc32c(&rec[HEADER_LEN..]) != h.payload_crc {
            break Some("payload checksum mismatch");
        }
        sparse.maybe_add(index, pos);
        last = Some(index);
        pos += len as u64;
        index += 1;
    };
    Ok(Scan {
        last,
        end: pos,
        stop,
    })
}

/// Sequential reads through a file in large blocks.
struct Buffered<'a> {
    /// Borrowed descriptor being scanned.
    file: &'a File,
    /// Core-local service used for lower-share recovery reads.
    io: &'a StorageIo,
    /// Byte boundary beyond which this scan must not read.
    file_len: u64,
    /// Most recently fetched contiguous block of file bytes.
    buf: Vec<u8>,
    /// File position of `buf[0]`.
    at: u64,
}

impl Buffered<'_> {
    /// The `len` bytes at `pos`, or `None` if the file ends first.
    async fn get(&mut self, pos: u64, len: usize) -> io::Result<Option<&[u8]>> {
        if pos + len as u64 > self.file_len {
            return Ok(None);
        }
        let end = self.at + self.buf.len() as u64;
        if pos < self.at || pos + len as u64 > end {
            let want = (len.max(SCAN_BLOCK) as u64).min(self.file_len - pos) as usize;
            self.buf = self.io.read_recovery(self.file, pos, want).await?;
            if self.buf.len() < len {
                return Ok(None);
            }
            self.at = pos;
        }
        let off = (pos - self.at) as usize;
        Ok(Some(&self.buf[off..off + len]))
    }
}
