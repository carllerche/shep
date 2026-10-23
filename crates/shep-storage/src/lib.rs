//! Per-core storage for Shep's replicated WAL, after Redpanda's: segmented
//! Raft logs of opaque records, and a small key-value store.
//!
//! Shep runs thread-per-core, one current-thread tokio runtime per core, and
//! storage follows suit. Each core owns:
//!
//! - a [`StorageIo`]: scheduled asynchronous files driven by io_uring;
//! - a [`WritePool`]: the budget of write-behind buffer bytes its logs share;
//! - a [`KvStore`]: votes, log start indexes and clean-shutdown markers;
//! - its groups' [`Log`]s.
//!
//! Device capacity is shared across cores; file and log state stay local.
//! `Log`, `KvStore` and `Storage` are
//! `!Send` and must stay on the thread that opened them. [`Storage`] bundles
//! a core's share.

mod appender;
mod cache;
mod error;
mod index;
mod io;
mod kvstore;
mod log;
mod record;

use std::path::{Path, PathBuf};

pub use error::{Error, Result};
pub use io::{IoStats, StorageIo, WritePool};
pub use kvstore::{KeySpace, KvConfig, KvStats, KvStore};
pub use log::{Log, LogConfig, Offsets, SegmentInfo};
pub use record::{HEADER_LEN, Record};

/// Per-core storage policy. Clone the same device domain to coordinate cores on one disk.
#[derive(Clone, Debug)]
pub struct StorageConfig {
    /// Write-behind buffer bytes for all of the core's logs.
    pub write_pool_bytes: usize,
    /// Clones share this physical-device budget across storage cores.
    pub device: shep_smp_io_scheduler::Device,
    /// Capacity limits for this core's io_uring driver and metadata helpers.
    pub io: shep_smp_io::Config,
    /// Use direct I/O for segment data; metadata files remain buffered.
    pub direct: bool,
    /// Group-commit and snapshot policy for this core's metadata store.
    pub kv: KvConfig,
    /// Default policy for logs opened without an explicit override.
    pub log: LogConfig,
}

impl Default for StorageConfig {
    fn default() -> StorageConfig {
        StorageConfig {
            write_pool_bytes: 32 << 20,
            device: Default::default(),
            io: Default::default(),
            direct: true,
            kv: KvConfig::default(),
            log: LogConfig::default(),
        }
    }
}

/// One core's storage, in its own directory:
///
/// ```text
/// kvstore/          the core's KvStore
/// logs/<id>/        each group's Log
/// ```
///
/// Cheap to clone. Not `Send`.
#[derive(Clone)]
pub struct Storage {
    /// Root directory for this core's metadata store and group logs.
    dir: PathBuf,
    /// Local scheduled I/O services retained by opened storage consumers.
    io: StorageIo,
    /// Byte budget shared by all log write-behind chunks on this core.
    pool: WritePool,
    /// Durable namespaced metadata shared by this core's logs.
    kv: KvStore,
    /// Default per-log policy copied into newly opened logs.
    log_config: LogConfig,
}

impl Storage {
    /// Starts the core's completion driver and opens its key-value store.
    pub async fn open(dir: impl Into<PathBuf>, config: StorageConfig) -> Result<Storage> {
        let dir = dir.into();
        if config.write_pool_bytes == 0 || config.write_pool_bytes > u32::MAX as usize {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid write pool capacity",
            )
            .into());
        }
        let io = StorageIo::with_config(config.io, config.device, config.direct)?;
        let kv = KvStore::open(&io, dir.join("kvstore"), config.kv).await?;
        Ok(Storage {
            dir,
            io,
            pool: WritePool::new(config.write_pool_bytes),
            kv,
            log_config: config.log,
        })
    }

    /// Opens group `id`'s log with the default log config.
    pub async fn open_log(&self, id: u64) -> Result<Log> {
        self.open_log_with(id, self.log_config.clone()).await
    }

    /// Open a group log with explicit segment, buffering, and cache settings.
    pub async fn open_log_with(&self, id: u64, config: LogConfig) -> Result<Log> {
        Log::open(&self.io, &self.pool, &self.kv, self.log_dir(id), id, config).await
    }

    /// Return the directory assigned to a group without opening or creating it.
    pub fn log_dir(&self, id: u64) -> PathBuf {
        self.dir.join("logs").join(id.to_string())
    }

    /// Return this core's storage root.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Borrow this core's scheduled storage I/O and diagnostic counters.
    pub fn io(&self) -> &StorageIo {
        &self.io
    }

    /// Borrow the write-buffer budget shared by this core's log appenders.
    pub fn pool(&self) -> &WritePool {
        &self.pool
    }

    /// Call after closing the core's logs and before ending its runtime.
    pub async fn shutdown(&self) -> Result<()> {
        self.kv.flush().await?;
        self.pool.drain().await;
        self.io.shutdown().await?;
        Ok(())
    }

    /// Borrow the durable key-value store used for Raft and storage metadata.
    pub fn kvstore(&self) -> &KvStore {
        &self.kv
    }
}
