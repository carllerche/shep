//! Positional file operations over the safe io_uring building block.
//! No kernel file cursor, admission backlog, or implicit durability barrier.
#![forbid(unsafe_code)]
use shep_smp_io2::{Buffer, Completion, Io, Operation, OperationHandle, Rejected, Value};
use std::{future::poll_fn, io, sync::Arc};

/// An already-open file bound to a local I/O producer. Clones share its descriptor.
#[derive(Clone)]
pub struct File {
    io: Io,
    file: Arc<std::fs::File>,
}
impl File {
    /// Reactor reference for polling or releasing eager operation handles.
    pub fn io(&self) -> &Io {
        &self.io
    }

    pub fn from_std(io: Io, file: std::fs::File) -> Self {
        Self {
            io,
            file: Arc::new(file),
        }
    }
    /// Admit immediately; the runtime submits the batch. A rejected operation
    /// returns its original buffer. Consume the handle with self.io(), or call
    /// handle.release(self.io()) to abandon its result.
    pub fn submit_write_at(
        &self,
        buffer: Buffer,
        offset: u64,
    ) -> Result<OperationHandle, Rejected> {
        self.io.push(Operation::WriteAt {
            file: self.file.clone(),
            buffer,
            offset,
        })
    }
    pub fn submit_read_at(&self, buffer: Buffer, offset: u64) -> Result<OperationHandle, Rejected> {
        self.io.push(Operation::ReadAt {
            file: self.file.clone(),
            buffer,
            offset,
        })
    }
    pub async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        self.finish_buffer(self.submit_write_at(buffer, offset))
            .await
    }
    pub async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        self.finish_buffer(self.submit_read_at(buffer, offset))
            .await
    }
    async fn finish_buffer(
        &self,
        operation: Result<OperationHandle, Rejected>,
    ) -> (io::Result<usize>, Buffer) {
        match operation {
            Ok(handle) => {
                let mut pending = Pending {
                    io: &self.io,
                    handle,
                };
                let Completion { result, operation } =
                    poll_fn(|cx| pending.handle.poll_complete(pending.io, cx)).await;
                (
                    result.map(Value::into_count),
                    operation.into_buffer().expect("buffer operation"),
                )
            }
            Err(rejected) => (
                Err(rejected.error),
                rejected
                    .operation
                    .into_buffer()
                    .expect("rejected buffer operation"),
            ),
        }
    }
    /// Wait for this sync operation. Callers establish write-completion ordering
    /// explicitly; an unrelated earlier SQE is not automatically a dependency.
    pub async fn sync_data(&self) -> io::Result<()> {
        self.sync(true).await
    }
    pub async fn sync_all(&self) -> io::Result<()> {
        self.sync(false).await
    }
    async fn sync(&self, data_only: bool) -> io::Result<()> {
        let handle = self
            .io
            .push(Operation::Fsync {
                file: self.file.clone(),
                data_only,
            })
            .map_err(|rejected| rejected.error)?;
        let mut pending = Pending {
            io: &self.io,
            handle,
        };
        poll_fn(|cx| pending.handle.poll_complete(pending.io, cx))
            .await
            .result
            .map(|_| ())
    }
}

// The adapter owns cancellation policy and already borrows its Io through File.
// No per-operation Rc clone is needed to release a dropped async operation.
struct Pending<'a> {
    io: &'a Io,
    handle: OperationHandle,
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.handle.release(self.io);
    }
}
