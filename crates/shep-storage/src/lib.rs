//! Per-core storage for Shep's replicated WAL, after Redpanda's: segmented
//! Raft logs of opaque records, and a small key-value store.
//!
//! Shep runs thread-per-core, one current-thread tokio runtime per core, and
//! storage follows suit. Each core owns:
//!
//! - a [`Disk`]: the thread that does the core's blocking file I/O, so the
//!   runtime thread never blocks;
//! - a [`WritePool`]: the budget of write-behind buffer bytes its logs share;
//! - a [`KvStore`]: votes, log start indexes and clean-shutdown markers;
//! - its groups' [`Log`]s.
//!
//! Nothing is shared across cores. `Log`, `KvStore` and `Storage` are
//! `!Send` and must stay on the thread that opened them. [`Storage`] bundles
//! a core's share.

mod appender;
mod cache;
mod disk;
mod error;
mod index;
mod kvstore;
mod log;
mod record;

use std::path::{Path, PathBuf};

pub use disk::{Disk, DiskStats, WritePool};
pub use error::{Error, Result};
pub use kvstore::{KeySpace, KvConfig, KvStats, KvStore};
pub use log::{Log, LogConfig, Offsets, SegmentInfo};
pub use record::{HEADER_LEN, Record};

#[derive(Clone, Debug)]
pub struct StorageConfig {
    /// Write-behind buffer bytes for all of the core's logs.
    pub write_pool_bytes: usize,
    pub kv: KvConfig,
    pub log: LogConfig,
}

impl Default for StorageConfig {
    fn default() -> StorageConfig {
        StorageConfig {
            write_pool_bytes: 32 << 20,
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
    dir: PathBuf,
    disk: Disk,
    pool: WritePool,
    kv: KvStore,
    log_config: LogConfig,
}

impl Storage {
    /// Starts the core's disk worker and opens its key-value store.
    pub async fn open(dir: impl Into<PathBuf>, config: StorageConfig) -> Result<Storage> {
        let dir = dir.into();
        let disk = Disk::new()?;
        let kv = KvStore::open(&disk, dir.join("kvstore"), config.kv).await?;
        Ok(Storage {
            dir,
            disk,
            pool: WritePool::new(config.write_pool_bytes),
            kv,
            log_config: config.log,
        })
    }

    /// Opens group `id`'s log with the default log config.
    pub async fn open_log(&self, id: u64) -> Result<Log> {
        self.open_log_with(id, self.log_config.clone()).await
    }

    pub async fn open_log_with(&self, id: u64, config: LogConfig) -> Result<Log> {
        Log::open(
            &self.disk,
            &self.pool,
            &self.kv,
            self.log_dir(id),
            id,
            config,
        )
        .await
    }

    pub fn log_dir(&self, id: u64) -> PathBuf {
        self.dir.join("logs").join(id.to_string())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn disk(&self) -> &Disk {
        &self.disk
    }

    pub fn pool(&self) -> &WritePool {
        &self.pool
    }

    pub fn kvstore(&self) -> &KvStore {
        &self.kv
    }
}
