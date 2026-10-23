#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shep_storage::{
    Disk, HEADER_LEN, KvConfig, KvStore, Log, LogConfig, Offsets, Record, SegmentInfo, WritePool,
};
use tempfile::TempDir;

pub const LOG_ID: u64 = 1;

/// One core's storage in a temporary directory, on the real disk.
pub struct Env {
    pub tmp: TempDir,
    pub disk: Disk,
    pub pool: WritePool,
    pub kv: KvStore,
    pub kv_config: KvConfig,
}

impl Env {
    pub async fn new() -> Env {
        Env::with_pool(8 << 20).await
    }

    pub async fn with_pool(pool_bytes: usize) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let disk = Disk::new().unwrap();
        let kv_config = KvConfig {
            commit_interval: Duration::ZERO,
            ..KvConfig::default()
        };
        let kv = KvStore::open(&disk, tmp.path().join("kvstore"), kv_config.clone())
            .await
            .unwrap();
        Env {
            tmp,
            disk,
            pool: WritePool::new(pool_bytes),
            kv,
            kv_config,
        }
    }

    pub fn log_dir(&self) -> PathBuf {
        self.tmp.path().join("log")
    }

    pub async fn try_open(&self, config: LogConfig) -> shep_storage::Result<Log> {
        Log::open(
            &self.disk,
            &self.pool,
            &self.kv,
            self.log_dir(),
            LOG_ID,
            config,
        )
        .await
    }

    pub async fn open(&self, config: LogConfig) -> Log {
        self.try_open(config).await.unwrap()
    }

    /// Reopens the key-value store from disk, as a restarted process would.
    pub async fn restart_kv(&mut self) {
        self.kv = KvStore::open(
            &self.disk,
            self.tmp.path().join("kvstore"),
            self.kv_config.clone(),
        )
        .await
        .unwrap();
    }

    /// Drops `log` without closing it, as a crash would, then reopens it.
    pub async fn crash(&mut self, log: Log, config: LogConfig) -> Log {
        drop(log);
        self.restart_kv().await;
        self.open(config).await
    }

    pub async fn reopen(&mut self, log: Log, config: LogConfig) -> Log {
        log.close().await.unwrap();
        drop(log);
        self.restart_kv().await;
        self.open(config).await
    }

    pub fn segment_files(&self) -> Vec<PathBuf> {
        files_with(&self.log_dir(), "log")
    }

    pub fn index_files(&self) -> Vec<PathBuf> {
        files_with(&self.log_dir(), "index")
    }
}

pub fn files_with(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        .collect();
    v.sort();
    v
}

/// Small segments and buffers, so tests cross every boundary quickly.
pub fn small() -> LogConfig {
    LogConfig {
        segment_size: 4096,
        prealloc_step: 1024,
        chunk_size: 512,
        idle_write: Duration::from_secs(3600),
        cache_bytes: 0,
    }
}

pub fn payload(index: u64, len: usize) -> Vec<u8> {
    let seed = format!("{index}:");
    seed.bytes().cycle().take(len).collect::<Vec<u8>>()
}

pub fn rec(index: u64, term: u64) -> Record {
    Record::new(index, term, (index % 3) as u8, payload(index, 100))
}

pub fn recs(range: std::ops::Range<u64>, term: u64) -> Vec<Record> {
    range.map(|i| rec(i, term)).collect()
}

/// Bytes a record from `rec` takes on disk.
pub const REC_LEN: u64 = HEADER_LEN as u64 + 100;

pub async fn read_all(log: &Log) -> Vec<Record> {
    log.read(.., usize::MAX).await.unwrap()
}

/// Everything observable about a log, to compare across a reopen.
#[derive(Debug, PartialEq, Eq)]
pub struct State {
    pub start: u64,
    pub next: u64,
    pub segments: Vec<SegmentInfo>,
    pub records: Vec<Record>,
    pub terms: Vec<Option<u64>>,
}

pub async fn state(log: &Log) -> State {
    let Offsets { start, next, .. } = log.offsets();
    State {
        start,
        next,
        segments: log.segments(),
        records: read_all(log).await,
        terms: (start.saturating_sub(1)..next + 1)
            .map(|i| log.term_at(i))
            .collect(),
    }
}

/// Overwrites bytes of a file in place.
pub fn poke(path: &Path, pos: u64, bytes: &[u8]) {
    use std::os::unix::fs::FileExt;
    let f = fs::OpenOptions::new().write(true).open(path).unwrap();
    f.write_all_at(bytes, pos).unwrap();
}

pub fn flip(path: &Path, pos: u64) {
    use std::os::unix::fs::FileExt;
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut b = [0];
    f.read_exact_at(&mut b, pos).unwrap();
    f.write_all_at(&[b[0] ^ 0xff], pos).unwrap();
}

pub fn set_len(path: &Path, len: u64) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(len)
        .unwrap();
}

pub fn file_len(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}
