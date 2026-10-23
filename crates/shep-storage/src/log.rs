//! A Raft group's log: a directory of segment files, after Redpanda's
//! `io_log_impl`.
//!
//! ```text
//! <base:020>-<term>.log     records, starting with index `base`, all of
//!                           term `term`
//! <base:020>-<term>.index   the segment's sparse index (see `index`)
//! ```
//!
//! A segment holds one term, so the term of any entry comes from segment
//! metadata. A new segment starts when the term changes, when the active one
//! passes the size limit, and on the first append after opening: the
//! segments found on io are never appended to again, only truncated. That
//! keeps a closed segment's `.index` (written when it is closed or
//! truncated) trustworthy.
//!
//! Positions, each one past the last index it covers:
//!
//! - appended: buffered in memory, readable
//! - written: all record bytes have completed writing
//! - flushed: `fdatasync`ed, so it survives a crash
//!
//! Durable state outside the directory lives in the core's [`KvStore`]: the
//! log's start index, set by prefix truncation, and a marker that the log was
//! closed cleanly, which lets the next open trust the last segment's index
//! instead of rescanning it.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use shep_futures::FutureExt;
use shep_futures::future::{LocalBoxFuture, Shared};
use tokio::sync::watch;

use crate::appender::{Appender, AppenderConfig};
use crate::cache::Cache;
use crate::error::{Error, Result, corrupt};
use crate::index::{self, INDEX_INTERVAL, SegmentMeta, SparseIndex};
use crate::io::{File, StorageIo, WritePool};
use crate::kvstore::{KeySpace, KvStore};
use crate::record::{self, HEADER_LEN, Parsed, Record};
use shep_smp_io::FileIo;

/// The least and most a read asks of the disk at once; a record larger than
/// the most is read whole.
const READ_MIN: usize = 64 << 10;
const READ_MAX: usize = 8 << 20;

/// Per-log segment, write-behind, and read-cache policy; device budgets live in StorageIo.
#[derive(Clone, Debug)]
pub struct LogConfig {
    /// Logical record bytes past which the active segment rolls to a new file.
    pub segment_size: u64,
    /// Byte increment reserved ahead of segment data; zero disables proactive reservation.
    pub prealloc_step: u64,
    /// Write-behind chunk size in bytes, and so the size of most writes.
    pub chunk_size: usize,
    /// How long a partly filled buffer waits for more appends before it is
    /// written anyway.
    pub idle_write: Duration,
    /// Bytes of recently appended records kept in memory for reads.
    pub cache_bytes: usize,
}

impl Default for LogConfig {
    fn default() -> LogConfig {
        LogConfig {
            segment_size: 128 << 20,
            prealloc_step: 2 << 20,
            chunk_size: 64 << 10,
            idle_write: Duration::from_millis(100),
            cache_bytes: 1 << 20,
        }
    }
}

/// The log's positions; each is one past the last index it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offsets {
    /// The first readable index.
    pub start: u64,
    /// Appended: the next index to append.
    pub next: u64,
    /// All records before this index have completed writing, without a durability guarantee.
    pub written: u64,
    /// All records before this index are covered by a completed synchronization.
    pub flushed: u64,
}

/// A segment, for inspection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentInfo {
    /// First record index, also used in the segment filename.
    pub base: u64,
    /// Raft term assigned to the segment's records.
    pub term: u64,
    /// One past the last index in it.
    pub end: u64,
    /// Bytes of records.
    pub size: u64,
}

/// A Raft group's log. Cheap to clone; all clones share it. Not `Send`: it
/// belongs to its core's thread and draws on that core's [`StorageIo`],
/// [`WritePool`] and [`KvStore`].
///
/// Dropping the last clone without [`Log::close`] abandons whatever wasn't
/// written yet, as a crash would.
#[derive(Clone)]
pub struct Log {
    /// Shared local segment catalog, appender, and mutation lock.
    inner: Rc<Inner>,
}

/// Shared core-local log ownership; the mutation lock spans asynchronous file transitions.
struct Inner {
    /// Raft group ID used to namespace persistent log metadata.
    id: u64,
    /// Directory containing this group's segment and index files.
    dir: PathBuf,
    /// Per-log segment, buffering, and cache policy.
    config: LogConfig,
    /// Core-local I/O services shared with other logs.
    io: StorageIo,
    /// Durable metadata store containing this log's retained-prefix boundary.
    kv: KvStore,
    /// Translate active byte writes into complete-record progress.
    appender: Appender,
    /// Record watermark derived from the byte appender's contiguous prefix.
    written: Arc<AtomicU64>,
    /// Publish exclusive durable record ends to flush subscribers.
    flushed_tx: watch::Sender<u64>,
    /// Serializes appends, truncations and closing.
    lock: tokio::sync::Mutex<()>,
    /// Local logical catalog; the mutation lock protects transitions across awaits.
    state: RefCell<State>,
}

