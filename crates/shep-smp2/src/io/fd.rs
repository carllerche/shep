//! Descriptor ownership and the admission invariant shared by all io2 operations.
#![allow(unsafe_code)]

use std::{
    fs::File,
    io,
    marker::PhantomData,
    os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd, RawFd},
    rc::Rc,
};

/// A thread-local owned descriptor that can be consumed by io_uring CLOSE.
///
/// This type is neither Send nor Sync. Shared local ownership uses Rc<Fd>.
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<shep_smp2::io::Fd>();
/// ```
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<shep_smp2::io::Fd>();
/// ```
///
/// Use [`Self::close`] to explicitly await an io_uring CLOSE. Dropping the last
/// owner otherwise publishes a detached CLOSE through the currently entered
/// runtime. A full SQ first triggers bounded submission/reaping to make room.
/// Close falls back to synchronous release when runtime context is unavailable,
/// the driver is gone/closed/failed or borrowed, publication fails, or bounded
/// progress cannot free SQ space. Drop ignores errors and stores no runtime reference.
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
/// let close = shep_smp2::io::Operation::close(file);
/// ```
#[derive(Debug)]
pub struct Fd {
    file: Option<File>,
    _local: PhantomData<Rc<()>>,
}

impl Fd {
    /// Consume the descriptor and await an io_uring CLOSE.
    ///
    /// The caller must keep driving the supplied Io's driver. This future is
    /// lazy: dropping it before its first poll follows the descriptor's drop
    /// policy. Admission failure closes synchronously and returns the error.
    /// After publication, cancellation only abandons the result; the driver
    /// completes the CLOSE, including during shutdown. Flush errors are returned
    /// without retrying a descriptor number that may already have been reused.
    pub async fn close(self, io: &crate::io::Io) -> io::Result<()> {
        io.push(crate::io::Operation::close(self))
            .map_err(|rejected| rejected.error)?
            .await
            .result
            .map(|_| ())
    }

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
        Self {
            file: Some(fd.into()),
            _local: PhantomData,
        }
    }

    /// Duplicate the descriptor, preserving the invariant without revalidation.
    /// The duplicate shares its open file description (offset and status flags).
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: Some(self.as_file().try_clone()?),
            _local: PhantomData,
        })
    }

    /// Borrow the standard File for synchronous I/O or metadata operations.
    /// No mutable reference is exposed that could replace the owned descriptor.
    pub fn as_file(&self) -> &File {
        self.file.as_ref().expect("owned descriptor")
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        let Some(file) = self.file.take() else { return };
        let Some(io) = crate::rt::context::try_io() else {
            return;
        };
        // Operation::close transfers the File out of Fd, so rejection closes
        // synchronously rather than recursively consulting the runtime again.
        let fd = Self {
            file: Some(file),
            _local: PhantomData,
        };
        if let Ok(mut close) = io.push(super::Operation::close(fd)) {
            close.release(&io);
        }
    }
}

impl AsFd for Fd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.as_file().as_fd()
    }
}
impl AsRawFd for Fd {
    fn as_raw_fd(&self) -> RawFd {
        self.as_file().as_raw_fd()
    }
}
impl IntoRawFd for Fd {
    fn into_raw_fd(mut self) -> RawFd {
        self.file.take().unwrap().into_raw_fd()
    }
}
impl From<Fd> for File {
    fn from(mut fd: Fd) -> Self {
        fd.file.take().unwrap()
    }
}
impl From<Fd> for OwnedFd {
    fn from(mut fd: Fd) -> Self {
        fd.file.take().unwrap().into()
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

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::{
        Runtime,
        io::{Driver, Operation},
    };

    #[test]
    fn final_owner_in_a_completed_operation_uses_the_current_runtime() {
        // The descriptor was created outside any runtime and used on another
        // driver. Its final drop must use the runtime entered at that point.
        let mut driver = Driver::new(1).unwrap();
        let fd = Rc::new(Fd::new(tempfile::tempfile().unwrap()).unwrap());
        let raw = fd.as_raw_fd();
        let mut operation = driver
            .push(Operation::Fsync {
                file: fd.clone(),
                data_only: false,
            })
            .unwrap();
        drop(fd);
        operation.release(driver.io());
        let mut runtime = Runtime::new(Default::default()).unwrap();
        let io = runtime.io();
        let baseline = io.in_flight();
        let enter = runtime.enter();
        loop {
            driver.submit_and_wait(None).unwrap();
            if driver.tick() != 0 {
                break;
            }
        }
        assert_eq!(driver.in_flight(), 0);
        assert_eq!(io.in_flight(), baseline + 1);
        assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_ok());
        runtime.shutdown().unwrap();
        assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_err());
        drop(enter);
    }

    #[test]
    fn conversions_transfer_ownership_without_queuing_a_close() {
        let runtime = Runtime::new(Default::default()).unwrap();
        let io = runtime.io();
        let _enter = runtime.enter();
        let baseline = io.in_flight();
        let fd = Fd::new(tempfile::tempfile().unwrap()).unwrap();
        let duplicate = fd.try_clone().unwrap();
        let raw = fd.as_raw_fd();
        let file = File::from(fd);
        assert_eq!(io.in_flight(), baseline);
        assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_ok());
        drop(file);
        drop(duplicate);
        assert_eq!(io.in_flight(), baseline + 1);
    }

    #[test]
    fn dropping_while_the_arena_is_borrowed_falls_back_without_panicking() {
        let runtime = Runtime::new(Default::default()).unwrap();
        let io = runtime.io();
        let _enter = runtime.enter();
        let baseline = io.in_flight();
        let fd = Fd::new(tempfile::tempfile().unwrap()).unwrap();
        let raw = fd.as_raw_fd();
        let borrow = io.shared.state.borrow_mut();
        drop(fd);
        assert!(std::fs::read_link(format!("/proc/self/fd/{raw}")).is_err());
        drop(borrow);
        assert_eq!(io.in_flight(), baseline);
    }
}
