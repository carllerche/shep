//! Lazy socket operations. Cancellation waits for the original terminal result.
use super::{
    Buffer, TcpStream,
    socket::{Pending, Socket, Transfer},
};
use crate::io::{self as operation, Value};
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};

fn canceled() -> io::Error {
    io::Error::from_raw_os_error(libc::ECANCELED)
}

struct TransferFuture<'a, T: Transfer> {
    socket: &'a Socket,
    buffer: Option<Buffer>,
    pending: Option<Pending<'a, T>>,
    cancel: bool,
}
impl<'a, T: Transfer> TransferFuture<'a, T> {
    fn new(socket: &'a Socket, buffer: Buffer) -> Self {
        Self {
            socket,
            buffer: Some(buffer),
            pending: None,
            cancel: false,
        }
    }
    fn request_cancel(&mut self) {
        self.cancel = true;
        if let Some(pending) = &mut self.pending {
            pending.request_cancel();
        }
    }
}
impl<T: Transfer> Future for TransferFuture<'_, T> {
    type Output = (io::Result<usize>, Buffer);
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(buffer) = this.buffer.take() {
            let check = if this.cancel {
                Err(canceled())
            } else {
                this.socket.check().and_then(|()| {
                    if T::WRITE && this.socket.write_closed.get() {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "write direction closed",
                        ))
                    } else if buffer.len() > this.socket.config.max_buffer_size {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "TCP buffer exceeds configured limit",
                        ))
                    } else {
                        Ok(())
                    }
                })
            };
            if let Err(error) = check {
                return Poll::Ready((Err(error), buffer));
            }
            if buffer.is_empty() {
                return Poll::Ready((Ok(0), buffer));
            }
            match this.socket.run(T::new(this.socket.file.clone(), buffer)) {
                Ok(pending) => this.pending = Some(pending),
                Err(rejected) => {
                    return Poll::Ready((Err(rejected.error), rejected.operation.into_buffer()));
                }
            }
        }
        let completion =
            ready!(Pin::new(this.pending.as_mut().expect("operation already consumed")).poll(cx));
        this.pending = None;
        Poll::Ready((
            completion.result.map(Value::into_count),
            completion.operation.into_buffer(),
        ))
    }
}
macro_rules! transfer_future {
    ($name:ident, $operation:ty) => {
        /// A lazy owned-buffer operation. Drop requests cancellation without closing the socket.
        #[must_use = "operations do nothing until polled"]
        pub struct $name<'a>(TransferFuture<'a, $operation>);
        impl<'a> $name<'a> {
            pub(super) fn new(socket: &'a Socket, buffer: Buffer) -> Self {
                Self(TransferFuture::new(socket, buffer))
            }
            /// Request cancellation and await the original terminal result and buffer.
            /// Completion may win the race. Cancellation publication is best effort;
            /// if it fails, this waits for natural completion, possibly indefinitely.
            /// Dropping this future abandons the result.
            pub async fn cancel(mut self) -> (io::Result<usize>, Buffer) {
                self.0.request_cancel();
                self.await
            }
        }
        impl Future for $name<'_> {
            type Output = (io::Result<usize>, Buffer);
            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                Pin::new(&mut self.get_mut().0).poll(cx)
            }
        }
    };
}
transfer_future!(Read, operation::Recv);
transfer_future!(Write, operation::Send);

