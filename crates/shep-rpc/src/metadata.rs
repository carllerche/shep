//! The metadata section: key/value pairs between the header and the body.
//!
//! ```text
//! +---------+---------+-------------------+---------+---------+-----
//! | key u16 | len u16 | value (len bytes) | key u16 | len u16 | ...
//! +---------+---------+-------------------+---------+---------+-----
//! ```
//!
//! Entries are packed with no padding, little-endian, in strictly increasing
//! key order, and exactly fill the section. That is all the transport
//! checks: keys and values mean whatever the service defines. A receiver
//! reads the section, checks it with the header, and can act on it (say, to
//! choose where to serve a request) before reading the body.

use std::collections::BTreeMap;

use crate::wire::MAX_METADATA;

/// Bytes of an entry before its value: key and length.
const ENTRY_HEADER: usize = 4;

/// Why a metadata section is malformed, or can't be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetadataError {
    #[error("metadata entry at byte {at} runs past the end of the section")]
    Truncated { at: usize },
    #[error("metadata key {key} is not above the key before it")]
    OutOfOrder { key: u16 },
    #[error("metadata of {0} bytes is larger than {MAX_METADATA}")]
    TooLarge(usize),
    #[error("metadata value of {0} bytes is larger than a u16 length")]
    ValueTooLarge(usize),
}

/// A well-formed metadata section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    bytes: Vec<u8>,
}

impl Metadata {
    /// No entries.
    pub fn empty() -> Metadata {
        Metadata::default()
    }

    /// Checks that `bytes` is a well-formed section: entries that fit, keys
    /// strictly increasing, at most [`MAX_METADATA`] bytes.
    pub fn parse(bytes: Vec<u8>) -> Result<Metadata, MetadataError> {
        if bytes.len() > MAX_METADATA {
            return Err(MetadataError::TooLarge(bytes.len()));
        }
        let mut last = None;
        let mut at = 0;
        while at < bytes.len() {
            let (key, value) = entry(&bytes, at).ok_or(MetadataError::Truncated { at })?;
            if last.is_some_and(|last| key <= last) {
                return Err(MetadataError::OutOfOrder { key });
            }
            last = Some(key);
            at = value.end;
        }
        Ok(Metadata { bytes })
    }

    /// The section as it goes on the wire.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The value under `key`, if there is one.
    pub fn get(&self, key: u16) -> Option<&[u8]> {
        self.iter()
            .find(|&(k, _)| k >= key)
            .filter(|&(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// The entries in key order.
    pub fn iter(&self) -> impl Iterator<Item = (u16, &[u8])> + '_ {
        let mut at = 0;
        std::iter::from_fn(move || {
            let (key, value) = entry(&self.bytes, at)?;
            at = value.end;
            Some((key, &self.bytes[value]))
        })
    }
}

/// The entry at `at`: its key and where its value lies, if it fits.
fn entry(bytes: &[u8], at: usize) -> Option<(u16, std::ops::Range<usize>)> {
    let head = bytes.get(at..at + ENTRY_HEADER)?;
    let key = u16::from_le_bytes([head[0], head[1]]);
    let len = u16::from_le_bytes([head[2], head[3]]) as usize;
    let start = at + ENTRY_HEADER;
    let value = start..start + len;
    (value.end <= bytes.len()).then_some((key, value))
}

/// Builds a metadata section from entries added in any order.
#[derive(Clone, Debug, Default)]
pub struct MetadataBuilder {
    entries: BTreeMap<u16, Vec<u8>>,
}

impl MetadataBuilder {
    pub fn new() -> MetadataBuilder {
        MetadataBuilder::default()
    }

    /// Sets `key` to `value`, replacing any value it had.
    pub fn insert(mut self, key: u16, value: impl Into<Vec<u8>>) -> MetadataBuilder {
        self.entries.insert(key, value.into());
        self
    }

    /// Sets `key` to `value` as a little-endian `u64`, the conventional
    /// encoding of integers.
    pub fn insert_u64(self, key: u16, value: u64) -> MetadataBuilder {
        self.insert(key, value.to_le_bytes().to_vec())
    }

    pub fn build(self) -> Result<Metadata, MetadataError> {
        let mut size = 0;
        for value in self.entries.values() {
            if value.len() > u16::MAX as usize {
                return Err(MetadataError::ValueTooLarge(value.len()));
            }
            size += ENTRY_HEADER + value.len();
        }
        if size > MAX_METADATA {
            return Err(MetadataError::TooLarge(size));
        }
        let mut bytes = Vec::with_capacity(size);
        for (key, value) in &self.entries {
            bytes.extend_from_slice(&key.to_le_bytes());
            bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
            bytes.extend_from_slice(value);
        }
        Ok(Metadata { bytes })
    }
}
