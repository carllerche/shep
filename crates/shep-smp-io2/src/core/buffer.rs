//! Stable, initialized buffer storage without a custom allocator.
#![forbid(unsafe_code)]

use std::{
    io,
    ops::{Deref, DerefMut},
};

/// Uniquely owned, initialized bytes at a stable, explicitly aligned address.
/// Moving this value does not move its backing allocation. The kernel owner
/// takes the whole buffer, preventing access until completion.
#[derive(Debug)]
pub struct Buffer {
    storage: Box<[u8]>,
    start: usize,
    len: usize,
    alignment: usize,
}

impl Buffer {
    /// Allocate zeroed storage. At most alignment - 1 extra bytes are reserved.
    pub fn new(len: usize, alignment: usize) -> io::Result<Self> {
        if !alignment.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid alignment",
            ));
        }
        let size = len
            .max(1)
            .checked_add(alignment - 1)
            .filter(|size| *size <= isize::MAX as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer too large"))?;
        let mut storage = Vec::new();
        storage.try_reserve_exact(size).map_err(io::Error::other)?;
        storage.resize(size, 0);
        let storage = storage.into_boxed_slice();
        let start = storage.as_ptr().addr().wrapping_neg() & (alignment - 1);
        Ok(Self {
            storage,
            start,
            len,
            alignment,
        })
    }

    pub fn from_slice(bytes: &[u8], alignment: usize) -> io::Result<Self> {
        let mut buffer = Self::new(bytes.len(), alignment)?;
        buffer.copy_from_slice(bytes);
        Ok(buffer)
    }

    pub fn alignment(&self) -> usize {
        self.alignment
    }
    pub fn capacity(&self) -> usize {
        self.storage.len() - self.start
    }

    /// Resize within the stable allocation, zeroing newly exposed bytes.
    pub fn resize(&mut self, len: usize) {
        assert!(len <= self.capacity(), "buffer capacity exceeded");
        let old = self.len;
        self.len = len;
        if len > old {
            self[old..].fill(0);
        }
    }
}
impl Deref for Buffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.storage[self.start..self.start + self.len]
    }
}
impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.storage[self.start..self.start + self.len]
    }
}
