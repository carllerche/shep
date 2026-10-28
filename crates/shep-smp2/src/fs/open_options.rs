use std::io;

/// File opening policy. Direct I/O is explicit and never silently downgraded.
#[derive(Clone, Copy, Debug)]
pub struct OpenOptions {
    pub read: bool,
    pub write: bool,
    pub create: bool,
    pub create_new: bool,
    pub truncate: bool,
    pub direct: bool,
}
impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            read: true,
            write: true,
            create: false,
            create_new: false,
            truncate: false,
            direct: false,
        }
    }
}
impl OpenOptions {
    pub(super) fn flags(self) -> io::Result<i32> {
        if (!self.read && !self.write)
            || (!self.write && (self.create || self.create_new || self.truncate))
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut flags = match (self.read, self.write) {
            (true, true) => libc::O_RDWR,
            (false, true) => libc::O_WRONLY,
            _ => libc::O_RDONLY,
        };
        if self.create || self.create_new {
            flags |= libc::O_CREAT;
        }
        if self.create_new {
            flags |= libc::O_EXCL;
        }
        // Match std's exclusive-create behavior: existing files are never truncated.
        if self.truncate && !self.create_new {
            flags |= libc::O_TRUNC;
        }
        if self.direct {
            flags |= libc::O_DIRECT;
        }
        Ok(flags)
    }
}