/// A lazy accept. Drop requests cancellation and leaves the listener usable.
#[must_use = "operations do nothing until polled"]
pub struct Accept<'a> {
    socket: &'a Socket,
    pending: Option<Pending<'a, operation::Accept>>,
    started: bool,
    cancel: bool,
}
impl<'a> Accept<'a> {
    pub(super) fn new(socket: &'a Socket) -> Self {
        Self {
            socket,
            pending: None,
            started: false,
            cancel: false,
        }
    }
    /// Attempt cancellation and await the original result. A connection that wins
    /// the race is returned. Failed cancellation may leave this waiting indefinitely.
    pub async fn cancel(mut self) -> io::Result<TcpStream> {
        self.cancel = true;
        if let Some(pending) = &mut self.pending {
            pending.request_cancel();
        }
        self.await
    }
}
impl Future for Accept<'_> {
    type Output = io::Result<TcpStream>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if !this.started {
            this.started = true;
            if this.cancel {
                return Poll::Ready(Err(canceled()));
            }
            this.socket.check()?;
            this.pending = Some(
                this.socket
                    .run(operation::Accept {
                        socket: this.socket.file.clone(),
                    })
                    .map_err(|r| r.error)?,
            );
        }
        let completion =
            ready!(Pin::new(this.pending.as_mut().expect("operation already consumed")).poll(cx));
        this.pending = None;
        Poll::Ready(match completion.result? {
            Value::File(file) => Ok(TcpStream {
                socket: Socket::new(&this.socket.io, file, this.socket.config),
            }),
            _ => unreachable!("accept returns a descriptor"),
        })
    }
}

/// A lazy sequence of sends. Errors/cancellation may follow partial progress;
/// the returned buffer is whole, and the result does not report bytes already sent.
#[must_use = "operations do nothing until polled"]
pub struct WriteAll<'a> {
    socket: &'a Socket,
    buffer: Option<Buffer>,
    pending: Option<Pending<'a, operation::SendRange>>,
    offset: usize,
    cancel: bool,
}
impl<'a> WriteAll<'a> {
    pub(super) fn new(socket: &'a Socket, buffer: Buffer) -> Self {
        Self {
            socket,
            buffer: Some(buffer),
            pending: None,
            offset: 0,
            cancel: false,
        }
    }
    /// Cancel the current send, await its terminal result, and issue no further sends.
    /// Success is returned if the full buffer was sent before cancellation took effect.
    /// Cancellation publication is best effort; this may wait indefinitely for
    /// the current send's natural completion if cancellation fails.
    pub async fn cancel(mut self) -> (io::Result<()>, Buffer) {
        self.cancel = true;
        if let Some(pending) = &mut self.pending {
            pending.request_cancel();
        }
        self.await
    }
}
impl Future for WriteAll<'_> {
    type Output = (io::Result<()>, Buffer);
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            if let Some(buffer) = this.buffer.take() {
                let check = if this.cancel {
                    Err(canceled())
                } else {
                    this.socket.check().and_then(|()| {
                        if this.socket.write_closed.get() {
                            Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "write direction closed",
                            ))
                        } else {
                            Ok(())
                        }
                    })
                };
                if let Err(error) = check {
                    return Poll::Ready((Err(error), buffer));
                }
                if this.offset == buffer.len() {
                    return Poll::Ready((Ok(()), buffer));
                }
                let len = (buffer.len() - this.offset).min(this.socket.config.max_buffer_size);
                match this.socket.run(operation::SendRange {
                    socket: this.socket.file.clone(),
                    buffer,
                    range: this.offset..this.offset + len,
                }) {
                    Ok(pending) => this.pending = Some(pending),
                    Err(rejected) => {
                        return Poll::Ready((Err(rejected.error), rejected.operation.buffer));
                    }
                }
            }
            let completion = ready!(
                Pin::new(this.pending.as_mut().expect("operation already consumed")).poll(cx)
            );
            this.pending = None;
            let buffer = completion.operation.buffer;
            match completion.result.map(Value::into_count) {
                Ok(0) => {
                    return Poll::Ready((
                        Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "TCP send returned zero",
                        )),
                        buffer,
                    ));
                }
                Ok(count) => {
                    this.offset += count;
                    if this.offset == buffer.len() {
                        return Poll::Ready((Ok(()), buffer));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Poll::Ready((Err(error), buffer)),
            }
            this.buffer = Some(buffer);
        }
    }
}
