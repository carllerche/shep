use crate::io::{Completion, Fd, Io, Operation, OperationHandle, Value};
use crate::net::{Buffer, Config};
use std::{cell::Cell, future::poll_fn, io, rc::Rc};

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
        // this call. Cancellation alone is insufficient when its reserve is full.
        let _ = shep_sys::shutdown(&*self.file, std::net::Shutdown::Both);
    }
    pub(super) async fn run(
        &self,
        operation: Operation,
    ) -> Result<Completion, crate::io::Rejected> {
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
    pub(super) async fn transfer(
        &self,
        buffer: Buffer,
        write: bool,
    ) -> (io::Result<usize>, Buffer) {
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
