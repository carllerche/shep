//! Core-local asynchronous files. Requests own their buffers and descriptors
//! until kernel completion, including when their waiting futures are dropped.
//! Run on a Tokio LocalRuntime (or LocalSet). Independent requests are unordered.
//! io_uring is required; initialization errors are returned to the caller.
//! Call `shutdown` after closing consumers and before dropping the runtime.
mod buffer;
mod driver;

pub use buffer::Buffer;
pub use driver::{Config, Io, IoStats};

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// File opening policy. Direct I/O is explicit and never silently downgraded.
#[derive(Clone, Copy, Debug)]
pub struct OpenOptions {
    /// Allow reads from the opened descriptor.
    pub read: bool,
    /// Allow writes and length changes through the opened descriptor.
    pub write: bool,
    /// Create the file if absent; retain existing contents.
    pub create: bool,
    /// Require a new file, failing if the path already exists.
    pub create_new: bool,
    /// Discard existing contents when opening for writing.
    pub truncate: bool,
    /// Require O_DIRECT and discover its alignment before returning.
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

/// A local file handle. Clones share a descriptor and close gate; operations retain
/// the descriptor through completion. Independent requests have no implicit ordering.
#[derive(Clone)]
pub struct File {
    /// Local driver retaining accepted work through completion.
    io: Io,
    /// Shared descriptor ownership, also retained by each accepted request.
    handle: Arc<std::fs::File>,
    /// Original open path for diagnostics; unaffected by later renames.
    path: Arc<PathBuf>,
    /// Required buffer-address, length, and offset alignment in bytes.
    alignment: usize,
    /// Close gate and outstanding-operation count shared by all clones.
    activity: Arc<driver::Activity>,
}

/// The small interface implemented by raw and scheduled files. Reads and writes
/// may complete short. Metadata mutation requires caller-managed exclusion.
#[allow(async_fn_in_trait)]
pub trait FileIo: Clone + 'static {
    /// Return the required alignment of buffer addresses, lengths, and offsets.
    /// Buffered files return one.
    fn alignment(&self) -> usize;
    /// Read at an explicit offset and return the owned buffer, including on error.
    /// A short result may indicate EOF; independent operations have no ordering.
    async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer);
    /// Write at an explicit offset and return the owned buffer, including on error.
    /// Short writes are allowed; successful completion does not establish durability.
    async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer);
    /// Make completed writes and the metadata needed to read them durable.
    /// Await dependent writes first; submitting them before this call is insufficient.
    async fn sync_data(&self) -> io::Result<()>;
    /// Make completed file changes, including metadata, durable.
    /// Await dependent operations first; directory entries need a separate directory sync.
    async fn sync_all(&self) -> io::Result<()>;
    /// Reserve backing space without changing logical length.
    /// The caller excludes conflicting writes, syncs, and metadata changes.
    async fn allocate(&self, offset: u64, len: u64) -> io::Result<()>;
    /// Set logical length, discarding data beyond a smaller end.
    /// The caller excludes conflicting operations and synchronizes when durability is needed.
    async fn set_len(&self, len: u64) -> io::Result<()>;
    /// Read logical file length, which may include preallocation exposed by `set_len`.
    async fn len(&self) -> io::Result<u64>;
    /// Report whether logical file length is zero.
    async fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len().await? == 0)
    }
}

impl Io {
    /// Open through the bounded metadata helpers and retain the descriptor in a local handle.
    /// Direct I/O remains explicit and its alignment is discovered before returning.
    pub async fn open(&self, path: impl Into<PathBuf>, options: OpenOptions) -> io::Result<File> {
        let path = path.into();
        let p = path.clone();
        let (handle, alignment) = self
            .blocking(move || {
                let handle = std::fs::OpenOptions::new()
                    .read(options.read)
                    .write(options.write)
                    .create(options.create)
                    .create_new(options.create_new)
                    .truncate(options.truncate)
                    .custom_flags(if options.direct { libc::O_DIRECT } else { 0 })
                    .open(p)?;
                let alignment = if options.direct {
                    direct_alignment(&handle)?
                } else {
                    1
                };
                Ok((handle, alignment))
            })
            .await?;
        Ok(File {
            io: self.clone(),
            handle: Arc::new(handle),
            path: Arc::new(path),
            alignment,
            activity: Arc::default(),
        })
    }

