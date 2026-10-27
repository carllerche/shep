//! Descriptor ownership and the admission invariant shared by all io2 operations.
#![allow(unsafe_code)]

use std::{
    fs::File,
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd, RawFd},
};

/// An owned descriptor that can be consumed by io_uring CLOSE.
///
/// The descriptor must be live, uniquely owned, and must not refer to an
/// io_uring instance. Duplicates with distinct descriptor numbers are allowed.
/// All io2 operations accept this type, so CLOSE needs no validation syscall.
///
/// Generic imports are checked once by [`Self::new`]. In particular, a standard
/// `File` or socket wrapper can contain an arbitrary `OwnedFd`, so its Rust type
/// alone does not establish this invariant. io2's open/socket/accept outputs and
/// [`Self::try_clone`] establish it without an additional validation syscall.
///
/// A standard File cannot bypass the construction boundary:
/// ```compile_fail
/// let file = std::fs::File::open("/dev/null").unwrap();
/// let close = shep_smp_io2::Operation::close(file);
/// ```
#[derive(Debug)]
pub struct Fd {
    file: File,
}

impl Fd {
    /// Import and validate an owned descriptor. On error the input is closed.
    ///
    /// Calls fstat once. For anonymous inodes only, also reads `/proc/self/fd`
    /// to distinguish io_uring from supported descriptors such as eventfd and
    /// epoll. If that inspection fails, the import fails rather than guessing.
    pub fn new(fd: impl Into<OwnedFd>) -> io::Result<Self> {
        let fd = fd.into();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fd is live and stat is writable storage of the required size.
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Successful fstat initialized stat.
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT == 0 {
            let target = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
            if target == std::path::Path::new("anon_inode:[io_uring]") {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "io_uring descriptors cannot be owned by io2::Fd",
                ));
            }
        }
        // SAFETY: The owned descriptor was checked above and cannot be replaced
        // while this function owns it without violating Rust's I/O safety rules.
        Ok(unsafe { Self::from_owned_fd_unchecked(fd) })
    }

    /// Import without validation or syscalls.
    ///
    /// # Safety
    /// The descriptor must not refer to an io_uring instance. The caller must
    /// also uphold OwnedFd's live, exclusive descriptor ownership requirements.
    /// Ordinary filesystem descriptors, sockets, eventfd and epoll are valid.
    pub unsafe fn from_owned_fd_unchecked(fd: OwnedFd) -> Self {
        Self { file: fd.into() }
    }

    /// Duplicate the descriptor, preserving the invariant without revalidation.
    /// The duplicate shares its open file description (offset and status flags).
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
        })
    }

    /// Borrow the standard File for synchronous I/O or metadata operations.
    /// No mutable reference is exposed that could replace the owned descriptor.
    pub fn as_file(&self) -> &File {
        &self.file
    }
}

impl AsFd for Fd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}
impl AsRawFd for Fd {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}
impl IntoRawFd for Fd {
    fn into_raw_fd(self) -> RawFd {
        self.file.into_raw_fd()
    }
}
impl From<Fd> for File {
    fn from(fd: Fd) -> Self {
        fd.file
    }
}
impl From<Fd> for OwnedFd {
    fn from(fd: Fd) -> Self {
        fd.file.into()
    }
}

macro_rules! checked_import {
    ($($ty:ty),+ $(,)?) => { $(
        impl TryFrom<$ty> for Fd {
            type Error = io::Error;
            fn try_from(fd: $ty) -> io::Result<Self> { Self::new(fd) }
        }
    )+ };
}
checked_import!(
    OwnedFd,
    File,
    std::net::TcpListener,
    std::net::TcpStream,
    std::net::UdpSocket,
    std::os::unix::net::UnixListener,
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixDatagram,
);
