//! The per-core key-value store, after Redpanda's `kvstore`: small state that
//! must be durable but isn't a log of its own, such as a Raft group's vote, a
//! log's start index, and its clean-shutdown marker.
//!
//! Writes are group-committed. An op is queued, and once the commit window
//! has passed since the first queued op, everything queued goes into one
//! record, is appended and synced, and only then applied to the in-memory
//! map and acknowledged. So a `get` never sees a write that a crash could
//! lose. When the log outgrows its limit the whole map is written as a
//! snapshot and the log is emptied.
//!
//! Layout of the directory:
//!
//! ```text
//! kvstore.log        batches of ops, framed as records (see `record`)
//!                    whose index is the batch's sequence number
//! kvstore.snapshot   the whole map, and the first batch it doesn't include
//! ```
//!
//! A batch's payload is `count: u32` then, per op, `tag: u8` (1 put,
//! 0 delete), `key_len: u32`, the key, and for a put `value_len: u32` and the
//! value; keys carry their key space as a big-endian `u16` prefix. The
//! snapshot is `b"SHKV"`, `version: u32`, `next_seq: u64`, `count: u64`, the
//! `(key, value)` pairs length-prefixed as in a batch, then a crc32c of all
//! of it.
//!
//! There's no background task: whichever caller is waiting drives the
//! commit, and a commit in flight is a shared future any of them can finish,
//! so dropping a waiting future loses nothing.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use shep_futures::FutureExt;
use shep_futures::future::{LocalBoxFuture, Shared};
use tokio::time::Instant;

use crate::disk::{self, Disk, DiskFile};
use crate::error::{Error, Result, corrupt};
use crate::index::Reader;
use crate::record::{self, Parsed, Record};

const LOG: &str = "kvstore.log";
const SNAPSHOT: &str = "kvstore.snapshot";
const SNAPSHOT_MAGIC: &[u8; 4] = b"SHKV";
const SNAPSHOT_VERSION: u32 = 1;
const BATCH_KIND: u8 = 1;

/// A namespace of keys, so users can't collide.
///
/// A number rather than an enum so later users can claim their own without
/// changing this crate; the constants are the ones known so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeySpace(pub u16);

impl KeySpace {
    /// Raft's per-group vote and term.
    pub const RAFT: KeySpace = KeySpace(1);
    /// This crate's per-log start index and clean-shutdown marker.
    pub const STORAGE: KeySpace = KeySpace(2);
    /// Which core each group lives on.
    pub const SHARD_PLACEMENT: KeySpace = KeySpace(3);
    /// For tests.
    pub const TESTING: KeySpace = KeySpace(0xffff);

    fn key(self, key: &[u8]) -> Vec<u8> {
        let mut k = Vec::with_capacity(2 + key.len());
        k.extend_from_slice(&self.0.to_be_bytes());
        k.extend_from_slice(key);
        k
    }
}

#[derive(Clone, Debug)]
pub struct KvConfig {
    /// How long the first queued op waits for others to share its commit.
    /// Zero commits as soon as no commit is in flight.
    pub commit_interval: Duration,
    /// Log size past which the map is snapshotted and the log emptied.
    pub max_log_bytes: u64,
}

impl Default for KvConfig {
    fn default() -> KvConfig {
        KvConfig {
            commit_interval: Duration::from_millis(10),
            max_log_bytes: 16 << 20,
        }
    }
}

/// Counts of what the store has done, for tests and metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KvStats {
    /// Batches committed, each one append and one `fdatasync`.
    pub batches: u64,
    pub snapshots: u64,
    /// Bytes in the log.
    pub log_bytes: u64,
}

/// A core's key-value store. Cheap to clone; all clones share it. Not
/// `Send`: it belongs to its core's thread.
#[derive(Clone)]
pub struct KvStore {
    inner: Rc<Inner>,
}

struct Inner {
    disk: Disk,
    dir: PathBuf,
    config: KvConfig,
    log: Arc<DiskFile>,
    state: RefCell<State>,
}

