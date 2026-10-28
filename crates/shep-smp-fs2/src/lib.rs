//! Core-local asynchronous filesystem operations over io2.
//!
//! Transfers own their buffers, expose short results, and reject saturation with
//! WouldBlock. Namespace and metadata helpers use a bounded worker, never the
//! core thread. Callers order writes, syncs and directory syncs explicitly.
//! Close/drain waits for accepted work, including abandoned result waiters; it
//! does not flush appender buffers or imply durability. The runtime must keep
//! driving io2 while these futures wait.
#![forbid(unsafe_code)]
mod activity;
mod helper;

use activity::{Activity, Guard};
pub use shep_smp_io2::Buffer;
use shep_smp_io2::{Completion, Fd, Io, Operation, OperationHandle, Rejected, Value};
use std::{
    cell::RefCell,
    ffi::CString,
    future::poll_fn,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

/// Limits for the filesystem's lazily started metadata worker.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Maximum queued helper jobs, in addition to one executing job.
    /// Full queues reject with WouldBlock; rejected work has no effects.
    pub metadata_queue: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self { metadata_queue: 64 }
    }
}

/// File opening policy. Direct I/O is explicit and never silently downgraded.
#[derive(Clone, Copy, Debug)]
pub struct OpenOptions {
    pub read: bool,
    pub write: bool,
    pub create: bool,
    pub create_new: bool,
    pub truncate: bool,
    pub direct: bool,
}
impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            read: true,
            write: true,
            create: false,
            create_new: false,
            truncate: false,
            direct: false,
        }
    }
}
impl OpenOptions {
    fn flags(self) -> io::Result<i32> {
        if (!self.read && !self.write)
            || (!self.write && (self.create || self.create_new || self.truncate))
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut flags = match (self.read, self.write) {
            (true, true) => libc::O_RDWR,
            (false, true) => libc::O_WRONLY,
            _ => libc::O_RDONLY,
        };
        if self.create || self.create_new {
            flags |= libc::O_CREAT;
        }
        if self.create_new {
            flags |= libc::O_EXCL;
        }
        // Match std's exclusive-create behavior: existing files are never truncated.
        if self.truncate && !self.create_new {
            flags |= libc::O_TRUNC;
        }
        if self.direct {
            flags |= libc::O_DIRECT;
        }
        Ok(flags)
    }
}

struct Domain {
    activity: Arc<Activity>,
    helper: RefCell<Option<helper::Helper>>,
    config: Config,
}
impl Domain {
    fn new(config: Config) -> Rc<Self> {
        Rc::new(Self {
            activity: Arc::default(),
            helper: RefCell::new(None),
            config,
        })
    }
    fn metadata<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<helper::ResultFuture<T>> {
        let guard = self.activity.begin()?;
        let mut helper = self.helper.borrow_mut();
        if helper.is_none() {
            *helper = Some(helper::Helper::new(self.config.metadata_queue)?);
        }
        helper.as_ref().unwrap().run(move || {
            let _guard = guard;
            work()
        })
    }
}

