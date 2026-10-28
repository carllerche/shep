use std::{
    io,
    net::{Shutdown, SocketAddr},
    os::fd::{AsFd, AsRawFd, OwnedFd},
    time::Duration,
};

/// Create a nonblocking, close-on-exec TCP socket for the address's family.
pub fn tcp_socket(address: &SocketAddr) -> io::Result<OwnedFd> {
    let domain = if address.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    // SAFETY: socket has no pointer arguments and returns a fresh descriptor.
    crate::owned(unsafe {
        libc::socket(
            domain,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    })
}

/// Start a connection. A nonblocking socket can return EINPROGRESS.
pub fn connect(fd: impl AsFd, address: &SocketAddr) -> io::Result<()> {
    let fd = fd.as_fd();
    let result = match address {
        SocketAddr::V4(address) => {
            let addr = libc::sockaddr_in {
                sin_family: libc::AF_INET as _,
                sin_port: address.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(address.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: addr is initialized and valid for the supplied size during the call.
            unsafe {
                libc::connect(
                    fd.as_raw_fd(),
                    (&addr as *const libc::sockaddr_in).cast(),
                    size_of::<libc::sockaddr_in>() as _,
                )
            }
        }
        SocketAddr::V6(address) => {
            let addr = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as _,
                sin6_port: address.port().to_be(),
                sin6_flowinfo: address.flowinfo().to_be(),
                sin6_addr: libc::in6_addr {
                    s6_addr: address.ip().octets(),
                },
                sin6_scope_id: address.scope_id(),
            };
            // SAFETY: addr is initialized and valid for the supplied size during the call.
            unsafe {
                libc::connect(
                    fd.as_raw_fd(),
                    (&addr as *const libc::sockaddr_in6).cast(),
                    size_of::<libc::sockaddr_in6>() as _,
                )
            }
        }
    };
    crate::cvt(result).map(|_| ())
}

/// Read and clear SO_ERROR, returning any pending socket error.
pub fn socket_error(fd: impl AsFd) -> io::Result<()> {
    let mut error: libc::c_int = 0;
    let mut len = size_of_val(&error) as libc::socklen_t;
    // SAFETY: error and len are writable and correctly sized for SO_ERROR.
    crate::cvt(unsafe {
        libc::getsockopt(
            fd.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&mut error as *mut libc::c_int).cast(),
            &mut len,
        )
    })?;
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(error))
    }
}

/// Shut down one or both socket directions.
pub fn shutdown(fd: impl AsFd, how: Shutdown) -> io::Result<()> {
    let how = match how {
        Shutdown::Read => libc::SHUT_RD,
        Shutdown::Write => libc::SHUT_WR,
        Shutdown::Both => libc::SHUT_RDWR,
    };
    // SAFETY: shutdown takes only integer arguments and borrows the descriptor.
    crate::cvt(unsafe { libc::shutdown(fd.as_fd().as_raw_fd(), how) }).map(|_| ())
}

/// Set SO_LINGER, in whole seconds. None disables linger; zero enables reset on close.
pub fn set_socket_linger(fd: impl AsFd, linger: Option<Duration>) -> io::Result<()> {
    let value = libc::linger {
        l_onoff: i32::from(linger.is_some()),
        l_linger: linger
            .map_or(Ok(0), |d| libc::c_int::try_from(d.as_secs()))
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "linger duration exceeds c_int")
            })?,
    };
    // SAFETY: value is initialized and its size matches SO_LINGER's expected type.
    crate::cvt(unsafe {
        libc::setsockopt(
            fd.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&value as *const libc::linger).cast(),
            size_of_val(&value) as _,
        )
    })
    .map(|_| ())
}

/// Set the socket's send buffer size (Linux may round or double this value).
pub fn set_socket_send_buffer_size(fd: impl AsFd, size: usize) -> io::Result<()> {
    let value = libc::c_int::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "send buffer exceeds c_int"))?;
    // SAFETY: value is an initialized integer of the size SO_SNDBUF expects.
    crate::cvt(unsafe {
        libc::setsockopt(
            fd.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&value as *const libc::c_int).cast(),
            size_of_val(&value) as _,
        )
    })
    .map(|_| ())
}