struct State {
    map: Map,
    pending: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// When the pending ops get committed.
    deadline: Option<Instant>,
    /// Ops queued and committed so far; an op's ticket is its position.
    enqueued: u64,
    committed: u64,
    /// The next batch's sequence number.
    next_seq: u64,
    log_size: u64,
    inflight: Option<Inflight>,
    next_id: u64,
    /// After a failed commit nothing more is accepted: what reached the disk
    /// is unknown.
    poisoned: Option<Error>,
    stats: KvStats,
}

struct Inflight {
    id: u64,
    done: Shared<LocalBoxFuture<'static, Result<()>>>,
    job: Job,
}

enum Job {
    Batch {
        ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        len: u64,
    },
    Snapshot,
}

impl KvStore {
    /// Opens the store in `dir`, creating it if needed, and replays the
    /// snapshot and log. A torn final batch was never acknowledged and is
    /// dropped.
    pub async fn open(disk: &Disk, dir: impl Into<PathBuf>, config: KvConfig) -> Result<KvStore> {
        let dir = dir.into();
        let counters = disk.counters();
        let d = dir.clone();
        let (snapshot, batches, log, log_size) = disk
            .run(move || {
                fs::create_dir_all(&d)?;
                if let Some(parent) = d.parent() {
                    disk::sync_dir(parent, &counters)?;
                }
                disk::remove_optional(&d.join(format!("{SNAPSHOT}.tmp")))?;
                let snapshot = disk::read_optional(&d.join(SNAPSHOT))?;
                let path = d.join(LOG);
                let file = File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&path)?;
                let bytes = fs::read(&path)?;
                let mut pos = 0;
                let mut batches = Vec::new();
                while let Parsed::Record(r, n) = record::parse(&bytes[pos..]) {
                    batches.push(r);
                    pos += n;
                }
                if pos < bytes.len() {
                    tracing::warn!(path = %path.display(), pos, "truncating a torn kvstore batch");
                    file.set_len(pos as u64)?;
                    disk::fdatasync(&file, &counters)?;
                }
                disk::sync_dir(&d, &counters)?;
                Ok((
                    snapshot,
                    batches,
                    Arc::new(DiskFile::new(file, path)),
                    pos as u64,
                ))
            })
            .await?;

        let mut map = BTreeMap::new();
        let mut next_seq = 0;
        if let Some(b) = snapshot {
            (next_seq, map) =
                decode_snapshot(&b).ok_or_else(|| corrupt(dir.join(SNAPSHOT), "bad snapshot"))?;
        }
        for batch in batches {
            if batch.index < next_seq {
                // Already in the snapshot; the log reset didn't happen.
                continue;
            }
            if batch.index != next_seq || batch.kind != BATCH_KIND {
                return Err(corrupt(
                    dir.join(LOG),
                    format!("batch {} where {next_seq} was expected", batch.index),
                ));
            }
            let ops =
                decode_ops(&batch.payload).ok_or_else(|| corrupt(dir.join(LOG), "bad batch"))?;
            for (k, v) in ops {
                apply(&mut map, k, v);
            }
            next_seq += 1;
        }

