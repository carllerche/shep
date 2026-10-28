use std::io;

/// Bound on each owned read/write and each write_all send.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub max_buffer_size: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_buffer_size: 64 * 1024,
        }
    }
}
impl Config {
    pub(super) fn validate(self) -> io::Result<Self> {
        if self.max_buffer_size == 0 || self.max_buffer_size > i32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid TCP buffer limit",
            ));
        }
        Ok(self)
    }
}
