//! Local TCP with owned buffers, exclusive directions, and no write backlog.
//!
//! Futures are lazy. Admission saturation returns `WouldBlock`. Dropping an
//! admitted accept/read/write/connect future shuts down the entire resource:
//! cancellation is terminal, and may leave partial network effects. Unpolled
//! futures have no effects. Runtime shutdown can therefore retire abandoned
//! socket operations even when the driver's cancellation reserve is occupied.
//! `flush` means all preceding writes completed locally, not peer acknowledgement.
#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

pub use shep_smp_io2::Buffer;
use shep_smp_io2::{Completion, Fd, Io, Operation, OperationHandle, Value};
use std::{cell::Cell, future::poll_fn, io, net::SocketAddr, os::fd::OwnedFd, rc::Rc};

/// Bound on each owned read/write and each temporary write_all chunk.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub max_buffer_size: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_buffer_size: 64 * 1024,
        }
    }
}
impl Config {
    fn validate(self) -> io::Result<Self> {
        if self.max_buffer_size == 0 || self.max_buffer_size > i32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid TCP buffer limit",
            ));
        }
        Ok(self)
    }
}

struct Socket {
    io: Io,
    file: Rc<Fd>,
    closed: Cell<bool>,
    write_closed: Cell<bool>,
    config: Config,
}
impl Socket {
    fn new(io: Io, file: Fd, config: Config) -> Self {
        Self {
            io,
            file: Rc::new(file),
            closed: Cell::new(false),
            write_closed: Cell::new(false),
            config,
        }
    }
    fn check(&self) -> io::Result<()> {
        if self.closed.get() {
            Err(io::Error::new(io::ErrorKind::NotConnected, "socket closed"))
        } else {
            Ok(())
        }
    }
    fn abort(&self) {
        self.closed.set(true);
        // Also terminates queued receives/sends and accepts submitted *after*
        // this call. Cancellation alone is insufficient when its reserve is full.
        let _ = rustix::net::shutdown(&*self.file, rustix::net::Shutdown::Both);
    }
    async fn run(&self, operation: Operation) -> Result<Completion, shep_smp_io2::Rejected> {
        let handle = self.io.push(operation)?;
        let mut pending = Pending {
            socket: self,
            handle: Some(handle),
        };
        let completion =
            poll_fn(|cx| pending.handle.as_mut().unwrap().poll_complete(&self.io, cx)).await;
        pending.handle = None;
        Ok(completion)
    }
    async fn transfer(&self, buffer: Buffer, write: bool) -> (io::Result<usize>, Buffer) {
        let check = self.check().and_then(|()| {
            if write && self.write_closed.get() {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "write direction closed",
                ))
            } else if buffer.len() > self.config.max_buffer_size {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "TCP buffer exceeds configured limit",
                ))
            } else {
                Ok(())
            }
        });
        if let Err(error) = check {
            return (Err(error), buffer);
        }
        if buffer.is_empty() {
            return (Ok(0), buffer);
        }
        let operation = if write {
            Operation::Send {
                socket: self.file.clone(),
                buffer,
            }
        } else {
            Operation::Recv {
                socket: self.file.clone(),
                buffer,
            }
        };
        match self.run(operation).await {
            Ok(completion) => (
                completion.result.map(Value::into_count),
                completion.operation.into_buffer().unwrap(),
            ),
            Err(rejected) => (
                Err(rejected.error),
                rejected.operation.into_buffer().unwrap(),
            ),
        }
    }
}
impl Drop for Socket {
    fn drop(&mut self) {
        self.abort();
    }
}

struct Pending<'a> {
    socket: &'a Socket,
    handle: Option<OperationHandle>,
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Some(mut handle) = self.handle.take() {
            self.socket.abort();
            let target = handle.cancel_target(&self.socket.io);
            if let Ok(mut cancel) = self.socket.io.push(Operation::Cancel(target)) {
                cancel.release(&self.socket.io);
            }
            handle.release(&self.socket.io);
        }
    }
}

/// One local listener. Only one accept may be outstanding.
/// Canceling an admitted accept closes the listener; bind a new one to resume.
pub struct TcpListener {
    socket: Socket,
    address: SocketAddr,
}
impl TcpListener {
    /// Bind a numeric address synchronously; does not perform DNS resolution.
    pub fn bind(io: Io, address: SocketAddr, config: Config) -> io::Result<Self> {
        Self::from_std(io, std::net::TcpListener::bind(address)?, config)
    }
    pub fn from_std(io: Io, listener: std::net::TcpListener, config: Config) -> io::Result<Self> {
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
                socket: Socket::new(self.socket.io.clone(), file, self.socket.config),
            }),
            _ => unreachable!("accept returns a descriptor"),
        }
    }
    /// Stop accepting. Pending kernel work retains its descriptor until reaped.
    pub fn close(self) {
        self.socket.abort();
    }
}

/// A local TCP connection, with no hidden write queue or clonable writer.
pub struct TcpStream {
    socket: Socket,
}
impl TcpStream {
    /// Connect to a numeric address. Nonblocking connect setup precedes an
    /// io_uring readiness wait, so dropping even an unsubmitted wait can abort
    /// the connection without depending on cancellation admission.
    pub async fn connect(io: Io, address: SocketAddr, config: Config) -> io::Result<Self> {
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
    pub fn from_std(io: Io, stream: std::net::TcpStream, config: Config) -> io::Result<Self> {
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

pub struct ReadHalf<'a> {
    socket: &'a Socket,
}
impl ReadHalf<'_> {
    pub async fn read(&mut self, buffer: Buffer) -> (io::Result<usize>, Buffer) {
        self.socket.transfer(buffer, false).await
    }
}
pub struct WriteHalf<'a> {
    socket: &'a Socket,
}
impl WriteHalf<'_> {
    pub async fn write(&mut self, buffer: Buffer) -> (io::Result<usize>, Buffer) {
        self.socket.transfer(buffer, true).await
    }
    pub async fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        self.socket.check()?;
        while !bytes.is_empty() {
            let len = bytes.len().min(self.socket.config.max_buffer_size);
            let buffer = Buffer::from_slice(&bytes[..len], 1)?;
            let (result, _) = self.write(buffer).await;
            let count = result?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "TCP send returned zero",
                ));
            }
            bytes = &bytes[count..];
        }
        Ok(())
    }
    /// There is no adapter backlog; each write awaits its own completion.
    pub async fn flush(&mut self) -> io::Result<()> {
        self.socket.check()
    }
    /// Send FIN after prior writes complete, retaining the read direction.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.socket.check()?;
        if self.socket.write_closed.get() {
            return Ok(());
        }
        self.socket
            .run(Operation::Shutdown {
                socket: self.socket.file.clone(),
                how: std::net::Shutdown::Write,
            })
            .await
            .map_err(|r| r.error)?
            .result?;
        self.socket.write_closed.set(true);
        Ok(())
    }
}
