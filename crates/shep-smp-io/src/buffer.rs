use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::io;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// An initialized, uniquely owned allocation with a stable address. Moving it
/// does not move its bytes. The driver takes ownership while the kernel uses it.
pub struct Buffer {
    /// Stable allocation address; ownership lasts through any kernel use.
    ptr: NonNull<u8>,
    /// Allocation size and alignment, also required when freeing the buffer.
    layout: Layout,
    /// Initialized byte prefix exposed to callers; never exceeds allocation size.
    len: usize,
}

// The allocation has unique ownership, and its contents are ordinary bytes.
unsafe impl Send for Buffer {}
unsafe impl Sync for Buffer {}

impl Buffer {
    /// Allocate zeroed bytes at the requested alignment, without later relocation.
    pub fn new(len: usize, alignment: usize) -> io::Result<Self> {
        let layout = Layout::from_size_align(len.max(1), alignment)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid buffer layout"))?;
        // All exposed bytes are initialized, including padding used by direct I/O.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })
            .unwrap_or_else(|| handle_alloc_error(layout));
        Ok(Self { ptr, layout, len })
    }

    /// Copy bytes into a stable allocation suitable for owned asynchronous I/O.
    pub fn from_slice(bytes: &[u8], alignment: usize) -> io::Result<Self> {
        let mut buffer = Self::new(bytes.len(), alignment)?;
        buffer.copy_from_slice(bytes);
        Ok(buffer)
    }

    /// Return the allocation alignment, which stays fixed across resizes.
    pub fn alignment(&self) -> usize {
        self.layout.align()
    }
    /// Return the allocation size; an empty buffer still owns at least one byte.
    pub fn capacity(&self) -> usize {
        self.layout.size()
    }
    /// Change the exposed length without reallocating, zeroing newly exposed bytes.
    /// Panics if the requested length exceeds capacity.
    pub fn resize(&mut self, len: usize) {
        assert!(len <= self.capacity());
        let old = self.len;
        self.len = len;
        if len > old {
            self[old..len].fill(0);
        }
    }
}

impl Deref for Buffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // Allocation remains live until Drop, and every byte is initialized.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}
impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}
impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Buffer")
            .field("len", &self.len)
            .field("alignment", &self.alignment())
            .finish()
    }
}
