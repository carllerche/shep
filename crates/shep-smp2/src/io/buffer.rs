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
    /// Fixed initialized allocation, including alignment padding; never reallocated.
    storage: Box<[u8]>,
    /// Offset of the aligned data region within storage.
    start: usize,
    /// Visible byte length exposed by Deref; always initialized.
    len: usize,
    /// Requested power-of-two byte alignment for the data address.
    alignment: usize,
}

impl Buffer {
    /// Allocate zeroed stable storage at a nonzero power-of-two alignment.
    /// Zero-length buffers still reserve an aligned byte; invalid/overflowing
    /// layouts and allocation failures return errors.
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

    /// Copy bytes into a new aligned allocation; propagate layout/allocation errors.
    pub fn from_slice(bytes: &[u8], alignment: usize) -> io::Result<Self> {
        let mut buffer = Self::new(bytes.len(), alignment)?;
        buffer.copy_from_slice(bytes);
        Ok(buffer)
    }

    pub fn alignment(&self) -> usize {
        self.alignment
    }
    /// Maximum length without reallocating; may include spare alignment padding.
    pub fn capacity(&self) -> usize {
        self.storage.len() - self.start
    }

    /// Resize without moving storage, zeroing newly exposed bytes. Panics if
    /// len exceeds capacity; shrinking does not release the allocation.
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
