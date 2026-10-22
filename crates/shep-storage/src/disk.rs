//! The per-core disk I/O worker and the per-core write buffer pool.
//!
//! A core's runtime thread must never block, so every file operation runs on
//! a dedicated thread fed by a channel, standing in for Seastar's I/O queue.
//! Jobs run one at a time in submission order, which the write path leans
//! on: a write submitted before an `fdatasync` is on the file by the time the
//! sync runs, and a read submitted after a write sees it.

use std::fs::{self, File};
use std::future::Future;
use std::io;
use std::os::unix::fs::FileExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

type Job = Box<dyn FnOnce() + Send>;

/// A core's disk I/O worker. Cloning shares the worker; it stops when the
/// last clone is dropped and its queue drains.
#[derive(Clone)]
pub struct Disk {
    tx: mpsc::Sender<Job>,
    counters: Arc<Counters>,
}

/// Counts of the I/O a [`Disk`] has done, for tests and metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskStats {
    /// `fdatasync` calls on files. Flushes coalesce, so this grows much more
    /// slowly than the number of flush requests.
    pub fsyncs: u64,
    /// `fsync` calls on directories, after creating, renaming or removing
    /// files.
    pub dir_syncs: u64,
    /// Positional writes.
    pub writes: u64,
    pub bytes_written: u64,
}

#[derive(Default)]
pub(crate) struct Counters {
    fsyncs: AtomicU64,
    dir_syncs: AtomicU64,
    writes: AtomicU64,
    bytes_written: AtomicU64,
}

impl Disk {
    /// Starts a worker thread.
    pub fn new() -> io::Result<Disk> {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("shep-disk".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    // A panicking job fails only its own caller, whose reply
                    // channel is dropped.
                    let _ = catch_unwind(AssertUnwindSafe(job));
                }
            })?;
        Ok(Disk {
            tx,
            counters: Arc::default(),
        })
    }

    /// Runs `f` on the worker after everything submitted before it.
    ///
    /// The job is queued now, not when the future is first polled, so
    /// ordering follows calls to `run`, and dropping the future doesn't
    /// cancel the job.
    pub fn run<T, F>(&self, f: F) -> impl Future<Output = io::Result<T>> + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce() -> io::Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.submit(move || {
            let _ = tx.send(f());
        });
        async move {
            rx.await
                .unwrap_or_else(|_| Err(io::Error::other("disk worker stopped")))
        }
    }

    pub fn stats(&self) -> DiskStats {
        let c = &self.counters;
        DiskStats {
            fsyncs: c.fsyncs.load(Ordering::Relaxed),
            dir_syncs: c.dir_syncs.load(Ordering::Relaxed),
            writes: c.writes.load(Ordering::Relaxed),
            bytes_written: c.bytes_written.load(Ordering::Relaxed),
        }
    }

    /// Queues `f` without waiting for it.
    pub(crate) fn submit(&self, f: impl FnOnce() + Send + 'static) {
        // The worker only stops once every sender is gone, and `self` is one.
        let _ = self.tx.send(Box::new(f));
    }

    pub(crate) async fn open(&self, path: PathBuf, create_new: bool) -> io::Result<Arc<DiskFile>> {
        self.run(move || {
            let file = File::options()
                .read(true)
                .write(true)
                .create_new(create_new)
                .open(&path)?;
            Ok(Arc::new(DiskFile::new(file, path)))
        })
        .await
    }

    /// Writes `buf` at `pos` in the background, then calls `done` with
    /// whether it succeeded. A failure sticks to the file and fails its next
    /// sync.
    pub(crate) fn write(
        &self,
        file: &Arc<DiskFile>,
        pos: u64,
        buf: Vec<u8>,
        done: impl FnOnce(bool) + Send + 'static,
    ) {
        let file = file.clone();
        let counters = self.counters.clone();
        self.submit(move || {
            let result = file.file.write_all_at(&buf, pos);
            counters.writes.fetch_add(1, Ordering::Relaxed);
            counters
                .bytes_written
                .fetch_add(buf.len() as u64, Ordering::Relaxed);
            if let Err(e) = &result {
                file.fail(e);
            }
            done(result.is_ok());
        });
    }

    /// Sets the file's length in the background; a failure sticks to it.
    pub(crate) fn set_len(&self, file: &Arc<DiskFile>, len: u64) {
        let file = file.clone();
        self.submit(move || {
            if let Err(e) = file.file.set_len(len) {
                file.fail(&e);
            }
        });
    }

    /// `fdatasync`s the file, reporting any earlier background failure.
    pub(crate) fn sync(
        &self,
        file: &Arc<DiskFile>,
    ) -> impl Future<Output = io::Result<()>> + use<> {
        let file = file.clone();
        let counters = self.counters.clone();
        self.run(move || sync_file(&file, &counters))
    }

    pub(crate) async fn read_at(
        &self,
        file: &Arc<DiskFile>,
        pos: u64,
        len: usize,
    ) -> io::Result<Bytes> {
        let file = file.clone();
        self.run(move || {
            let mut buf = vec![0; len];
            let mut done = 0;
            while done < len {
                match file.file.read_at(&mut buf[done..], pos + done as u64)? {
                    0 => break,
                    n => done += n,
                }
            }
            buf.truncate(done);
            Ok(Bytes::from(buf))
        })
        .await
    }

    pub(crate) async fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        let dir = dir.to_path_buf();
        let counters = self.counters.clone();
        self.run(move || sync_dir(&dir, &counters)).await
    }

    pub(crate) fn counters(&self) -> Arc<Counters> {
        self.counters.clone()
    }
}