/// Logical record state and segment catalog; epochs invalidate reads/syncs across truncation.
struct State {
    /// First readable record index after prefix truncation.
    start: u64,
    /// Next record index to append, including buffered records.
    next: u64,
    /// Exclusive record end covered by a completed synchronization.
    flushed: u64,
    /// By base index. Only the last can be active.
    segments: BTreeMap<u64, Segment>,
    /// Recent accepted records available without disk reads.
    cache: Cache,
    /// Bumped by truncations, so a read or sync that straddles one doesn't
    /// trust what it saw before.
    epoch: u64,
    /// A suffix truncation is changing files ahead of the segments that
    /// describe them; reads wait until they agree again.
    truncating: bool,
    /// The sync that flushes wait on; at most one at a time.
    inflight: Option<InflightSync>,
    /// Next flush ID used to apply shared completion at most once.
    next_sync_id: u64,
    /// Reject new mutations after close begins.
    closed: bool,
}

/// An open data descriptor and its record range; only the final segment may accept appends.
struct Segment {
    /// First record index and segment filename key.
    base: u64,
    /// Raft term shared by records in this segment.
    term: u64,
    /// One past the last index.
    end: u64,
    /// Bytes of records, including any still buffered.
    size: u64,
    /// Scheduled data descriptor retained for reads and persistence operations.
    file: File,
    /// Sparse record-start offsets into this segment file.
    index: SparseIndex,
    /// This final segment is still attached to the byte appender.
    active: bool,
}

/// Marks a suffix truncation in progress for as long as it lives.
struct Truncating<'a>(&'a RefCell<State>);

impl<'a> Truncating<'a> {
    /// Block disk reads from observing a suffix truncation while its segment metadata changes.
    fn start(state: &'a RefCell<State>) -> Truncating<'a> {
        state.borrow_mut().truncating = true;
        Truncating(state)
    }
}

impl Drop for Truncating<'_> {
    fn drop(&mut self) {
        self.0.borrow_mut().truncating = false;
    }
}

/// Coalesced flush completion for a record prefix within one truncation epoch.
struct InflightSync {
    /// Flush ID used to recognize completion already applied by another waiter.
    id: u64,
    /// Shared persistence result awaited by concurrent flush callers.
    done: Shared<LocalBoxFuture<'static, Result<()>>>,
    /// One past the last index the sync makes durable.
    upto: u64,
    /// Truncation generation captured when this flush started.
    epoch: u64,
}

/// A segment as recovery leaves it.
struct Recovered {
    /// First record index parsed from the segment filename.
    base: u64,
    /// Raft term parsed from the filename and checked against records.
    term: u64,
    /// Exclusive record end validated by metadata or sequential recovery scan.
    end: u64,
    /// Validated logical data length in bytes.
    size: u64,
    /// Recovered descriptor retained for the live segment catalog.
    file: File,
    /// Validated or rebuilt sparse record-start offsets.
    index: SparseIndex,
}

fn segment_path(dir: &Path, base: u64, term: u64) -> PathBuf {
    dir.join(format!("{base:020}-{term}.log"))
}

fn index_path(dir: &Path, base: u64, term: u64) -> PathBuf {
    dir.join(format!("{base:020}-{term}.index"))
}

/// Extract a segment base and term only when the expected filename extension matches.
fn parse_name(name: &str, ext: &str) -> Option<(u64, u64)> {
    let (base, term) = name.strip_suffix(ext)?.split_once('-')?;
    Some((base.parse().ok()?, term.parse().ok()?))
}

fn start_key(id: u64) -> Vec<u8> {
    format!("log/{id}/start").into_bytes()
}

fn clean_key(id: u64) -> Vec<u8> {
    format!("log/{id}/clean").into_bytes()
}

fn decode_u64(b: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(b.try_into().ok()?))
}

