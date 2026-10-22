//! A group's Raft log on shep-storage: entries as records in the group's
//! segmented log, and its vote and purge point in its core's key-value store.

use std::fmt::Debug;
use std::io;
use std::ops::{Bound, RangeBounds};

use shep_raft_engine::storage::{IOFlushed, LogState, RaftLogReader, RaftLogStorage};
use shep_storage::{KeySpace, KvStore, Log};

use crate::types::{
    Entry, GroupId, LogId, TypeConfig, Vote, WireLogId, WireVote, entry_to_record, invalid, log_id,
    record_to_entry,
};

/// Reads at most this many bytes of records at a time.
const READ_CHUNK: usize = 4 << 20;

#[derive(Clone)]
pub(crate) struct GroupLog {
    group: GroupId,
    log: Log,
    kv: KvStore,
    /// The most entry bytes replication reads for one AppendEntries.
    max_append_bytes: usize,
}

impl GroupLog {
    pub(crate) fn new(group: GroupId, log: Log, kv: KvStore, max_append_bytes: usize) -> GroupLog {
        GroupLog {
            group,
            log,
            kv,
            max_append_bytes,
        }
    }

    fn key(&self, what: &str) -> Vec<u8> {
        format!("group/{}/{what}", self.group.0).into_bytes()
    }

    fn purged(&self) -> io::Result<Option<LogId>> {
        match self.kv.get(KeySpace::RAFT, &self.key("purged")) {
            None => Ok(None),
            Some(b) => {
                let l: WireLogId = shep_codec::from_bytes(b).map_err(invalid)?;
                Ok(Some(l.into()))
            }
        }
    }
}

fn storage_err(e: shep_storage::Error) -> io::Error {
    io::Error::other(e)
}

impl RaftLogReader<TypeConfig> for GroupLog {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug>(
        &mut self,
        range: RB,
    ) -> io::Result<Vec<Entry>> {
        let mut next = match range.start_bound() {
            Bound::Included(&i) => i,
            Bound::Excluded(&i) => i + 1,
            Bound::Unbounded => self.log.start_index(),
        };
        let end = match range.end_bound() {
            Bound::Included(&i) => i.saturating_add(1),
            Bound::Excluded(&i) => i,
            Bound::Unbounded => self.log.next_index(),
        };
        let mut out = Vec::new();
        while next < end {
            let records = self
                .log
                .read(next..end, READ_CHUNK)
                .await
                .map_err(storage_err)?;
            let Some(last) = records.last() else { break };
            next = last.index + 1;
            for r in records {
                out.push(record_to_entry(r)?);
            }
        }
        Ok(out)
    }

    /// Replication's read for one AppendEntries: entries from `start`, up to
    /// `end` or about `max_append_bytes`, whichever comes first, and at least
    /// one. The engine bounds batches by count only.
    async fn limited_get_log_entries(&mut self, start: u64, end: u64) -> io::Result<Vec<Entry>> {
        let records = self
            .log
            .read(start..end, self.max_append_bytes)
            .await
            .map_err(storage_err)?;
        records.into_iter().map(record_to_entry).collect()
    }

    async fn read_vote(&mut self) -> io::Result<Option<Vote>> {
        match self.kv.get(KeySpace::RAFT, &self.key("vote")) {
            None => Ok(None),
            Some(b) => {
                let v: WireVote = shep_codec::from_bytes(b).map_err(invalid)?;
                Ok(Some(v.into()))
            }
        }
    }
}

impl RaftLogStorage<TypeConfig> for GroupLog {
    type LogReader = GroupLog;

    async fn get_log_state(&mut self) -> io::Result<LogState<TypeConfig>> {
        let last_purged_log_id = self.purged()?;
        let last_log_id = match self.log.last_index() {
            Some(index) => {
                let term = self
                    .log
                    .term_at(index)
                    .ok_or_else(|| invalid(format!("no term for index {index}")))?;
                Some(log_id(term, index))
            }
            None => last_purged_log_id,
        };
        Ok(LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> GroupLog {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote) -> io::Result<()> {
        // `put` resolves once the vote is durable.
        let value = shep_codec::to_bytes(&WireVote::from(vote));
        self.kv
            .put(KeySpace::RAFT, &self.key("vote"), value)
            .await
            .map_err(storage_err)
    }

    async fn append<I>(&mut self, entries: I, callback: IOFlushed<TypeConfig>) -> io::Result<()>
    where
        I: IntoIterator<Item = Entry>,
    {
        let records: Vec<_> = entries.into_iter().map(|e| entry_to_record(&e)).collect();
        self.log.append(records).await.map_err(storage_err)?;
        // The entries are written behind; Raft hears they're durable when
        // the next flush covers them. Concurrent flushes share one fdatasync.
        let flush = self.log.flush();
        tokio::task::spawn_local(async move {
            callback.io_completed(flush.await.map_err(storage_err));
        });
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<LogId>) -> io::Result<()> {
        let from = last_log_id.map_or(self.log.start_index(), |l| l.index + 1);
        self.log.truncate_suffix(from).await.map_err(storage_err)
    }

    async fn purge(&mut self, upto: LogId) -> io::Result<()> {
        // The purge point first, so a crash between leaves it no lower than
        // the log's start.
        let value = shep_codec::to_bytes(&WireLogId::from(&upto));
        self.kv
            .put(KeySpace::RAFT, &self.key("purged"), value)
            .await
            .map_err(storage_err)?;
        self.log
            .truncate_prefix(upto.index + 1)
            .await
            .map_err(storage_err)
    }
}
