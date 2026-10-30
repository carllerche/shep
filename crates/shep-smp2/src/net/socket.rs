use crate::io::{Completion, Fd, Io, Op, OperationData, Recv, Rejected, Send};
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
            cancel_requested: false,
        })
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

/// Owns the completion claim and requests operation cancellation on abandonment.
pub(super) struct Pending<'a, T: OperationData> {
    socket: &'a Socket,
    handle: Option<Op<T>>,
    cancel_requested: bool,
}
impl<T: OperationData> Pending<'_, T> {
    pub(super) fn request_cancel(&mut self) {
        if !self.cancel_requested {
            if let Some(handle) = &self.handle {
                // Cancellation publication is best effort. The original claim
                // (or its abandonment storage) still owns inputs until completion.
                let _ = self.socket.io.request_cancel(handle.cancel_target());
            }
            self.cancel_requested = true;
        }
    }
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
        self.request_cancel();
        // Op's destructor abandons its result, retaining kernel-owned inputs.
    }
}