impl Log {
    /// Opens the log `id` in `dir`, creating it if needed, and recovers it.
    ///
    /// Closed segments are trusted through their `.index` files. The last
    /// segment is rescanned unless the log was closed cleanly, and cut back
    /// to its last good record: a torn or corrupt tail there is what a crash
    /// leaves. Anywhere else it's an error.
    pub async fn open(
        io: &StorageIo,
        pool: &WritePool,
        kv: &KvStore,
        dir: impl Into<PathBuf>,
        id: u64,
        config: LogConfig,
    ) -> Result<Log> {
        let dir = dir.into();
        io.create_dir_all(&dir).await?;
        if let Some(parent) = dir.parent() {
            io.sync_dir(parent).await?;
        }
        let mut names = Vec::new();
        let mut indexes = Vec::new();
        for path in io.read_dir(&dir).await? {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".tmp") {
                io.remove(&path).await?;
            } else if let Some(n) = parse_name(name, ".log") {
                names.push(n);
            } else if let Some(n) = parse_name(name, ".index") {
                indexes.push(n);
            }
        }
        for (base, term) in indexes {
            if !names.contains(&(base, term)) {
                io.remove(&index_path(&dir, base, term)).await?;
            }
        }
        names.sort();

        let start = kv
            .get(KeySpace::STORAGE, &start_key(id))
            .and_then(|b| decode_u64(&b))
            .unwrap_or(0);
        let clean = kv
            .get(KeySpace::STORAGE, &clean_key(id))
            .and_then(|b| decode_u64(&b));

        let mut segments: Vec<Recovered> = Vec::new();
        for (n, &(base, term)) in names.iter().enumerate() {
            if names.get(n + 1).is_some_and(|&(b, _)| b == base) {
                return Err(corrupt(&dir, format!("two segments start at {base}")));
            }
            let last = n + 1 == names.len();
            let trusted = !last || clean == Some(base);
            let rec = recover_segment(io, &dir, base, term, trusted, last).await?;
            let rec = match rec {
                Ok(rec) => rec,
                Err(reason) => return Err(corrupt(segment_path(&dir, base, term), reason)),
            };
            if let Some(prev) = segments.last() {
                if rec.base != prev.end {
                    return Err(corrupt(
                        segment_path(&dir, base, term),
                        format!(
                            "starts at {base} but the previous segment ends at {}",
                            prev.end
                        ),
                    ));
                }
                if rec.term < prev.term {
                    return Err(corrupt(
                        segment_path(&dir, base, term),
                        format!("term {term} follows term {}", prev.term),
                    ));
                }
            }
            segments.push(rec);
        }

        // An empty last segment is one a crash cut off before its first
        // record was written; segments wholly below the start are left from
        // a prefix truncation a crash interrupted.
        let mut doomed = Vec::new();
        doomed.extend(segments.pop_if(|s| s.end == s.base));
        let keep = segments.partition_point(|s| s.end <= start);
        doomed.extend(segments.drain(..keep));
        if !doomed.is_empty() {
            for segment in doomed {
                io.remove(&index_path(&dir, segment.base, segment.term))
                    .await?;
                io.remove(&segment_path(&dir, segment.base, segment.term))
                    .await?;
            }
            io.sync_dir(&dir).await?;
        }
        if let Some(first) = segments.first()
            && first.base > start
        {
            return Err(corrupt(
                &dir,
                format!(
                    "the log starts at {start} but its first segment at {}",
                    first.base
                ),
            ));
        }