        Ok(KvStore {
            inner: Rc::new(Inner {
                disk: disk.clone(),
                dir,
                config,
                log,
                state: RefCell::new(State {
                    map,
                    pending: Vec::new(),
                    deadline: None,
                    enqueued: 0,
                    committed: 0,
                    next_seq,
                    log_size,
                    inflight: None,
                    next_id: 0,
                    poisoned: None,
                    stats: KvStats {
                        log_bytes: log_size,
                        ..KvStats::default()
                    },
                }),
            }),
        })
    }

    /// The committed value of `key`.
    pub fn get(&self, ks: KeySpace, key: &[u8]) -> Option<Vec<u8>> {
        self.inner.state.borrow().map.get(&ks.key(key)).cloned()
    }

    /// Every committed key in `ks`, without the key-space prefix, in order.
    pub fn scan(&self, ks: KeySpace) -> Vec<(Vec<u8>, Vec<u8>)> {
        let st = self.inner.state.borrow();
        let prefix = ks.key(&[]);
        st.map
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| (k[2..].to_vec(), v.clone()))
            .collect()
    }

    /// Sets `key`. The op is queued when this is called, in call order; the
    /// future resolves once it is durable. Dropping the future doesn't
    /// cancel the op.
    pub fn put<V: Into<Vec<u8>>>(
        &self,
        ks: KeySpace,
        key: &[u8],
        value: V,
    ) -> impl Future<Output = Result<()>> + use<V> {
        self.write(ks.key(key), Some(value.into()))
    }

    /// Removes `key`, like [`KvStore::put`].
    pub fn delete(&self, ks: KeySpace, key: &[u8]) -> impl Future<Output = Result<()>> + use<> {
        self.write(ks.key(key), None)
    }

    /// Waits for every op queued so far to commit.
    pub fn flush(&self) -> impl Future<Output = Result<()>> + use<> {
        let ticket = self.inner.state.borrow().enqueued.checked_sub(1);
        let this = self.clone();
        async move {
            match ticket {
                Some(ticket) => this.wait(ticket).await,
                None => Ok(()),
            }
        }
    }

    pub fn stats(&self) -> KvStats {
        self.inner.state.borrow().stats
    }

    fn write(
        &self,
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    ) -> impl Future<Output = Result<()>> + use<> {
        let ticket = {
            let mut st = self.inner.state.borrow_mut();
            match &st.poisoned {
                Some(e) => Err(e.clone()),
                None => {
                    if st.pending.is_empty() {
                        st.deadline = Some(Instant::now() + self.inner.config.commit_interval);
                    }
                    st.pending.push((key, value));
                    st.enqueued += 1;
                    Ok(st.enqueued - 1)
                }
            }
        };
        let this = self.clone();
        async move { this.wait(ticket?).await }
    }

    async fn wait(&self, ticket: u64) -> Result<()> {
        enum Step {
            Await(u64, Shared<LocalBoxFuture<'static, Result<()>>>),
            Sleep(Instant),
        }
        loop {
            let step = {
                let mut st = self.inner.state.borrow_mut();
                if st.committed > ticket {
                    return Ok(());
                }
                if let Some(e) = &st.poisoned {
                    return Err(e.clone());
                }
                match (&st.inflight, st.deadline) {
                    (Some(f), _) => Step::Await(f.id, f.done.clone()),
                    (None, Some(d)) if Instant::now() < d => Step::Sleep(d),
                    _ => {
                        self.start_batch(&mut st);
                        continue;
                    }
                }
            };
            match step {
                Step::Await(id, done) => {
                    let result = done.await;
                    self.finish(id, result);
                }
                Step::Sleep(d) => tokio::time::sleep_until(d).await,
            }
        }
    }

    fn start_batch(&self, st: &mut State) {
        let ops = std::mem::take(&mut st.pending);
        st.deadline = None;
        let mut payload = Vec::new();
        payload.extend_from_slice(&(ops.len() as u32).to_le_bytes());
        for (k, v) in &ops {
            payload.push(v.is_some() as u8);
            put_bytes(&mut payload, k);
            if let Some(v) = v {
                put_bytes(&mut payload, v);
            }
        }
        let mut buf = Vec::new();
        Record::new(st.next_seq, 0, BATCH_KIND, payload).encode_into(&mut buf);
        let len = buf.len() as u64;
        let pos = st.log_size;
        let log = self.inner.log.clone();
        let counters = self.inner.disk.counters();
        let done = self.inner.disk.run(move || {
            log.file.write_all_at(&buf, pos)?;
            disk::sync_file(&log, &counters)
        });
        self.set_inflight(st, done, Job::Batch { ops, len });
    }

    fn start_snapshot(&self, st: &mut State) {
        let mut b = Vec::new();
        b.extend_from_slice(SNAPSHOT_MAGIC);
        b.extend_from_slice(&(SNAPSHOT_VERSION).to_le_bytes());
        b.extend_from_slice(&(st.next_seq).to_le_bytes());
        b.extend_from_slice(&(st.map.len() as u64).to_le_bytes());
        for (k, v) in &st.map {
            put_bytes(&mut b, k);
            put_bytes(&mut b, v);
        }
        let crc = crc32c::crc32c(&b);
        b.extend_from_slice(&(crc).to_le_bytes());
        let dir = self.inner.dir.clone();
        let log = self.inner.log.clone();
        let counters = self.inner.disk.counters();
        let done = self.inner.disk.run(move || {
            disk::write_atomic(&dir, SNAPSHOT, &b, &counters)?;
            // Batches before `next_seq` are skipped on replay, so a crash
            // before this reset is harmless.
            log.file.set_len(0)?;
            disk::sync_file(&log, &counters)
        });
        self.set_inflight(st, done, Job::Snapshot);
    }

    fn set_inflight(
        &self,
        st: &mut State,
        done: impl Future<Output = io::Result<()>> + 'static,
        job: Job,
    ) {
        let id = st.next_id;
        st.next_id += 1;
        st.inflight = Some(Inflight {
            id,
            done: async move { done.await.map_err(Error::from) }
                .boxed_local()
                .shared(),
            job,
        });
    }

    /// Applies a finished commit, once, whoever observes it first.
    fn finish(&self, id: u64, result: Result<()>) {
        let mut st = self.inner.state.borrow_mut();
        let st = &mut *st;
        if st.inflight.as_ref().is_none_or(|f| f.id != id) {
            return;
        }
        let job = st.inflight.take().unwrap().job;
        if let Err(e) = result {
            tracing::error!(error = %e, "kvstore commit failed");
            st.poisoned = Some(e);
            return;
        }
        match job {
            Job::Batch { ops, len } => {
                st.committed += ops.len() as u64;
                for (k, v) in ops {
                    apply(&mut st.map, k, v);
                }
                st.next_seq += 1;
                st.log_size += len;
                st.stats.batches += 1;
                if st.log_size > self.inner.config.max_log_bytes {
                    self.start_snapshot(st);
                }
            }
            Job::Snapshot => {
                st.log_size = 0;
                st.stats.snapshots += 1;
            }
        }
        st.stats.log_bytes = st.log_size;
    }
}

