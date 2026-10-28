use crate::{
    Buffer, Config, FileIo,
    activity::{Activity, Guard},
    domain::Domain,
    pending::finish,
};
use shep_smp_io2::{Completion, Fd, Io, Operation, OperationHandle, Rejected, Value};
use std::{
    io,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

struct FileInner {
    descriptor: Rc<Fd>,
    activity: Arc<Activity>,
    path: PathBuf,
    alignment: usize,
}
/// File clones share descriptor ownership and one close gate.
#[derive(Clone)]
pub struct File {
    io: Io,
    domain: Rc<Domain>,
    inner: Rc<FileInner>,
}
impl File {
    pub(super) fn new(
        io: Io,
        domain: Rc<Domain>,
        file: Rc<Fd>,
        path: PathBuf,
        alignment: usize,
    ) -> Self {
        Self {
            io,
            domain,
            inner: Rc::new(FileInner {
                descriptor: file,
                activity: Arc::default(),
                path,
                alignment,
            }),
        }
    }
    pub fn io(&self) -> &Io {
        &self.io
    }
    /// Import a buffered descriptor without starting a helper. For O_DIRECT use
    /// Fs::from_std instead. Validates io2 descriptor eligibility synchronously.
    /// Aliases must not change descriptor flags.
    pub fn from_std(io: Io, file: std::fs::File) -> io::Result<Self> {
        Ok(Self::new(
            io,
            Domain::new(Config::default()),
            std::rc::Rc::new(Fd::new(file)?),
            PathBuf::new(),
            1,
        ))
    }
    /// Original open path; empty for imported descriptors, unchanged by rename.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
    pub fn alignment(&self) -> usize {
        self.inner.alignment
    }
    fn begin(&self) -> io::Result<(Guard, Guard)> {
        Ok((self.domain.activity.begin()?, self.inner.activity.begin()?))
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
    /// Eager admission. Consume or release the returned handle using self.io().
    /// File/domain activity survives handle release until the kernel completes.
    pub fn submit_write_at(
        &self,
        buffer: Buffer,
        offset: u64,
    ) -> Result<OperationHandle, Rejected> {
        self.submit_buffer(buffer, offset, true)
    }
    pub fn submit_read_at(&self, buffer: Buffer, offset: u64) -> Result<OperationHandle, Rejected> {
        self.submit_buffer(buffer, offset, false)
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
        self.domain
            .metadata(move || {
                let _guard = guard;
                Ok(file.metadata()?.len())
            })?
            .await
    }
    pub async fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len().await? == 0)
    }
    /// Stop new operations through all clones and await accepted work, without
    /// syncing. Clones retain the descriptor until dropped, including this
    /// consuming clone until close returns. Completed unconsumed results may
    /// also retain it. Canceling close leaves the shared gate closed.
    pub async fn close(self) -> io::Result<()> {
        self.inner.activity.close();
        self.inner.activity.idle().await;
        Ok(())
    }
}
impl FileIo for File {
    fn alignment(&self) -> usize {
        File::alignment(self)
    }
    async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        File::read_at(self, buffer, offset).await
    }
    async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        File::write_at(self, buffer, offset).await
    }
    async fn sync_data(&self) -> io::Result<()> {
        File::sync_data(self).await
    }
    async fn sync_all(&self) -> io::Result<()> {
        File::sync_all(self).await
    }
    async fn allocate(&self, offset: u64, len: u64) -> io::Result<()> {
        File::allocate(self, offset, len).await
    }
    async fn set_len(&self, len: u64) -> io::Result<()> {
        File::set_len(self, len).await
    }
    async fn len(&self) -> io::Result<u64> {
        File::len(self).await
    }
}
