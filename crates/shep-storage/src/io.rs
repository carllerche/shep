//! Storage's typed use of the shared I/O infrastructure. There is no global
//! ordering: callers await the file dependencies their persistence protocol needs.
use shep_smp_io::{Buffer, FileIo, Io, OpenOptions};
use shep_smp_io_scheduler::{Class, Device, Scheduler};
use std::cell::Cell;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio::sync::Notify;

pub use shep_append::BufferPool as WritePool;
/// Successful writes and durability operations across this core's storage handles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoStats {
    /// Successful data synchronizations issued through storage files.
    pub fsyncs: u64,
    /// Successful directory synchronizations after namespace changes.
    pub dir_syncs: u64,
    /// Successful physical write calls, including partial-page rewrites.
    pub writes: u64,
    /// Physical bytes written, including alignment padding and rewrites.
    pub bytes_written: u64,
}
/// Local accounting shared by files and accepted transactions, including detached work.
#[derive(Default)]
struct Counters {
    /// Successful I/O totals shared by this core's storage files.
    stats: Cell<IoStats>,
    /// Accepted whole storage transactions still running, including detached ones.
    tasks: Cell<usize>,
    /// Wake drain waiters when an accepted transaction exits.
    changed: Notify,
}
/// One core's storage I/O handles. Files on a shared physical device use a
/// shared Device domain, even when they belong to different cores.
#[derive(Clone)]
pub struct StorageIo {
    /// Local admission and completion stack sharing a physical-device domain.
    scheduler: Scheduler,
    /// Foreground WAL data and synchronization lane.
    writes: Class,
    /// Foreground range-read lane.
    reads: Class,
    /// Buffered metadata and filesystem namespace lane.
    metadata: Class,
    /// Lower-share scan lane so recovery yields to foreground traffic.
    recovery: Class,
    /// Whether segment data opens require O_DIRECT.
    direct: bool,
    /// Per-core accounting retained by files and accepted transactions.
    counters: Rc<Counters>,
}
/// Scheduled descriptor with storage-level accounting; clones share the raw close gate.
#[derive(Clone)]
pub(crate) struct File {
    /// Scheduled descriptor and its default admission class.
    file: shep_smp_io_scheduler::File,
    /// Shared storage accounting, also retained after caller cancellation.
    counters: Rc<Counters>,
}
impl StorageIo {
    /// Create a local driver with default budgets and direct I/O for segment data.
    /// Use `with_config` to share a device budget across cores.
    pub fn new() -> io::Result<Self> {
        Self::with_config(Default::default(), Device::default(), true)
    }
    /// Create WAL, foreground-read, metadata, and lower-share recovery classes.
    /// Clones of `device` coordinate capacity across all storage cores using that disk.
    pub fn with_config(
        config: shep_smp_io::Config,
        device: Device,
        direct: bool,
    ) -> io::Result<Self> {
        let driver = Io::new(config)?;
        let scheduler = Scheduler::new(driver, device, 256)?;
        Ok(Self {
            writes: scheduler.class("wal", 1000)?,
            reads: scheduler.class("reads", 1000)?,
            metadata: scheduler.class("metadata", 1000)?,
            recovery: scheduler.class("recovery", 100)?,
            scheduler,
            direct,
            counters: Rc::default(),
        })
    }
    /// Snapshot successful storage writes and durability operations on this core.
    pub fn stats(&self) -> IoStats {
        self.counters.stats.get()
    }
    /// Wait for accepted storage transactions and scheduled I/O to finish.
    /// Buffered appender heads need dispatch or close before they become driver work.
    pub async fn drain(&self) -> io::Result<()> {
        loop {
            let changed = self.counters.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.counters.tasks.get() == 0 {
                break;
            }
            changed.await;
        }
        self.scheduler.drain().await
    }
    /// Drain storage transactions, stop scheduler admission, and shut down the driver.
    /// Close logs first so their buffered bytes and persistence dependencies are complete.
    pub async fn shutdown(&self) -> io::Result<()> {
        self.drain().await?;
        self.scheduler.shutdown().await
    }
    /// A storage transaction composed of typed asynchronous operations. It is
    /// accepted now and runs independently of its waiter; no blocking closure or
    /// implicit ordering is involved. Tracks whole KV transactions for shutdown.
    pub(crate) fn task<T: 'static, F: Future<Output = io::Result<T>> + 'static>(
        &self,
        future: F,
    ) -> impl Future<Output = io::Result<T>> + use<T, F> {
        self.counters.tasks.set(self.counters.tasks.get() + 1);
        let guard = TaskGuard(self.counters.clone());
        let task = tokio::task::spawn_local(async move {
            let _guard = guard;
            future.await
        });
        async move { task.await.map_err(io::Error::other)? }
    }
    /// Open a buffered metadata file, creating it if absent unless exclusive creation is requested.
    pub(crate) async fn open(&self, path: PathBuf, create_new: bool) -> io::Result<File> {
        self.open_with(
            path,
            OpenOptions {
                create: !create_new,
                create_new,
                ..Default::default()
            },
            false,
        )
        .await
    }
    /// Open segment data using the configured direct-I/O policy.
    /// Existing files must already exist; new segments use exclusive creation.
    pub(crate) async fn open_data(&self, path: PathBuf, create_new: bool) -> io::Result<File> {
        self.open_with(
            path,
            OpenOptions {
                create_new,
                ..Default::default()
            },
            true,
        )
        .await
    }
    /// Bind a descriptor to the metadata or WAL class and attach storage accounting.
    async fn open_with(
        &self,
        path: PathBuf,
        mut options: OpenOptions,
        data: bool,
    ) -> io::Result<File> {
        options.direct = data && self.direct;
        let class = if data { &self.writes } else { &self.metadata };
        let file = class.open(path, options).await?;
        Ok(File {
            file,
            counters: self.counters.clone(),
        })
    }
    /// Create missing directories under the metadata class.
    pub(crate) async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.metadata.create_dir_all(path.to_owned()).await
    }
    /// List directory entries under the metadata class.
    pub(crate) async fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.metadata.read_dir(path.to_owned()).await
    }
    /// Unlink a path idempotently; the caller persists the directory change separately.
    pub(crate) async fn remove(&self, path: &Path) -> io::Result<()> {
        match self.metadata.remove_file(path.to_owned()).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
    /// Persist directory entries and count successful directory synchronizations.
    pub(crate) async fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.metadata.sync_dir(path.to_owned()).await?;
        let mut stats = self.stats();
        stats.dir_syncs += 1;
        self.counters.stats.set(stats);
        Ok(())
    }
    /// Read a buffered metadata file completely, treating only absence as `None`.
    pub(crate) async fn read_optional(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        let file = match self
            .open_with(path.to_owned(), OpenOptions::default(), false)
            .await
        {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            result => result?,
        };
        let len = usize::try_from(file.len().await?)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        Ok(Some(self.read_at(&file, 0, len).await?))
    }
    /// Read a logical byte range with foreground priority, returning fewer bytes at EOF.
    pub(crate) async fn read_at(&self, file: &File, pos: u64, len: usize) -> io::Result<Vec<u8>> {
        self.read_in_class(file, pos, len, &self.reads).await
    }
    /// Read a logical byte range with lower recovery shares so scans yield to foreground work.
    pub(crate) async fn read_recovery(
        &self,
        file: &File,
        pos: u64,
        len: usize,
    ) -> io::Result<Vec<u8>> {
        self.read_in_class(file, pos, len, &self.recovery).await
    }
    /// Translate arbitrary byte ranges into bounded, aligned requests for the chosen class.
    /// Discard alignment padding and preserve short-read/EOF behavior.
    async fn read_in_class(
        &self,
        file: &File,
        pos: u64,
        len: usize,
        class: &Class,
    ) -> io::Result<Vec<u8>> {
        let file = file.file.with_class(class.clone());
        let mut result = Vec::with_capacity(len);
        let alignment = file.alignment();
        while result.len() < len {
            let offset = pos + result.len() as u64;
            let aligned = offset / alignment as u64 * alignment as u64;
            let skip = (offset - aligned) as usize;
            // Keep every scheduler request bounded even for exceptionally large records.
            let want = (len - result.len()).min(1 << 20);
            let size = (want + skip).div_ceil(alignment) * alignment;
            let (n, buffer) = file.read_at(Buffer::new(size, alignment)?, aligned).await;
            let n = match n {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if n <= skip {
                break;
            }
            let take = want.min(n - skip);
            result.extend_from_slice(&buffer[skip..skip + take]);
            if take < want {
                break;
            }
        }
        Ok(result)
    }
    /// Write bytes in bounded requests, retrying interruptions and short writes.
    /// The caller supplies offsets, any direct-I/O alignment, and durability dependencies.
    pub(crate) async fn write_all(
        &self,
        file: &File,
        bytes: &[u8],
        mut pos: u64,
    ) -> io::Result<()> {
        for chunk in bytes.chunks(1 << 20) {
            let mut buffer = Buffer::from_slice(chunk, file.alignment())?;
            while !buffer.is_empty() {
                let (n, returned) = file.write_at(buffer, pos).await;
                buffer = returned;
                let n = match n {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    result => result?,
                };
                if n == 0 {
                    return Err(io::Error::from(io::ErrorKind::WriteZero));
                }
                let len = buffer.len();
                buffer.copy_within(n..len, 0);
                buffer.resize(len - n);
                pos += n as u64;
            }
        }
        Ok(())
    }
    /// Replace a buffered file's contents and synchronize its data before returning.
    /// Persist its directory separately when creation or replacement must survive a crash.
    pub(crate) async fn write_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let file = self
            .open_with(
                path.to_owned(),
                OpenOptions {
                    create: true,
                    truncate: true,
                    ..Default::default()
                },
                false,
            )
            .await?;
        self.write_all(&file, bytes, 0).await?;
        file.sync_data().await
    }
    /// Publish metadata by synchronizing a temporary file, renaming it, then syncing the directory.
    /// The caller serializes replacements using the same temporary name.
    pub(crate) async fn write_atomic(
        &self,
        dir: &Path,
        name: &str,
        bytes: &[u8],
    ) -> io::Result<()> {
        let tmp = dir.join(format!("{name}.tmp"));
        self.write_file(&tmp, bytes).await?;
        self.metadata.rename(tmp, dir.join(name)).await?;
        self.sync_dir(dir).await
    }
}
/// Keeps a complete storage transaction visible to drain after its caller stops waiting.
struct TaskGuard(Rc<Counters>);
impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.tasks.set(self.0.tasks.get() - 1);
        self.0.changed.notify_waiters();
    }
}
impl File {
    /// Return the descriptor's recorded open path for storage diagnostics.
    pub(crate) fn path(&self) -> &Path {
        self.file.path()
    }
}
impl FileIo for File {
    fn alignment(&self) -> usize {
        self.file.alignment()
    }
    async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        self.file.read_at(buffer, offset).await
    }
    /// Count successful physical writes, including alignment padding and partial-page rewrites.
    async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        let (result, buffer) = self.file.write_at(buffer, offset).await;
        if let Ok(n) = result {
            let mut stats = self.counters.stats.get();
            stats.writes += 1;
            stats.bytes_written += n as u64;
            self.counters.stats.set(stats);
        }
        (result, buffer)
    }
    /// Count successful data synchronizations after the scheduled operation completes.
    async fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data().await?;
        let mut stats = self.counters.stats.get();
        stats.fsyncs += 1;
        self.counters.stats.set(stats);
        Ok(())
    }
    async fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all().await
    }
    async fn allocate(&self, offset: u64, len: u64) -> io::Result<()> {
        self.file.allocate(offset, len).await
    }
    async fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len).await
    }
    async fn len(&self) -> io::Result<u64> {
        self.file.len().await
    }
}
