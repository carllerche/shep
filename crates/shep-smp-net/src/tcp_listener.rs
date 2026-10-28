use crate::{Config, TcpStream, socket::Socket};
use shep_smp_io2::{Fd, Io, Operation, Value};
use std::{io, net::SocketAddr, os::fd::OwnedFd};

/// One local listener. Only one accept may be outstanding.
/// Canceling an admitted accept closes the listener; bind a new one to resume.
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
    pub async fn accept(&mut self) -> io::Result<TcpStream> {
        self.socket.check()?;
        let completion = self
            .socket
            .run(Operation::Accept {
                socket: self.socket.file.clone(),
            })
            .await
            .map_err(|r| r.error)?;
        match completion.result? {
            Value::File(file) => Ok(TcpStream {
                socket: Socket::new(&self.socket.io, file, self.socket.config),
            }),
            _ => unreachable!("accept returns a descriptor"),
        }
    }
    /// Stop accepting. Pending kernel work retains its descriptor until reaped.
    pub fn close(self) {
        self.socket.abort();
    }
}
