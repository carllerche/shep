use std::{
    io,
    os::fd::{AsFd, AsRawFd, OwnedFd},
};

/// Read the descriptor's file status flags (`F_GETFL`).
pub fn file_flags(fd: impl AsFd) -> io::Result<libc::c_int> {
    // SAFETY: the borrowed descriptor is live; this command takes no argument.
    crate::cvt(unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), libc::F_GETFL) })
}

/// Set file status flags (`F_SETFL`). Flags shared by descriptor aliases change too.
pub fn set_file_flags(fd: impl AsFd, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: F_SETFL takes an integer, not a pointer.
    crate::cvt(unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), libc::F_SETFL, flags) }).map(|_| ())
}

/// Create a close-on-exec event counter, optionally nonblocking.
pub fn eventfd(initial: u32, nonblocking: bool) -> io::Result<OwnedFd> {
    let flags = libc::EFD_CLOEXEC | if nonblocking { libc::EFD_NONBLOCK } else { 0 };
    // SAFETY: eventfd has no pointer arguments and returns a fresh descriptor.
    crate::owned(unsafe { libc::eventfd(initial, flags) })
}

/// Create a close-on-exec epoll instance.
pub fn epoll() -> io::Result<OwnedFd> {
    // SAFETY: epoll_create1 has no pointer arguments and returns a fresh descriptor.
    crate::owned(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })
}

/// Query direct-I/O memory and offset alignment, or None if not reported.
/// Kernel errors are returned unchanged; callers choose their fallback policy.
pub fn direct_io_alignment(fd: impl AsFd) -> io::Result<Option<(u32, u32)>> {
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: the empty path is NUL terminated, the descriptor is borrowed, and
    // stat points to writable storage of the exact type and size statx expects.
    crate::cvt(unsafe {
        libc::statx(
            fd.as_fd().as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_DIOALIGN,
            stat.as_mut_ptr(),
        )
    })?;
    // SAFETY: statx succeeded and the entire storage was initialized to zero.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.stx_mask & libc::STATX_DIOALIGN != 0)
        .then_some((stat.stx_dio_mem_align, stat.stx_dio_offset_align)))
}