        let next = segments.last().map_or(start, |s| s.end).max(start);
        let written = Arc::new(AtomicU64::new(next));
        let appender = Appender::new(
            pool.clone(),
            AppenderConfig {
                chunk_size: config.chunk_size,
                prealloc_step: config.prealloc_step,
                idle: config.idle_write,
            },
            written.clone(),
        );
        let segments = segments
            .into_iter()
            .map(|s| {
                let seg = Segment {
                    base: s.base,
                    term: s.term,
                    end: s.end,
                    size: s.size,
                    file: s.file,
                    index: s.index,
                    active: false,
                };
                (s.base, seg)
            })
            .collect();
        Ok(Log {
            inner: Rc::new(Inner {
                id,
                dir,
                io: io.clone(),
                kv: kv.clone(),
                appender,
                written,
                flushed_tx: watch::Sender::new(next),
                lock: tokio::sync::Mutex::new(()),
                state: RefCell::new(State {
                    start,
                    next,
                    flushed: next,
                    segments,
                    cache: Cache::new(config.cache_bytes),
                    epoch: 0,
                    truncating: false,
                    inflight: None,
                    next_sync_id: 0,
                    closed: false,
                }),
                config,
            }),
        })
    }

    /// Return the group identifier whose records this log stores.
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// Return the directory containing this log's segments and indexes.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Snapshot record positions after translating completed byte writes into whole-record progress.
    pub fn offsets(&self) -> Offsets {
        self.inner.appender.refresh_written();
        let st = self.inner.state.borrow();
        Offsets {
            start: st.start,
            next: st.next,
            written: self.inner.written.load(Ordering::Acquire).min(st.next),
            flushed: st.flushed,
        }
    }

    /// The first readable index.
    pub fn start_index(&self) -> u64 {
        self.inner.state.borrow().start
    }

    /// The index the next append must have.
    pub fn next_index(&self) -> u64 {
        self.inner.state.borrow().next
    }

    /// Return the final readable record index, or `None` when the retained log is empty.
    pub fn last_index(&self) -> Option<u64> {
        let st = self.inner.state.borrow();
        (st.next > st.start).then(|| st.next - 1)
    }

    /// The term of the entry at `index`, if the log holds it.
    pub fn term_at(&self, index: u64) -> Option<u64> {
        let st = self.inner.state.borrow();
        if index < st.start || index >= st.next {
            return None;
        }
        st.segments
            .range(..=index)
            .next_back()
            .filter(|(_, s)| index < s.end)
            .map(|(_, s)| s.term)
    }

    /// Return the final readable record's term, or `None` for an empty log.
    pub fn last_term(&self) -> Option<u64> {
        self.term_at(self.last_index()?)
    }

    /// Snapshot segment boundaries and logical sizes in base-index order.
    pub fn segments(&self) -> Vec<SegmentInfo> {
        let st = self.inner.state.borrow();
        st.segments
            .values()
            .map(|s| SegmentInfo {
                base: s.base,
                term: s.term,
                end: s.end,
                size: s.size,
            })
            .collect()
    }

    /// Watches the flushed position. It moves back when a suffix truncation
    /// cuts below it.
    pub fn subscribe_flushed(&self) -> watch::Receiver<u64> {
        self.inner.flushed_tx.subscribe()
    }

    /// Appends records, returning once they are buffered, not durable; see
    /// [`Log::flush`]. Indexes must continue from [`Log::next_index`] and
    /// terms must not go back. Waits if the core's write pool is exhausted.
    /// Once polled, the transaction runs even if its waiting future is dropped;
    /// cancellation cannot leave half a record in front of later appends.
    pub async fn append(&self, records: impl IntoIterator<Item = Record>) -> Result<()> {
        let records: Vec<Record> = records.into_iter().collect();
        let this = self.clone();
        tokio::task::spawn_local(async move { this.append_inner(records).await })
            .await
            .map_err(|e| Error::from(std::io::Error::other(e)))?
    }

    /// Serialize validation, segment rolls, and complete framed-record appends as one transaction.
    async fn append_inner(&self, records: Vec<Record>) -> Result<()> {
        let _guard = self.inner.lock.lock().await;
        {
            let st = self.inner.state.borrow();
            if st.closed {
                return Err(Error::Closed);
            }
            let mut term = st
                .segments
                .values()
                .next_back()
                .filter(|s| s.end > st.start)
                .map_or(0, |s| s.term);
            for (next, r) in (st.next..).zip(&records) {
                if r.index != next {
                    return Err(Error::OutOfOrder(format!(
                        "index {} where {next} was expected",
                        r.index
                    )));
                }
                if r.term < term {
                    return Err(Error::OutOfOrder(format!(
                        "term {} at index {} after term {term}",
                        r.term, r.index
                    )));
                }
                term = r.term;
            }
        }
        for r in records {
            let roll = {
                let st = self.inner.state.borrow();
                st.segments.values().next_back().is_none_or(|s| {
                    !s.active || s.term != r.term || s.size >= self.inner.config.segment_size
                })
            };
            if roll {
                self.roll(r.index, r.term).await?;
            }
            self.inner
                .appender
                .write(&r.header(), &r.payload, r.index)
                .await?;
            let mut st = self.inner.state.borrow_mut();
            let seg = st.segments.values_mut().next_back().unwrap();
            seg.index.maybe_add(r.index, seg.size);
            seg.size += r.disk_len() as u64;
            seg.end = r.index + 1;
            st.next = r.index + 1;
            st.cache.push(r);
        }
        Ok(())
    }

    /// Makes everything appended so far durable. Concurrent flushes share
    /// syncs: one `fdatasync` completes every flush asking for no more than
    /// what it covers.
    ///
    /// The target is what was appended when this is called, not when the
    /// future is first polled.
    pub fn flush(&self) -> impl Future<Output = Result<()>> + use<> {
        let target = self.inner.state.borrow().next;
        let this = self.clone();
        async move { this.flush_to(target).await }
    }

    /// Wait for a record prefix to become durable, sharing active segment syncs and close operations.
    async fn flush_to(&self, target: u64) -> Result<()> {
        loop {
            let (id, done) = {
                let mut st = self.inner.state.borrow_mut();
                if st.flushed >= target {
                    return Ok(());
                }
                if st.inflight.is_none() {
                    match self.inner.appender.sync() {
                        Some((sync, upto)) => self.set_inflight(&mut st, sync, upto),
                        None => {
                            // Nothing active and nothing in flight: what was
                            // appended is in closed, synced segments.
                            st.flushed = st.next;
                            self.inner.flushed_tx.send_replace(st.flushed);
                            return Ok(());
                        }
                    }
                }
                let s = st.inflight.as_ref().unwrap();
                (s.id, s.done.clone())
            };
            let result = done.await;
            self.finish_sync(id, result.clone());
            result?;
        }
    }

    /// Share a sync with its record limit and truncation epoch so stale completions cannot advance durability.
    fn set_inflight(
        &self,
        st: &mut State,
        sync: impl Future<Output = std::io::Result<()>> + 'static,
        upto: u64,
    ) {
        let id = st.next_sync_id;
        st.next_sync_id += 1;
        st.inflight = Some(InflightSync {
            id,
            done: async move { sync.await.map_err(Error::from) }
                .boxed_local()
                .shared(),
            upto,
            epoch: st.epoch,
        });
    }

    /// Applies a finished sync, once, whoever observes it first.
    fn finish_sync(&self, id: u64, result: Result<()>) {
        let mut st = self.inner.state.borrow_mut();
        if st.inflight.as_ref().is_none_or(|s| s.id != id) {
            return;
        }
        let s = st.inflight.take().unwrap();
        if result.is_ok() && s.epoch == st.epoch && s.upto > st.flushed {
            st.flushed = s.upto.min(st.next);
            self.inner.flushed_tx.send_replace(st.flushed);
        }
    }

    /// Reads records from `range`, in order, stopping once they add up to
    /// `max_bytes` (counting framing), but always returning at least one if
    /// the range has any. Recent appends come from memory.
    pub async fn read(
        &self,
        range: impl RangeBounds<u64>,
        max_bytes: usize,
    ) -> Result<Vec<Record>> {
        let lo = match range.start_bound() {
            Bound::Included(&i) => i,
            Bound::Excluded(&i) => i.saturating_add(1),
            Bound::Unbounded => self.start_index(),
        };
        let hi = match range.end_bound() {
            Bound::Included(&i) => i.saturating_add(1),
            Bound::Excluded(&i) => i,
            Bound::Unbounded => u64::MAX,
        };
        'restart: loop {
            let mut out: Vec<Record> = Vec::new();
            let mut bytes = 0;
            let mut lo = lo;
            // Where the last io read left off: (segment, index, position).
            let mut cursor: Option<(u64, u64, u64)> = None;
            let full = |out: &Vec<Record>, bytes: usize| !out.is_empty() && bytes >= max_bytes;
            loop {
                if self.inner.state.borrow().truncating {
                    // The truncation holds the lock until it's done.
                    drop(self.inner.lock.lock().await);
                    continue 'restart;
                }
                let (seg_base, file, mut pos, mut index, size, hi, epoch, barrier) = {
                    let st = self.inner.state.borrow();
                    if st.closed {
                        return Err(Error::Closed);
                    }
                    if lo < st.start && lo < hi {
                        return Err(Error::BelowStart {
                            index: lo,
                            start: st.start,
                        });
                    }
                    let hi = hi.min(st.next);
                    while lo < hi && !full(&out, bytes) {
                        let Some(r) = st.cache.get(lo) else { break };
                        bytes += r.disk_len();
                        out.push(r.clone());
                        lo += 1;
                    }
                    if lo >= hi || full(&out, bytes) {
                        return Ok(out);
                    }
                    let Some((_, seg)) = st.segments.range(..=lo).next_back() else {
                        return Err(corrupt(&self.inner.dir, format!("no segment holds {lo}")));
                    };
                    let (index, pos) = match cursor {
                        Some((b, i, p)) if b == seg.base && i <= lo => (i, p),
                        _ => seg.index.nearest(lo).unwrap_or((seg.base, 0)),
                    };
                    (
                        seg.base,
                        seg.file.clone(),
                        pos,
                        index,
                        seg.size,
                        hi,
                        st.epoch,
                        seg.active.then(|| self.inner.appender.read_barrier()),
                    )
                };
                if let Some(barrier) = barrier {
                    barrier.await?;
                }
                let want = max_bytes.saturating_sub(bytes).clamp(READ_MIN, READ_MAX);
                let mut len = ((lo - index) as usize * HEADER_LEN + INDEX_INTERVAL as usize + want)
                    .min((size - pos) as usize);
                let progressed = loop {
                    let buf = self.inner.io.read_at(&file, pos, len).await?;
                    if self.inner.state.borrow().epoch != epoch {
                        continue 'restart;
                    }
                    if buf.len() < len {
                        return Err(corrupt(file.path(), "segment shorter than its records"));
                    }
                    let mut off = 0;
                    let mut progressed = false;
                    while index < hi && !full(&out, bytes) && (pos + off as u64) < size {
                        match record::parse(&buf[off..]) {
                            Parsed::Record(r, n) => {
                                if r.index != index {
                                    return Err(corrupt(
                                        file.path(),
                                        format!("found index {} where {index} belongs", r.index),
                                    ));
                                }
                                if index >= lo {
                                    bytes += n;
                                    out.push(r);
                                }
                                off += n;
                                index += 1;
                                progressed = true;
                            }
                            Parsed::Short(need) if !progressed => {
                                if need > (size - pos) as usize {
                                    return Err(corrupt(
                                        file.path(),
                                        "record runs past the segment",
                                    ));
                                }
                                len = need;
                                break;
                            }
                            Parsed::Short(_) => break,
                            Parsed::End => return Err(corrupt(file.path(), "log ends early")),
                            Parsed::Corrupt(e) => return Err(corrupt(file.path(), e)),
                        }
                    }
                    pos += off as u64;
                    if progressed || index >= hi || pos >= size {
                        break progressed;
                    }
                };
                if !progressed && index < hi {
                    return Err(corrupt(file.path(), "no records where some belong"));
                }
                lo = lo.max(index);
                cursor = (pos < size).then_some((seg_base, index, pos));
            }
        }
    }

    /// Removes every entry at or above `from`, durably, as when a Raft
    /// follower's log conflicts with its leader's. Later segments are deleted
    /// first, so a crash part-way leaves a longer log, never one with a gap.
    pub async fn truncate_suffix(&self, from: u64) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_local(async move { this.truncate_suffix_inner(from).await })
            .await
            .map_err(|e| Error::from(std::io::Error::other(e)))?
    }

    /// Serialize suffix removal and file truncation, invalidating reads and old flush epochs.
    async fn truncate_suffix_inner(&self, from: u64) -> Result<()> {
        let _guard = self.inner.lock.lock().await;
        {
            let st = self.inner.state.borrow();
            if st.closed {
                return Err(Error::Closed);
            }
            if from >= st.next {
                return Ok(());
            }
            if from < st.start {
                return Err(Error::BelowStart {
                    index: from,
                    start: st.start,
                });
            }
        }
        // The segment's data and index are about to change; the next open
        // must not trust them.
        self.clear_clean_marker().await?;
        self.close_active().await?;

        // Reads wait from here until the segments match their files again,
        // even if this fails or is dropped part-way.
        let _truncating = Truncating::start(&self.inner.state);
        let doomed: Vec<Segment> = {
            let mut st = self.inner.state.borrow_mut();
            st.epoch += 1;
            // Cutting back to the start leaves nothing readable; segments
            // that hold only entries below it go too, so none outlive the
            // terms that follow.
            let lo = if from <= st.start { 0 } else { from };
            let bases: Vec<u64> = st.segments.range(lo..).map(|(&b, _)| b).collect();
            bases
                .into_iter()
                .rev()
                .map(|b| st.segments.remove(&b).unwrap())
                .collect()
        };
        for seg in doomed {
            self.remove_segment(seg.base, seg.term).await?;
        }

        let cut = {
            let st = self.inner.state.borrow();
            st.segments
                .values()
                .next_back()
                .filter(|s| s.end > from)
                .map(|s| (s.base, s.term, s.file.clone(), s.index.nearest(from)))
        };
        if let Some((base, term, file, nearest)) = cut {
            let (i, p) = nearest.unwrap_or((base, 0));
            let scan = index::scan(
                &self.inner.io,
                &file,
                p,
                i,
                term,
                Some(from),
                &mut SparseIndex::default(),
            )
            .await?;
            if scan.last.map_or(i, |last| last + 1) == from {
                file.set_len(scan.end).await?;
                file.sync_data().await?;
            }

            if scan.last.map_or(i, |l| l + 1) != from {
                return Err(corrupt(
                    file.path(),
                    format!("can't find {from} to truncate at"),
                ));
            }
            let meta = {
                let mut st = self.inner.state.borrow_mut();
                let seg = st.segments.get_mut(&base).unwrap();
                seg.size = scan.end;
                seg.end = from;
                seg.index.truncate(from);
                seg_meta(seg)
            };
            self.write_index(meta).await?;
        }

        let mut st = self.inner.state.borrow_mut();
        st.next = from;
        st.flushed = st.flushed.min(from);
        st.cache.truncate(from);
        self.inner.written.fetch_min(from, Ordering::AcqRel);
        self.inner.flushed_tx.send_replace(st.flushed);
        Ok(())
    }

    /// Discards entries below `new_start`, as after a Raft snapshot. The new
    /// start is persisted first; then only whole segments below it are
    /// deleted, so reads below it fail even where the bytes remain. A start
    /// past the end empties the log, and the next append must be at
    /// `new_start`.
    pub async fn truncate_prefix(&self, new_start: u64) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_local(async move { this.truncate_prefix_inner(new_start).await })
            .await
            .map_err(|e| Error::from(std::io::Error::other(e)))?
    }

    /// Persist the new readable start before deleting obsolete segments under log exclusion.
    async fn truncate_prefix_inner(&self, new_start: u64) -> Result<()> {
        let _guard = self.inner.lock.lock().await;
        let (start, next) = {
            let st = self.inner.state.borrow();
            if st.closed {
                return Err(Error::Closed);
            }
            (st.start, st.next)
        };
        if new_start <= start {
            return Ok(());
        }
        // Keep start <= flushed: a crash mustn't leave a log ending below
        // its start with entries missing in between.
        self.flush_to(new_start.min(next)).await?;
        // Only whole segments go, so a trusted index can't go stale here,
        // but every truncation clears the marker; the next close sets it.
        self.clear_clean_marker().await?;
        self.inner
            .kv
            .put(
                KeySpace::STORAGE,
                &start_key(self.inner.id),
                new_start.to_le_bytes().to_vec(),
            )
            .await?;
        if new_start >= next {
            self.close_active().await?;
        }
        let doomed: Vec<Segment> = {
            let mut st = self.inner.state.borrow_mut();
            st.start = new_start;
            st.epoch += 1;
            st.cache.evict_below(new_start);
            if new_start >= st.next {
                st.next = new_start;
                st.flushed = new_start;
                self.inner.written.fetch_max(new_start, Ordering::AcqRel);
                self.inner.flushed_tx.send_replace(new_start);
            }
            let bases: Vec<u64> = st
                .segments
                .values()
                .take_while(|s| s.end <= new_start)
                .map(|s| s.base)
                .collect();
            bases
                .into_iter()
                .map(|b| st.segments.remove(&b).unwrap())
                .collect()
        };
        for seg in doomed {
            debug_assert!(!seg.active);
            self.remove_segment(seg.base, seg.term).await?;
        }
        Ok(())
    }

    /// Writes out and syncs everything, and marks the log clean so the next
    /// open needn't rescan it. Later calls fail with [`Error::Closed`].
    pub async fn close(&self) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_local(async move { this.close_inner().await })
            .await
            .map_err(|e| Error::from(std::io::Error::other(e)))?
    }

    /// Serialize final segment persistence before recording the clean-recovery marker.
    async fn close_inner(&self) -> Result<()> {
        let _guard = self.inner.lock.lock().await;
        if self.inner.state.borrow().closed {
            return Ok(());
        }
        self.close_active().await?;
        let last = self
            .inner
            .state
            .borrow()
            .segments
            .keys()
            .next_back()
            .copied();
        let key = clean_key(self.inner.id);
        match last {
            Some(base) => {
                self.inner
                    .kv
                    .put(KeySpace::STORAGE, &key, base.to_le_bytes().to_vec())
                    .await?
            }
            None => self.clear_clean_marker().await?,
        }
        self.inner.state.borrow_mut().closed = true;
        Ok(())
    }

    /// Durably revoke trusted-index recovery before operations can make segment metadata stale.
    async fn clear_clean_marker(&self) -> Result<()> {
        let key = clean_key(self.inner.id);
        if self.inner.kv.get(KeySpace::STORAGE, &key).is_some() {
            self.inner.kv.delete(KeySpace::STORAGE, &key).await?;
        }
        Ok(())
    }

    /// Closes the active segment, then starts one at `base` for `term`.
    async fn roll(&self, base: u64, term: u64) -> Result<()> {
        self.close_active().await?;
        let path = segment_path(&self.inner.dir, base, term);
        let file = self.inner.io.open_data(path, true).await?;
        self.inner.io.sync_dir(&self.inner.dir).await?;
        self.inner.appender.activate(file.clone(), base).await?;
        self.inner.state.borrow_mut().segments.insert(
            base,
            Segment {
                base,
                term,
                end: base,
                size: 0,
                file,
                index: SparseIndex::default(),
                active: true,
            },
        );
        Ok(())
    }

    /// Writes out the active segment, trims its preallocated tail, syncs it
    /// and persists its index. Afterwards every segment is closed.
    async fn close_active(&self) -> Result<()> {
        if !self.inner.appender.is_active() {
            return Ok(());
        }
        // Let any sync in flight finish first, so the close's sync can be
        // the one flushes wait on.
        loop {
            let pending = {
                let st = self.inner.state.borrow();
                st.inflight.as_ref().map(|s| (s.id, s.done.clone()))
            };
            let Some((id, done)) = pending else { break };
            let result = done.await;
            self.finish_sync(id, result.clone());
            result?;
        }
        let closed = self.inner.appender.close().unwrap();
        let (id, done) = {
            let mut st = self.inner.state.borrow_mut();
            let upto = st.next;
            self.set_inflight(&mut st, closed.sync, upto);
            let s = st.inflight.as_ref().unwrap();
            (s.id, s.done.clone())
        };
        let result = done.await;
        self.finish_sync(id, result.clone());
        result?;
        let meta = {
            let mut st = self.inner.state.borrow_mut();
            let seg = st.segments.values_mut().next_back().unwrap();
            debug_assert_eq!(seg.size, closed.size);
            seg.active = false;
            (seg.end > seg.base).then(|| seg_meta(seg))
        };
        match meta {
            Some(meta) => self.write_index(meta).await,
            None => {
                // Rolled but never appended to.
                let seg = {
                    let mut st = self.inner.state.borrow_mut();
                    let base = *st.segments.keys().next_back().unwrap();
                    st.segments.remove(&base).unwrap()
                };
                self.remove_segment(seg.base, seg.term).await
            }
        }
    }

    /// Persist checksummed metadata for a closed segment; clean-marker ordering governs trust on reopen.
    async fn write_index(&self, meta: SegmentMeta) -> Result<()> {
        let path = index_path(&self.inner.dir, meta.base, meta.term);
        // Checksums and recorded length detect torn or stale index files.
        self.inner.io.write_file(&path, &meta.encode()).await?;
        Ok(())
    }

    /// Remove both index and data, then persist their directory removals.
    async fn remove_segment(&self, base: u64, term: u64) -> Result<()> {
        let dir = &self.inner.dir;
        self.inner.io.remove(&index_path(dir, base, term)).await?;
        self.inner.io.remove(&segment_path(dir, base, term)).await?;
        self.inner.io.sync_dir(dir).await?;
        Ok(())
    }
}

