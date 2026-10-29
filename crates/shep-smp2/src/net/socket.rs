use crate::io::{Completion, Fd, Io, Op, OperationData, Recv, Rejected, Send, Value};
use crate::net::{Buffer, Config};
use std::{
    cell::Cell,
    future::Future,
    io,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

pub(super) struct Socket {
    pub(super) io: Io,
    pub(super) file: Rc<Fd>,
    closed: Cell<bool>,
    pub(super) write_closed: Cell<bool>,
    pub(super) config: Config,
}
impl Socket {
    pub(super) fn new(io: &Io, file: Fd, config: Config) -> Self {
        Self {
            io: io.clone(),
            file: Rc::new(file),
            closed: Cell::new(false),
            write_closed: Cell::new(false),
            config,
        }
    }
    pub(super) fn check(&self) -> io::Result<()> {
        if self.closed.get() {
            Err(io::Error::new(io::ErrorKind::NotConnected, "socket closed"))
        } else {
            Ok(())
        }
    }
    pub(super) fn abort(&self) {
        self.closed.set(true);
        // Also terminates queued receives/sends and accepts submitted *after*
        // this call. Cancellation alone is insufficient when publication cannot progress.
        let _ = shep_sys::shutdown(&*self.file, std::net::Shutdown::Both);
    }
    /// Admission occurs when the enclosing public async operation is polled.
    pub(super) fn run<T: OperationData>(
        &self,
        operation: T,
    ) -> Result<Pending<'_, T>, Rejected<T>> {
        self.io.submit(operation).map(|handle| Pending {
            socket: self,
            handle: Some(handle),
        })
    }
    pub(super) async fn transfer<T: Transfer>(
        &self,
        buffer: Buffer,
    ) -> (io::Result<usize>, Buffer) {
        let check = self.check().and_then(|()| {
            if T::WRITE && self.write_closed.get() {
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
        let pending = match self.run(T::new(self.file.clone(), buffer)) {
            Ok(pending) => pending,
            Err(rejected) => return (Err(rejected.error), rejected.operation.into_buffer()),
        };
        let completion = pending.await;
        (
            completion.result.map(Value::into_count),
            completion.operation.into_buffer(),
        )
    }
}
impl Drop for Socket {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Concrete input/output storage for the common send/receive adapter.
pub(super) trait Transfer: OperationData {
    const WRITE: bool;
    fn new(socket: Rc<Fd>, buffer: Buffer) -> Self;
    fn into_buffer(self) -> Buffer;
}
macro_rules! transfers {
    ($($name:ident => $write:literal),*) => {$(
        impl Transfer for $name {
            const WRITE: bool = $write;
            fn new(socket: Rc<Fd>, buffer: Buffer) -> Self { Self { socket, buffer } }
            fn into_buffer(self) -> Buffer { self.buffer }
        }
    )*};
}
transfers!(Send => true, Recv => false);

/// The awaitable owner is also the abort-on-abandonment guard. There is no
/// enclosing async frame retaining a second operation argument or poll closure.
pub(super) struct Pending<'a, T: OperationData> {
    socket: &'a Socket,
    handle: Option<Op<T>>,
}
impl<T: OperationData> Future for Pending<'_, T> {
    type Output = Completion<T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let completion = std::task::ready!(
            this.handle
                .as_mut()
                .expect("operation already consumed")
                .poll_complete(cx)
        );
        this.handle = None;
        Poll::Ready(completion)
    }
}
impl<T: OperationData> Drop for Pending<'_, T> {
    fn drop(&mut self) {
        if let Some(mut handle) = self.handle.take() {
            self.socket.abort();
            let target = handle.cancel_target();
            if let Ok(mut cancel) = self.socket.io.submit(target) {
                cancel.release();
            }
            handle.release();
        }
    }
}
