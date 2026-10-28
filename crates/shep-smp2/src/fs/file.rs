use crate::fs::{Buffer, pending::finish};
use crate::io::support::activity::{Activity, Guard};
use crate::io::{Completion, Fd, Io, Operation, OperationHandle, Rejected, Value};
use std::{io, rc::Rc, sync::Arc};

struct FileInner {
    descriptor: Rc<Fd>,
    activity: Arc<Activity>,
    alignment: usize,
}
/// File clones share descriptor ownership and one close gate.
#[derive(Clone)]
pub struct File {
    io: Io,
    inner: Rc<FileInner>,
}
impl File {
    pub(super) fn new(io: Io, file: Rc<Fd>, alignment: usize) -> Self {
        Self {
            io,
            inner: Rc::new(FileInner {
                descriptor: file,
                activity: Arc::default(),
                alignment,
            }),
        }
    }
    pub fn alignment(&self) -> usize {
        self.inner.alignment
    }
    fn begin(&self) -> io::Result<Guard> {
        self.inner.activity.begin()
    }
    fn submit_buffer(
        &self,
        buffer: Buffer,
        offset: u64,
        write: bool,
    ) -> Result<OperationHandle, Rejected> {
        let file = self.inner.descriptor.clone();
        let operation = if write {
            Operation::WriteAt {
                file,
                buffer,
                offset,
            }
        } else {
            Operation::ReadAt {
                file,
                buffer,
                offset,
            }
        };
        let guards = match self.begin() {
            Ok(guards) => guards,
            Err(error) => return Err(Rejected { error, operation }),
        };
        let buffer = operation.buffer().unwrap();
        let a = self.alignment();
        if !offset.is_multiple_of(a as u64)
            || !buffer.len().is_multiple_of(a)
            || !buffer.as_ptr().addr().is_multiple_of(a)
        {
            return Err(Rejected {
                error: io::ErrorKind::InvalidInput.into(),
                operation,
            });
        }
        self.io.push_with_guard(operation, guards)
    }
    pub async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        self.finish_buffer(self.submit_buffer(buffer, offset, true))
            .await
    }
    pub async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        self.finish_buffer(self.submit_buffer(buffer, offset, false))
            .await
    }
    async fn finish_buffer(
        &self,
        operation: Result<OperationHandle, Rejected>,
    ) -> (io::Result<usize>, Buffer) {
        match operation {
            Ok(handle) => {
                let Completion { result, operation } = finish(&self.io, handle).await;
                (
                    result.map(Value::into_count),
                    operation.into_buffer().unwrap(),
                )
            }
            Err(rejected) => (
                Err(rejected.error),
                rejected.operation.into_buffer().unwrap(),
            ),
        }
    }
    async fn execute(&self, make: impl FnOnce(Rc<Fd>) -> Operation) -> io::Result<()> {
        let guard = self.begin()?;
        let handle = self
            .io
            .push_with_guard(make(self.inner.descriptor.clone()), guard)
            .map_err(|r| r.error)?;
        finish(&self.io, handle).await.result.map(|_| ())
    }
    pub async fn sync_data(&self) -> io::Result<()> {
        self.execute(|file| Operation::Fsync {
            file,
            data_only: true,
        })
        .await
    }
    pub async fn sync_all(&self) -> io::Result<()> {
        self.execute(|file| Operation::Fsync {
            file,
            data_only: false,
        })
        .await
    }
    /// Reserve space without extending logical length. Exclude conflicting operations.
    pub async fn allocate(&self, offset: u64, len: u64) -> io::Result<()> {
        self.execute(|file| Operation::Fallocate {
            file,
            offset,
            len,
            mode: libc::FALLOC_FL_KEEP_SIZE,
        })
        .await
    }
    pub async fn set_len(&self, len: u64) -> io::Result<()> {
        self.execute(|file| Operation::Ftruncate { file, len })
            .await
    }
    pub async fn len(&self) -> io::Result<u64> {
        let guard = self.begin()?;
        let file = self.inner.descriptor.as_file().try_clone()?;
        self.io
            .shared
            .worker
            .run(move || {
                let _guard = guard;
                Ok(file.metadata()?.len())
            })?
            .await
    }
    pub async fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len().await? == 0)
    }
    /// Stop admission through all clones and await accepted work, including
    /// operations whose futures were dropped. Does not sync or close the descriptor.
    /// Canceling an admitted shutdown leaves the shared gate closed.
    pub async fn shutdown(&self) {
        self.inner.activity.close();
        self.inner.activity.idle().await;
    }
}
