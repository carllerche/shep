//! Safe wrappers for the Linux primitives used by Shep.
//!
//! Descriptors are borrowed for calls and owned on creation. Errors use
//! `std::io::Error`; wrappers do not retry interrupted operations implicitly.
#![deny(unsafe_op_in_unsafe_fn)]

mod fd;
mod socket;

pub use fd::{direct_io_alignment, epoll, eventfd, file_flags, set_file_flags};
pub use socket::{
    connect, set_socket_linger, set_socket_send_buffer_size, shutdown, socket_error, tcp_socket,
};

use std::{
    io,
    os::fd::{FromRawFd, OwnedFd},
};

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

// Only call with the result of a syscall creating a fresh descriptor.
fn owned(result: libc::c_int) -> io::Result<OwnedFd> {
    let fd = cvt(result)?;
    // SAFETY: callers supply a fresh descriptor whose ownership transfers here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