    /// Create missing directories through the bounded metadata helpers.
    pub async fn create_dir_all(&self, path: PathBuf) -> io::Result<()> {
        self.blocking(move || std::fs::create_dir_all(path)).await
    }
    /// Collect directory entry paths through the bounded metadata helpers.
    pub async fn read_dir(&self, path: PathBuf) -> io::Result<Vec<PathBuf>> {
        self.blocking(move || {
            std::fs::read_dir(path)?
                .map(|e| e.map(|e| e.path()))
                .collect()
        })
        .await
    }
    /// Unlink a file; synchronize its directory separately to persist the removal.
    pub async fn remove_file(&self, path: PathBuf) -> io::Result<()> {
        self.blocking(move || std::fs::remove_file(path)).await
    }
    /// Rename a path; synchronize affected directories separately for durability.
    pub async fn rename(&self, from: PathBuf, to: PathBuf) -> io::Result<()> {
        self.blocking(move || std::fs::rename(from, to)).await
    }
    /// Persist directory entry changes through the bounded metadata helpers.
    pub async fn sync_dir(&self, path: PathBuf) -> io::Result<()> {
        self.blocking(move || std::fs::File::open(path)?.sync_all())
            .await
    }
}

impl File {
    /// Return the path recorded at open time; later renames do not update it.
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Stop operations on all clones and wait for this file's accepted work.
    /// This does not synchronize data or wait for unrelated files.
    pub async fn close(self) -> io::Result<()> {
        self.activity.close().await;
        Ok(())
    }
    /// Reject requests the direct-I/O alignment or kernel result width cannot represent.
    fn validate(&self, buffer: &Buffer, offset: u64) -> io::Result<()> {
        let a = self.alignment;
        if !offset.is_multiple_of(a as u64)
            || !buffer.len().is_multiple_of(a)
            || !(buffer.as_ptr() as usize).is_multiple_of(a)
            || buffer.len() > i32::MAX as usize
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unaligned or oversized I/O",
            ));
        }
        Ok(())
    }
}

impl FileIo for File {
    fn alignment(&self) -> usize {
        self.alignment
    }
    async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        if let Err(e) = self.validate(&buffer, offset) {
            return (Err(e), buffer);
        }
        self.io
            .data(
                driver::Kind::Read,
                self.handle.clone(),
                self.activity.clone(),
                buffer,
                offset,
            )
            .await
    }
    async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        if let Err(e) = self.validate(&buffer, offset) {
            return (Err(e), buffer);
        }
        self.io
            .data(
                driver::Kind::Write,
                self.handle.clone(),
                self.activity.clone(),
                buffer,
                offset,
            )
            .await
    }
    async fn sync_data(&self) -> io::Result<()> {
        self.io
            .sync(self.handle.clone(), self.activity.clone(), true)
            .await
    }
    async fn sync_all(&self) -> io::Result<()> {
        self.io
            .sync(self.handle.clone(), self.activity.clone(), false)
            .await
    }
    async fn set_len(&self, len: u64) -> io::Result<()> {
        let f = self.handle.clone();
        let guard = self.activity.begin()?;
        self.io
            .blocking(move || {
                let _guard = guard;
                f.set_len(len)
            })
            .await
    }
    async fn len(&self) -> io::Result<u64> {
        let f = self.handle.clone();
        let guard = self.activity.begin()?;
        self.io
            .blocking(move || {
                let _guard = guard;
                Ok(f.metadata()?.len())
            })
            .await
    }
    async fn allocate(&self, offset: u64, len: u64) -> io::Result<()> {
        let f = self.handle.clone();
        let guard = self.activity.begin()?;
        let offset =
            i64::try_from(offset).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let len = i64::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        self.io
            .blocking(move || {
                let _guard = guard;
                // Reserve blocks without changing logical length. The caller coordinates set_len.
                if unsafe { libc::fallocate(f.as_raw_fd(), libc::FALLOC_FL_KEEP_SIZE, offset, len) }
                    < 0
                {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            })
            .await
    }
}

/// Combine kernel-reported memory and offset constraints into one alignment.
/// Use a conservative filesystem fallback when the query is unavailable.
fn direct_alignment(file: &std::fs::File) -> io::Result<usize> {
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_DIOALIGN,
            &mut stat,
        )
    };
    if result == 0 && stat.stx_mask & libc::STATX_DIOALIGN != 0 {
        let alignment = stat.stx_dio_mem_align.max(stat.stx_dio_offset_align) as usize;
        if alignment == 0 || !alignment.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file has no supported direct-I/O alignment",
            ));
        }
        Ok(alignment)
    } else {
        // Conservative Linux filesystem fallback when statx DIOALIGN is unavailable.
        Ok(4096)
    }
}
