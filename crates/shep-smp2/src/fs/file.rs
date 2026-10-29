use crate::fs::Buffer;
use crate::io::{Fallocate, Fd, Fsync, Ftruncate, Io, ReadAt, Value, WriteAt};
use std::{io, rc::Rc};

/// File clones share descriptor ownership. Higher layers coordinate admission,
/// concurrent work, and shutdown; each operation owns its kernel resources.
#[derive(Clone)]
pub struct File {
    io: Io,
    descriptor: Rc<Fd>,
    alignment: usize,
}
impl File {
    pub(super) fn new(io: Io, file: Rc<Fd>, alignment: usize) -> Self {
        Self {
            io,
            descriptor: file,
            alignment,
        }
    }
    pub fn alignment(&self) -> usize {
        self.alignment
    }
    fn validate_buffer(&self, buffer: &Buffer, offset: u64) -> io::Result<()> {
        let a = self.alignment();
        if !offset.is_multiple_of(a as u64)
            || !buffer.len().is_multiple_of(a)
            || !buffer.as_ptr().addr().is_multiple_of(a)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(())
    }
    pub async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        if let Err(error) = self.validate_buffer(&buffer, offset) {
            return (Err(error), buffer);
        }
        let operation = WriteAt {
            file: self.descriptor.clone(),
            buffer,
            offset,
        };
        let op = match self.io.submit(operation) {
            Ok(op) => op,
            Err(rejected) => return (Err(rejected.error), rejected.operation.buffer),
        };
        let completion = op.await;
        (
            completion.result.map(Value::into_count),
            completion.operation.buffer,
        )
    }
    pub async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        if let Err(error) = self.validate_buffer(&buffer, offset) {
            return (Err(error), buffer);
        }
        let operation = ReadAt {
            file: self.descriptor.clone(),
            buffer,
            offset,
        };
        let op = match self.io.submit(operation) {
            Ok(op) => op,
            Err(rejected) => return (Err(rejected.error), rejected.operation.buffer),
        };
        let completion = op.await;
        (
            completion.result.map(Value::into_count),
            completion.operation.buffer,
        )
    }
    pub async fn sync_data(&self) -> io::Result<()> {
        self.io
            .submit(Fsync {
                file: self.descriptor.clone(),
                data_only: true,
            })
            .map_err(|r| r.error)?
            .await
            .result
            .map(|_| ())
    }
    pub async fn sync_all(&self) -> io::Result<()> {
        self.io
            .submit(Fsync {
                file: self.descriptor.clone(),
                data_only: false,
            })
            .map_err(|r| r.error)?
            .await
            .result
            .map(|_| ())
    }
    /// Reserve space without extending logical length. Exclude conflicting operations.
    pub async fn allocate(&self, offset: u64, len: u64) -> io::Result<()> {
        self.io
            .submit(Fallocate {
                file: self.descriptor.clone(),
                offset,
                len,
                mode: libc::FALLOC_FL_KEEP_SIZE,
            })
            .map_err(|r| r.error)?
            .await
            .result
            .map(|_| ())
    }
    pub async fn set_len(&self, len: u64) -> io::Result<()> {
        self.io
            .submit(Ftruncate {
                file: self.descriptor.clone(),
                len,
            })
            .map_err(|r| r.error)?
            .await
            .result
            .map(|_| ())
    }
    pub async fn len(&self) -> io::Result<u64> {
        let file = self.descriptor.as_file().try_clone()?;
        self.io
            .shared
            .worker
            .run(move || Ok(file.metadata()?.len()))?
            .await
    }
    pub async fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len().await? == 0)
    }
}
