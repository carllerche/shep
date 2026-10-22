//! A partition replica's durable Raft log: segment files of checksummed
//! records, plus the vote and the purge point.
//!
//! Appends are written to the OS before `append` returns, so readers see them
//! at once; a background thread makes them durable with `fsync`, batching
//! whatever accumulated, and then tells Raft. Truncation and purging are rare
//! and synchronous.
//!
//! Layout of a replica's directory:
//!
//! ```text
//! vote                 the latest vote, replaced atomically
//! purged               the last purged log ID, replaced atomically
//! log/<first>.seg      records [len: u32][crc32: u32][Entry], `first` the
//!                      index of the segment's first entry
//! ```

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Debug;
use std::fs::{self, File};
use std::io::{self, Write};
use std::ops::{Bound, RangeBounds};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use openraft::storage::{LogFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::{AnyError, LogId, StorageIOError, Vote};
use prost::Message;

use crate::raft_types::{self, Entry, StorageError, TypeConfig, proto};

const HEADER: usize = 8;

#[derive(Clone)]
pub(crate) struct LogStore {
    shared: Arc<Shared>,
}

struct Shared {
    dir: PathBuf,
    segment_size: u64,
    state: Mutex<State>,
    flush_tx: Mutex<mpsc::Sender<Flush>>,
}

struct State {
    vote: Option<Vote<u64>>,
    purged: Option<LogId<u64>>,
    /// By first index. The last is the one appended to.
    segments: BTreeMap<u64, Segment>,
    /// Where each entry in `first..first + locs.len()` is.
    first: u64,
    locs: VecDeque<Loc>,
    /// Files written since the last flush request.
    dirty: Vec<Arc<File>>,
}

struct Segment {
    path: PathBuf,
    file: Arc<File>,
    len: u64,
}

#[derive(Clone, Copy)]
struct Loc {
    segment: u64,
    offset: u64,
    len: u32,
    log_id: LogId<u64>,
}

struct Flush {
    files: Vec<Arc<File>>,
    callback: LogFlushed<TypeConfig>,
}

impl State {
    fn next_index(&self) -> u64 {
        self.first + self.locs.len() as u64
    }

    fn last_log_id(&self) -> Option<LogId<u64>> {
        self.locs.back().map(|l| l.log_id).or(self.purged)
    }
}

impl LogStore {
    /// Opens the log in `dir`, creating it if needed, and recovers from a
    /// torn final write.
    pub(crate) fn open(dir: &Path, segment_size: u64) -> io::Result<LogStore> {
        let log_dir = dir.join("log");
        fs::create_dir_all(&log_dir)?;

        let vote = read_optional(&dir.join("vote"))?
            .map(|b| raft_types::vote_from_proto(Some(decode::<proto::Vote>(&b)?)).map_err(invalid))
            .transpose()?;
        let purged = read_optional(&dir.join("purged"))?
            .map(|b| raft_types::log_id_from_proto(decode::<proto::LogId>(&b)?).map_err(invalid))
            .transpose()?;

        let mut names: Vec<(u64, PathBuf)> = Vec::new();
        for ent in fs::read_dir(&log_dir)? {
            let path = ent?.path();
            let Some(first) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".seg"))
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            names.push((first, path));
        }
        names.sort();

        let purged_index = purged.map(|p| p.index);
        let mut state = State {
            vote,
            purged,
            segments: BTreeMap::new(),
            first: purged_index.map_or(0, |i| i + 1),
            locs: VecDeque::new(),
            dirty: Vec::new(),
        };

        let count = names.len();
        for (n, (seg_first, path)) in names.into_iter().enumerate() {
            let is_last = n + 1 == count;
            let file = File::options().read(true).append(true).open(&path)?;
            let bytes = fs::read(&path)?;
            let mut offset = 0usize;
            let mut kept = 0usize;
            while offset < bytes.len() {
                let Some((entry, len)) = parse_record(&bytes[offset..]) else {
                    if !is_last {
                        return Err(invalid(format!(
                            "corrupt record in {} at {offset}",
                            path.display()
                        )));
                    }
                    tracing::warn!(path = %path.display(), offset, "truncating a torn write");
                    file.set_len(offset as u64)?;
                    file.sync_all()?;
                    break;
                };
                let index = entry.log_id.index;
                if offset == 0 && index != seg_first {
                    return Err(invalid(format!(
                        "{} starts at index {index}",
                        path.display()
                    )));
                }
                if purged_index.is_none_or(|p| index > p) {
                    if !state.locs.is_empty() && index != state.next_index() {
                        return Err(invalid(format!(
                            "gap before index {index} in {}",
                            path.display()
                        )));
                    }
                    if state.locs.is_empty() {
                        state.first = index;
                    }
                    state.locs.push_back(Loc {
                        segment: seg_first,
                        offset: offset as u64,
                        len: len as u32,
                        log_id: entry.log_id,
                    });
                    kept += 1;
                }
                offset += len;
            }
            let len = file.metadata()?.len();
            if kept == 0 && !is_last {
                // Everything in it was purged.
                fs::remove_file(&path)?;
                continue;
            }
            state.segments.insert(
                seg_first,
                Segment {
                    path,
                    file: Arc::new(file),
                    len,
                },
            );
        }
        sync_dir(&log_dir)?;

        let (flush_tx, flush_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("shep-wal-flush".into())
            .spawn(move || flusher(flush_rx))?;

        Ok(LogStore {
            shared: Arc::new(Shared {
                dir: dir.to_path_buf(),
                segment_size,
                state: Mutex::new(state),
                flush_tx: Mutex::new(flush_tx),
            }),
        })
    }

    pub(crate) fn last_log_id(&self) -> Option<LogId<u64>> {
        self.shared.state.lock().unwrap().last_log_id()
    }

    pub(crate) fn purged(&self) -> Option<LogId<u64>> {
        self.shared.state.lock().unwrap().purged
    }

    /// The entries in `range` that the log holds, in order.
    pub(crate) fn read<R: RangeBounds<u64>>(&self, range: R) -> io::Result<Vec<Entry>> {
        let reads: Vec<(Arc<File>, u64, Vec<u32>)> = {
            let st = self.shared.state.lock().unwrap();
            let lo = match range.start_bound() {
                Bound::Included(&i) => i,
                Bound::Excluded(&i) => i + 1,
                Bound::Unbounded => 0,
            }
            .max(st.first);
            let hi = match range.end_bound() {
                Bound::Included(&i) => i.saturating_add(1),
                Bound::Excluded(&i) => i,
                Bound::Unbounded => u64::MAX,
            }
            .min(st.next_index());
            // Coalesce runs of records that are adjacent in one segment.
            let mut reads: Vec<(Arc<File>, u64, Vec<u32>)> = Vec::new();
            let mut last: Option<(u64, u64)> = None;
            for i in lo..hi {
                let loc = st.locs[(i - st.first) as usize];
                match (reads.last_mut(), last) {
                    (Some(run), Some((seg, end))) if seg == loc.segment && end == loc.offset => {
                        run.2.push(loc.len);
                    }
                    _ => {
                        let file = st.segments[&loc.segment].file.clone();
                        reads.push((file, loc.offset, vec![loc.len]));
                    }
                }
                last = Some((loc.segment, loc.offset + loc.len as u64));
            }
            reads
        };

        let mut out = Vec::new();
        for (file, offset, lens) in reads {
            let total: usize = lens.iter().map(|&l| l as usize).sum();
            let mut buf = vec![0; total];
            file.read_exact_at(&mut buf, offset)?;
            let mut at = 0;
            for len in lens {
                let (entry, n) = parse_record(&buf[at..at + len as usize]).ok_or_else(|| {
                    invalid(format!("corrupt record at offset {}", offset + at as u64))
                })?;
                out.push(entry);
                at += n;
            }
        }
        Ok(out)
    }

    fn append_blocking(
        &self,
        entries: Vec<Entry>,
        callback: LogFlushed<TypeConfig>,
    ) -> io::Result<()> {
        let mut st = self.shared.state.lock().unwrap();
        for entry in entries {
            let index = entry.log_id.index;
            if !st.locs.is_empty() && index != st.next_index() {
                return Err(invalid(format!(
                    "append at {index}, expected {}",
                    st.next_index()
                )));
            }
            if st.locs.is_empty() {
                st.first = index;
            }
            let body = raft_types::entry_to_proto(&entry).encode_to_vec();
            let mut buf = Vec::with_capacity(HEADER + body.len());
            buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
            buf.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
            buf.extend_from_slice(&body);

            // An empty segment must be named for the index appended to it.
            if let Some((&seg_first, seg)) = st.segments.last_key_value()
                && seg.len == 0
                && seg_first != index
            {
                fs::remove_file(&seg.path)?;
                st.segments.remove(&seg_first);
            }
            let roll = st
                .segments
                .last_key_value()
                .is_none_or(|(_, s)| s.len >= self.shared.segment_size);
            if roll {
                let path = self.shared.dir.join("log").join(format!("{index:020}.seg"));
                let file = File::options()
                    .read(true)
                    .append(true)
                    .create_new(true)
                    .open(&path)?;
                sync_dir(&self.shared.dir.join("log"))?;
                st.segments.insert(
                    index,
                    Segment {
                        path,
                        file: Arc::new(file),
                        len: 0,
                    },
                );
            }
            let (&seg_first, seg) = st.segments.last_key_value().unwrap();
            let file = seg.file.clone();
            let offset = seg.len;
            (&*file).write_all(&buf)?;
            st.segments.get_mut(&seg_first).unwrap().len += buf.len() as u64;
            st.locs.push_back(Loc {
                segment: seg_first,
                offset,
                len: buf.len() as u32,
                log_id: entry.log_id,
            });
            if !st.dirty.iter().any(|f| Arc::ptr_eq(f, &file)) {
                st.dirty.push(file);
            }
        }
        let files = std::mem::take(&mut st.dirty);
        drop(st);
        let tx = self.shared.flush_tx.lock().unwrap().clone();
        tx.send(Flush { files, callback })
            .map_err(|_| io::Error::other("log flusher stopped"))
    }

    fn truncate_blocking(&self, index: u64) -> io::Result<()> {
        let mut st = self.shared.state.lock().unwrap();
        if index >= st.next_index() {
            return Ok(());
        }
        let keep = index.saturating_sub(st.first) as usize;
        let cut = st.locs[keep];
        // Later segments go first, so a crash part-way leaves a longer log,
        // never one with a gap.
        let later: Vec<u64> = st
            .segments
            .range(cut.segment + 1..)
            .map(|(&k, _)| k)
            .collect();
        for first in later.into_iter().rev() {
            let seg = st.segments.remove(&first).unwrap();
            fs::remove_file(&seg.path)?;
        }
        sync_dir(&self.shared.dir.join("log"))?;
        let seg = st.segments.get_mut(&cut.segment).unwrap();
        seg.file.set_len(cut.offset)?;
        seg.file.sync_all()?;
        seg.len = cut.offset;
        st.locs.truncate(keep);
        Ok(())
    }

    fn purge_blocking(&self, log_id: LogId<u64>) -> io::Result<()> {
        write_atomic(
            &self.shared.dir,
            "purged",
            &raft_types::log_id_to_proto(&log_id).encode_to_vec(),
        )?;
        let mut st = self.shared.state.lock().unwrap();
        st.purged = Some(log_id);
        while st.first <= log_id.index && !st.locs.is_empty() {
            st.locs.pop_front();
            st.first += 1;
        }
        if st.locs.is_empty() {
            st.first = log_id.index + 1;
        }
        // Drop every segment but the last whose entries are all purged.
        let firsts: Vec<u64> = st.segments.keys().copied().collect();
        for pair in firsts.windows(2) {
            if pair[1] <= log_id.index + 1 {
                let seg = st.segments.remove(&pair[0]).unwrap();
                fs::remove_file(&seg.path)?;
            }
        }
        sync_dir(&self.shared.dir.join("log"))
    }

    fn save_vote_blocking(&self, vote: &Vote<u64>) -> io::Result<()> {
        write_atomic(
            &self.shared.dir,
            "vote",
            &raft_types::vote_to_proto(vote).encode_to_vec(),
        )?;
        self.shared.state.lock().unwrap().vote = Some(*vote);
        Ok(())
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, StorageError> {
        self.read(range)
            .map_err(|e| StorageIOError::read_logs(AnyError::new(&e)).into())
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = LogStore;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError> {
        let st = self.shared.state.lock().unwrap();
        Ok(LogState {
            last_purged_log_id: st.purged,
            last_log_id: st.last_log_id(),
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError> {
        let this = self.clone();
        let vote = *vote;
        blocking(move || this.save_vote_blocking(&vote))
            .await
            .map_err(|e| StorageIOError::write_vote(AnyError::new(&e)).into())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError> {
        Ok(self.shared.state.lock().unwrap().vote)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        // Writing reaches only the OS's cache, so it's done inline; the
        // flusher makes it durable.
        self.append_blocking(entries.into_iter().collect(), callback)
            .map_err(|e| StorageIOError::write_logs(AnyError::new(&e)).into())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError> {
        let this = self.clone();
        blocking(move || this.truncate_blocking(log_id.index))
            .await
            .map_err(|e| StorageIOError::write_logs(AnyError::new(&e)).into())
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError> {
        let this = self.clone();
        blocking(move || this.purge_blocking(log_id))
            .await
            .map_err(|e| StorageIOError::write_logs(AnyError::new(&e)).into())
    }
}

fn flusher(rx: mpsc::Receiver<Flush>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        let mut files: Vec<&Arc<File>> = Vec::new();
        for f in batch.iter().flat_map(|b| &b.files) {
            if !files.iter().any(|g| Arc::ptr_eq(f, g)) {
                files.push(f);
            }
        }
        let result = files.iter().try_for_each(|f| f.sync_data());
        for flush in batch {
            let result = match &result {
                Ok(()) => Ok(()),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            };
            flush.callback.log_io_completed(result);
        }
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(io::Error::other)?
}

/// One record: the entry and the record's length, or `None` if it's torn or
/// corrupt.
fn parse_record(buf: &[u8]) -> Option<(Entry, usize)> {
    if buf.len() < HEADER {
        return None;
    }
    let len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    let crc = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let body = buf.get(HEADER..HEADER + len)?;
    if crc32fast::hash(body) != crc {
        return None;
    }
    let entry = raft_types::entry_from_proto(proto::Entry::decode(body).ok()?).ok()?;
    Some((entry, HEADER + len))
}

fn decode<M: Message + Default>(b: &[u8]) -> io::Result<M> {
    M::decode(b).map_err(invalid)
}

fn invalid(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Replaces `dir/name` with `bytes`, durably: write a temporary file, sync
/// it, rename it over, sync the directory.
pub(crate) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, dir.join(name))?;
    sync_dir(dir)
}

pub(crate) fn read_atomic(dir: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    read_optional(&dir.join(name))
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}
