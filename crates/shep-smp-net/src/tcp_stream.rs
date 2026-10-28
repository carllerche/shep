use crate::{Buffer, Config, ReadHalf, WriteHalf, socket::Socket};
use shep_smp_io2::{Fd, Io, Operation};
use std::{io, net::SocketAddr, os::fd::OwnedFd};

/// A local TCP connection, with no hidden write queue or clonable writer.
pub struct TcpStream {
    pub(super) socket: Socket,
}
impl TcpStream {
    /// Connect to a numeric address. Nonblocking connect setup precedes an
    /// io_uring readiness wait, so dropping even an unsubmitted wait can abort
    /// the connection without depending on cancellation admission.
    pub async fn connect(io: &Io, address: SocketAddr, config: Config) -> io::Result<Self> {
        use rustix::net::{AddressFamily, SocketFlags, SocketType};
        let config = config.validate()?;
        let domain = if address.is_ipv4() {
            AddressFamily::INET
        } else {
            AddressFamily::INET6
        };
        let fd = rustix::net::socket_with(
            domain,
            SocketType::STREAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )?;
        let socket = Socket::new(io, Fd::new(fd)?, config);
        match rustix::net::connect(&*socket.file, &address) {
            Ok(()) => {}
            Err(rustix::io::Errno::INPROGRESS) => {
                socket
                    .run(Operation::PollAdd {
                        file: socket.file.clone(),
                        events: libc::POLLOUT as u32,
                    })
                    .await
                    .map_err(|r| r.error)?
                    .result?;
                rustix::net::sockopt::socket_error(&*socket.file)??;
            }
            Err(error) => return Err(error.into()),
        }
        rustix::fs::fcntl_setfl(&*socket.file, rustix::fs::OFlags::empty())?;
        Ok(Self { socket })
    }
    /// Takes ownership; callers must not retain aliases that perform socket I/O
    /// or change descriptor flags. The socket is set to blocking for io_uring.
    pub fn from_std(io: &Io, stream: std::net::TcpStream, config: Config) -> io::Result<Self> {
        let config = config.validate()?;
        stream.set_nonblocking(false)?;
        let fd: OwnedFd = stream.into();
        Ok(Self {
            socket: Socket::new(io, Fd::new(fd)?, config),
        })
    }
    /// Borrow independent exclusive directions for full duplex I/O.
    pub fn split(&mut self) -> (ReadHalf<'_>, WriteHalf<'_>) {
        (
            ReadHalf {
                socket: &self.socket,
            },
            WriteHalf {
                socket: &self.socket,
            },
        )
    }
    pub async fn read(&mut self, buffer: Buffer) -> (io::Result<usize>, Buffer) {
        self.socket.transfer(buffer, false).await
    }
    pub async fn write(&mut self, buffer: Buffer) -> (io::Result<usize>, Buffer) {
        self.socket.transfer(buffer, true).await
    }
    /// Send all bytes with bounded temporary storage. Errors may follow partial
    /// sends; do not retry the whole message without an application protocol.
    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        WriteHalf {
            socket: &self.socket,
        }
        .write_all(bytes)
        .await
    }
    pub async fn flush(&mut self) -> io::Result<()> {
        self.socket.check()
    }
    pub async fn shutdown(&mut self) -> io::Result<()> {
        WriteHalf {
            socket: &self.socket,
        }
        .shutdown()
        .await
    }
    /// All prior writes have completed locally. Half-close the writer then
    /// release the socket. Peer delivery/acknowledgement is not guaranteed.
    pub async fn close(mut self) -> io::Result<()> {
        self.shutdown().await
    }
}