/// Snapshot a nonempty segment's logical end and sparse offsets for its persisted index.
fn seg_meta(seg: &Segment) -> SegmentMeta {
    SegmentMeta {
        base: seg.base,
        term: seg.term,
        last: seg.end - 1,
        size: seg.size,
        index: seg.index.clone(),
    }
}

/// Recovers one segment through asynchronous, scheduled reads. The outer error is I/O; the
/// inner one is corruption recovery can't repair.
async fn recover_segment(
    io: &StorageIo,
    dir: &Path,
    base: u64,
    term: u64,
    trusted: bool,
    last: bool,
) -> std::io::Result<std::result::Result<Recovered, String>> {
    let path = segment_path(dir, base, term);
    let file = io.open_data(path.clone(), false).await?;
    let file_len = file.len().await?;
    let ipath = index_path(dir, base, term);
    if trusted
        && let Some(meta) = io
            .read_optional(&ipath)
            .await?
            .and_then(|b| SegmentMeta::decode(&b))
        && meta.base == base
        && meta.term == term
        && meta.size == file_len
    {
        return Ok(Ok(Recovered {
            base,
            term,
            end: meta.last + 1,
            size: meta.size,
            file,
            index: meta.index,
        }));
    }
    let mut sparse = SparseIndex::default();
    let scan = index::scan(io, &file, 0, base, term, None, &mut sparse).await?;
    if !last {
        if scan.end != file_len {
            return Ok(Err(format!(
                "{} at byte {} of a closed segment",
                scan.stop.unwrap_or("trailing bytes"),
                scan.end
            )));
        }
        if scan.last.is_none() {
            return Ok(Err("empty closed segment".into()));
        }
    }
    if scan.end < file_len {
        // A crash's torn tail, or preallocated space.
        if scan.stop != Some("zero header") {
            tracing::warn!(
                path = %path.display(),
                at = scan.end,
                reason = scan.stop.unwrap_or(""),
                "truncating the log's torn tail"
            );
        }
        file.set_len(scan.end).await?;
    }
    // What a crashed process left may still be only in the page cache.
    file.sync_data().await?;
    if let Some(l) = scan.last {
        let meta = SegmentMeta {
            base,
            term,
            last: l,
            size: scan.end,
            index: sparse.clone(),
        };
        io.write_file(&ipath, &meta.encode()).await?;
    }
    Ok(Ok(Recovered {
        base,
        term,
        end: scan.last.map_or(base, |l| l + 1),
        size: scan.end,
        file,
        index: sparse,
    }))
}