fn apply(map: &mut Map, key: Vec<u8>, value: Option<Vec<u8>>) {
    match value {
        Some(v) => {
            map.insert(key, v);
        }
        None => {
            map.remove(&key);
        }
    }
}

fn put_bytes(b: &mut Vec<u8>, v: &[u8]) {
    b.extend_from_slice(&(v.len() as u32).to_le_bytes());
    b.extend_from_slice(v);
}

fn get_bytes(r: &mut Reader<'_>) -> Option<Vec<u8>> {
    let n = r.u32()? as usize;
    Some(r.take(n)?.to_vec())
}

/// Every key's value.
type Map = BTreeMap<Vec<u8>, Vec<u8>>;

type Ops = Vec<(Vec<u8>, Option<Vec<u8>>)>;

fn decode_ops(b: &[u8]) -> Option<Ops> {
    let mut r = Reader(b);
    let n = r.u32()?;
    let mut ops = Vec::new();
    for _ in 0..n {
        let put = r.u8()? == 1;
        let k = get_bytes(&mut r)?.to_vec();
        let v = if put { Some(get_bytes(&mut r)?) } else { None };
        ops.push((k, v));
    }
    r.0.is_empty().then_some(ops)
}

fn decode_snapshot(b: &[u8]) -> Option<(u64, Map)> {
    let (body, crc) = b.split_at_checked(b.len().checked_sub(4)?)?;
    if crc32c::crc32c(body) != u32::from_le_bytes(crc.try_into().ok()?) {
        return None;
    }
    let mut r = Reader(body);
    if r.take(4)? != SNAPSHOT_MAGIC || r.u32()? != SNAPSHOT_VERSION {
        return None;
    }
    let next_seq = r.u64()?;
    let n = r.u64()?;
    let mut map = BTreeMap::new();
    for _ in 0..n {
        let k = get_bytes(&mut r)?.to_vec();
        map.insert(k, get_bytes(&mut r)?);
    }
    r.0.is_empty().then_some((next_seq, map))
}