/// A filesystem admission/drain domain bound to one local driver.
/// Clones and opened files share its bounded helper and activity accounting.
#[derive(Clone)]
pub struct Fs {
    io: Io,
    domain: Rc<Domain>,
}
impl Fs {
    pub fn new(io: Io, config: Config) -> io::Result<Self> {
        if config.metadata_queue == 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(Self {
            io,
            domain: Domain::new(config),
        })
    }
    pub fn io(&self) -> &Io {
        &self.io
    }
    /// Open through io2, then discover direct-I/O constraints on the helper.
    /// Like ordinary open, cancellation or a later alignment-query error can
    /// follow creation/truncation. They do not roll back filesystem effects.
    pub async fn open(&self, path: impl Into<PathBuf>, options: OpenOptions) -> io::Result<File> {
        let path = path.into();
        let flags = options.flags()?;
        let name = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let guard = self.domain.activity.begin()?;
        let handle = self
            .io
            .push_with_guard(
                Operation::OpenAt {
                    directory: None,
                    path: name,
                    flags,
                    mode: 0o666,
                },
                guard,
            )
            .map_err(|r| r.error)?;
        let completion = finish(&self.io, handle).await;
        let Value::File(file) = completion.result? else {
            unreachable!("open result")
        };
        let file = Rc::new(file);
        let alignment = if options.direct {
            let query = file.as_file().try_clone()?;
            self.domain
                .metadata(move || direct_alignment(&query))?
                .await?
        } else {
            1
        };
        Ok(File::new(
            self.io.clone(),
            self.domain.clone(),
            file,
            path,
            alignment,
        ))
    }
    /// Import a descriptor after querying its flags and, for O_DIRECT, alignment.
    /// Do not retain aliases that change flags or perform conflicting I/O.
    pub async fn from_std(&self, file: std::fs::File) -> io::Result<File> {
        let (file, alignment) = self
            .domain
            .metadata(move || {
                let flags = rustix::fs::fcntl_getfl(&file)?;
                let alignment = if flags.contains(rustix::fs::OFlags::DIRECT) {
                    direct_alignment(&file)?
                } else {
                    1
                };
                Ok((file, alignment))
            })?
            .await?;
        Ok(File::new(
            self.io.clone(),
            self.domain.clone(),
            Rc::new(Fd::new(file)?),
            PathBuf::new(),
            alignment,
        ))
    }
    pub async fn create_dir_all(&self, path: impl Into<PathBuf>) -> io::Result<()> {
        let path = path.into();
        self.domain
            .metadata(move || std::fs::create_dir_all(path))?
            .await
    }
    pub async fn read_dir(&self, path: impl Into<PathBuf>) -> io::Result<Vec<PathBuf>> {
        let path = path.into();
        self.domain
            .metadata(move || {
                std::fs::read_dir(path)?
                    .map(|e| e.map(|e| e.path()))
                    .collect()
            })?
            .await
    }
    pub async fn remove_file(&self, path: impl Into<PathBuf>) -> io::Result<()> {
        let path = path.into();
        self.domain
            .metadata(move || std::fs::remove_file(path))?
            .await
    }
    pub async fn rename(&self, from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> io::Result<()> {
        let (from, to) = (from.into(), to.into());
        self.domain
            .metadata(move || std::fs::rename(from, to))?
            .await
    }
    /// Persist namespace changes; call after the dependent rename/create/unlink.
    pub async fn sync_dir(&self, path: impl Into<PathBuf>) -> io::Result<()> {
        let path = path.into();
        self.domain
            .metadata(move || {
                let fd = rustix::fs::open(
                    path,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )?;
                rustix::fs::fsync(fd).map_err(Into::into)
            })?
            .await
    }
    /// Wait for this domain's accepted kernel and helper work, without syncing.
    /// Quiesce producers first for a stable drain. Other driver users are unaffected.
    pub async fn drain(&self) {
        self.domain.activity.idle().await;
    }
    /// Stop domain admission (including opened files), then finish accepted work.
    /// The shared io2 driver remains open for other filesystem/network domains.
    pub async fn shutdown(&self) {
        self.domain.activity.close();
        self.drain().await;
        self.domain.helper.borrow_mut().take();
    }
}

/// Positional file contract shared by raw files, schedulers and appenders.
/// Metadata mutation requires caller-managed exclusion. Independent requests
/// have no implicit ordering; await writes before syncing their durability.
#[allow(async_fn_in_trait)]
pub trait FileIo: Clone + 'static {
    fn alignment(&self) -> usize;
    async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer);
    async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer);
    async fn sync_data(&self) -> io::Result<()>;
    async fn sync_all(&self) -> io::Result<()>;
    /// Reserve backing space without changing logical length.
    async fn allocate(&self, offset: u64, len: u64) -> io::Result<()>;
    async fn set_len(&self, len: u64) -> io::Result<()>;
    async fn len(&self) -> io::Result<u64>;
    async fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len().await? == 0)
    }
}

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
    fn new(io: Io, domain: Rc<Domain>, file: Rc<Fd>, path: PathBuf, alignment: usize) -> Self {
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

async fn finish(io: &Io, handle: OperationHandle) -> Completion {
    let mut pending = Pending { io, handle };
    poll_fn(|cx| pending.handle.poll_complete(pending.io, cx)).await
}
struct Pending<'a> {
    io: &'a Io,
    handle: OperationHandle,
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.handle.release(self.io);
    }
}

fn direct_alignment(file: &std::fs::File) -> io::Result<usize> {
    use rustix::fs::{AtFlags, StatxFlags, statx};
    match statx(file, "", AtFlags::EMPTY_PATH, StatxFlags::DIOALIGN) {
        Ok(stat) if stat.stx_mask & StatxFlags::DIOALIGN.bits() != 0 => {
            let memory = stat.stx_dio_mem_align as usize;
            let offset = stat.stx_dio_offset_align as usize;
            if !memory.is_power_of_two() || !offset.is_power_of_two() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported direct-I/O alignment",
                ));
            }
            Ok(memory.max(offset))
        }
        // Match the existing file layer's conservative fallback when the
        // filesystem/kernel cannot report DIOALIGN. Kernel errors remain visible.
        Ok(_)
        | Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::OPNOTSUPP) => {
            Ok(4096)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        sync::mpsc,
        task::{Context, Poll, Waker},
        time::{Duration, Instant},
    };
    fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn bounded_helper_keeps_canceled_work_in_shutdown_and_survives_panics() {
        let driver = shep_smp_io2::Driver::new(2).unwrap();
        let fs = Fs::new(driver.io().clone(), Config { metadata_queue: 1 }).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        let running = fs
            .domain
            .metadata(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(())
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let queued = fs
            .domain
            .metadata(move || {
                completed_tx.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            fs.domain.metadata(|| Ok(())).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(running);
        drop(queued);
        let mut shutdown = Box::pin(fs.shutdown());
        assert!(poll(shutdown.as_mut()).is_pending());
        assert_eq!(
            fs.domain.metadata(|| Ok(())).err().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while poll(shutdown.as_mut()).is_pending() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        completed_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let helper = helper::Helper::new(1).unwrap();
        let mut failed = Box::pin(
            helper
                .run(|| -> io::Result<()> { panic!("helper work failed") })
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Poll::Ready(result) = poll(failed.as_mut()) {
                assert!(result.is_err());
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        // The worker remains usable after a job panic.
        let mut next = Box::pin(helper.run(|| Ok(42)).unwrap());
        loop {
            if let Poll::Ready(result) = poll(next.as_mut()) {
                assert_eq!(result.unwrap(), 42);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