/// A file the worker owns handles to. Background write failures are
/// remembered so the next sync reports them.
pub(crate) struct DiskFile {
    pub(crate) file: File,
    pub(crate) path: PathBuf,
    error: Mutex<Option<(io::ErrorKind, String)>>,
}

impl DiskFile {
    pub(crate) fn new(file: File, path: PathBuf) -> DiskFile {
        DiskFile {
            file,
            path,
            error: Mutex::new(None),
        }
    }

    fn fail(&self, e: &io::Error) {
        let mut error = self.error.lock().unwrap();
        if error.is_none() {
            tracing::error!(path = %self.path.display(), error = %e, "background write failed");
            *error = Some((e.kind(), e.to_string()));
        }
    }

    fn check(&self) -> io::Result<()> {
        match &*self.error.lock().unwrap() {
            None => Ok(()),
            Some((kind, msg)) => Err(io::Error::new(*kind, msg.clone())),
        }
    }
}

pub(crate) fn sync_file(file: &DiskFile, counters: &Counters) -> io::Result<()> {
    file.check()?;
    fdatasync(&file.file, counters)?;
    file.check()
}

pub(crate) fn fdatasync(file: &File, counters: &Counters) -> io::Result<()> {
    file.sync_data()?;
    counters.fsyncs.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Makes creations, renames and removals in `dir` durable. Redpanda skips
/// this; a crash can then lose a whole new segment, or resurrect a deleted
/// one past a truncation.
pub(crate) fn sync_dir(dir: &Path, counters: &Counters) -> io::Result<()> {
    File::open(dir)?.sync_all()?;
    counters.dir_syncs.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Replaces `dir/name` durably: write a temporary file, sync it, rename it
/// over, sync the directory. Runs on the worker.
pub(crate) fn write_atomic(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    counters: &Counters,
) -> io::Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    let file = File::create(&tmp)?;
    file.write_all_at(bytes, 0)?;
    fdatasync(&file, counters)?;
    fs::rename(&tmp, dir.join(name))?;
    sync_dir(dir, counters)
}

pub(crate) fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn remove_optional(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A core's budget of write-behind buffer bytes, shared by all its logs.
///
/// Appends copy into buffers drawn from here and return; a buffer goes back
/// when its write completes. When the disk falls behind the pool empties and
/// appends wait, as with Redpanda's per-shard chunk cache.
#[derive(Clone)]
pub struct WritePool {
    sem: Arc<Semaphore>,
    capacity: usize,
}

impl WritePool {
    pub fn new(capacity: usize) -> WritePool {
        WritePool {
            sem: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes not currently held by a buffer.
    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }

    pub(crate) async fn acquire(&self, bytes: usize) -> OwnedSemaphorePermit {
        let n = bytes.clamp(1, self.capacity) as u32;
        self.sem
            .clone()
            .acquire_many_owned(n)
            .await
            .expect("write pool semaphore is never closed")
    }
}
