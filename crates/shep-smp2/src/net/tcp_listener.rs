use crate::io::{Fd, Io};
use crate::net::{Accept, Config, socket::Socket};
use std::{io, net::SocketAddr, os::fd::OwnedFd};

/// One local listener. Only one accept may be outstanding.
/// Canceling an accept leaves the listener open; await cancellation before reusing it.
pub struct TcpListener {
    socket: Socket,
    address: SocketAddr,
}
impl TcpListener {
    /// Bind a numeric address synchronously; does not perform DNS resolution.
    pub fn bind(io: &Io, address: SocketAddr, config: Config) -> io::Result<Self> {
        Self::from_std(io, std::net::TcpListener::bind(address)?, config)
    }
    pub fn from_std(io: &Io, listener: std::net::TcpListener, config: Config) -> io::Result<Self> {
        let config = config.validate()?;
        listener.set_nonblocking(false)?;
        let address = listener.local_addr()?;
        let fd: OwnedFd = listener.into();
        Ok(Self {
            socket: Socket::new(io, Fd::new(fd)?, config),
            address,
        })
    }
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }
    pub fn accept(&mut self) -> Accept<'_> {
        Accept::new(&self.socket)
    }
    /// Stop accepting. Pending kernel work retains its descriptor until reaped.
    pub fn close(self) {
        self.socket.abort();
    }
}
