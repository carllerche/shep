use crate::io::{Fd, Io, PollAdd};
use crate::net::{Buffer, Config, Read, ReadHalf, Write, WriteAll, WriteHalf, socket::Socket};
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
        let config = config.validate()?;
        let fd = shep_sys::tcp_socket(&address)?;
        let socket = Socket::new(io, Fd::new(fd)?, config);
        match shep_sys::connect(&*socket.file, &address) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EINPROGRESS) => {
                socket
                    .run(PollAdd {
                        file: socket.file.clone(),
                        events: libc::POLLOUT as u32,
                    })
                    .map_err(|r| r.error)?
                    .await
                    .result?;
                shep_sys::socket_error(&*socket.file)?;
            }
            Err(error) => return Err(error),
        }
        shep_sys::set_file_flags(&*socket.file, 0)?;
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
    pub fn read(&mut self, buffer: Buffer) -> Read<'_> {
        Read::new(&self.socket, buffer)
    }
    pub fn write(&mut self, buffer: Buffer) -> Write<'_> {
        Write::new(&self.socket, buffer)
    }
    /// Send the full buffer. Errors/cancellation may follow partial sends.
    pub fn write_all(&mut self, buffer: Buffer) -> WriteAll<'_> {
        WriteAll::new(&self.socket, buffer)
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
