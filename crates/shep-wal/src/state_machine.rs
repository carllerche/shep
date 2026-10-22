//! A partition replica's Raft state machine.
//!
//! The log itself is what readers consume, so the state machine is small: the
//! idempotency table, which records each key's outcome for the idempotency
//! window and travels in snapshots (§10.3), and the applied position, which
//! tells tailers how far the log is committed.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use openraft::storage::{RaftSnapshotBuilder, RaftStateMachine, Snapshot};
use openraft::{AnyError, BasicNode, EntryPayload, LogId, StorageIOError, StoredMembership};
use prost::Message;
use shep_wal_api::{Decided, DecidedStatus, Dot, PartitionId};
use tokio::sync::watch;

use crate::log_store::{read_atomic, write_atomic};
use crate::raft_types::{self, Applied, Entry, SnapshotMeta, StorageError, TypeConfig, proto};

#[derive(Clone)]
pub(crate) struct StateMachine {
    shared: Arc<Shared>,
}

struct Shared {
    partition: PartitionId,
    dir: PathBuf,
    state: Mutex<State>,
    applied: watch::Sender<Option<u64>>,
}

#[derive(Default)]
struct State {
    last_applied: Option<LogId<u64>>,
    membership: StoredMembership<u64, BasicNode>,
    keys: HashMap<Bytes, DecidedKey>,
    /// Keys in the order they were decided, for forgetting them.
    order: VecDeque<(u64, Bytes)>,
    snapshot: Option<(SnapshotMeta, Vec<u8>)>,
    snapshot_seq: u64,
}

#[derive(Clone, Debug)]
struct DecidedKey {
    index: u64,
    commit_time_us: u64,
    failed: bool,
    reply: Bytes,
}

impl State {
    fn forget_before(&mut self, floor_us: u64) {
        while let Some((index, key)) = self.order.front() {
            let Some(rec) = self.keys.get(key) else {
                self.order.pop_front();
                continue;
            };
            if rec.index != *index {
                // Superseded; only the newest record counts.
                self.order.pop_front();
                continue;
            }
            if rec.commit_time_us >= floor_us {
                break;
            }
            self.keys.remove(key);
            self.order.pop_front();
        }
    }

    fn decide(&mut self, key: Bytes, rec: DecidedKey) {
        if let Some(prev) = self.keys.get(&key) {
            // The leader checks keys before appending, so this is a bug.
            tracing::error!(
                ?key,
                prev = prev.index,
                index = rec.index,
                "idempotency key decided twice"
            );
        }
        self.order.push_back((rec.index, key.clone()));
        self.keys.insert(key, rec);
    }

    fn encode(&self) -> Vec<u8> {
        let mut keys: Vec<_> = self.keys.iter().collect();
        keys.sort_by_key(|(_, r)| r.index);
        proto::StateMachineSnapshot {
            keys: keys
                .into_iter()
                .map(|(k, r)| proto::DecidedKey {
                    idempotency_key: k.clone(),
                    index: r.index,
                    commit_time_us: r.commit_time_us,
                    failed: r.failed,
                    reply: r.reply.clone(),
                })
                .collect(),
        }
        .encode_to_vec()
    }

    fn restore(&mut self, meta: &SnapshotMeta, data: &[u8]) -> io::Result<()> {
        let snap = proto::StateMachineSnapshot::decode(data).map_err(io::Error::other)?;
        self.keys.clear();
        self.order.clear();
        for k in snap.keys {
            self.decide(
                k.idempotency_key,
                DecidedKey {
                    index: k.index,
                    commit_time_us: k.commit_time_us,
                    failed: k.failed,
                    reply: k.reply,
                },
            );
        }
        self.last_applied = meta.last_log_id;
        self.membership = meta.last_membership.clone();
        self.snapshot = Some((meta.clone(), data.to_vec()));
        Ok(())
    }
}

impl StateMachine {
    pub(crate) fn open(partition: PartitionId, dir: &Path) -> io::Result<StateMachine> {
        let mut state = State::default();
        if let Some(bytes) = read_atomic(dir, "snapshot")? {
            let stored = proto::StoredSnapshot::decode(&bytes[..]).map_err(io::Error::other)?;
            let meta =
                raft_types::snapshot_meta_from_proto(stored.meta).map_err(io::Error::other)?;
            state.restore(&meta, &stored.data)?;
        }
        let (applied, _) = watch::channel(state.last_applied.map(|l| l.index));
        Ok(StateMachine {
            shared: Arc::new(Shared {
                partition,
                dir: dir.to_path_buf(),
                state: Mutex::new(state),
                applied,
            }),
        })
    }

    /// The key's outcome, if it's decided and remembered.
    pub(crate) fn lookup(&self, key: &[u8]) -> Option<Decided> {
        let st = self.shared.state.lock().unwrap();
        st.keys.get(key).map(|r| Decided {
            dot: Dot::new(self.shared.partition, r.index),
            commit_time_us: r.commit_time_us,
            status: if r.failed {
                DecidedStatus::Failed
            } else {
                DecidedStatus::Committed
            },
            reply: r.reply.clone(),
        })
    }

    /// The highest index applied, which is committed.
    pub(crate) fn applied_index(&self) -> Option<u64> {
        *self.shared.applied.borrow()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<u64>> {
        self.shared.applied.subscribe()
    }

    /// Waits until `index` is applied.
    pub(crate) async fn wait_applied(&self, index: u64) {
        let mut rx = self.subscribe();
        let _ = rx.wait_for(|a| a.is_some_and(|a| a >= index)).await;
    }

    fn persist_snapshot(&self, meta: &SnapshotMeta, data: &[u8]) -> io::Result<()> {
        let stored = proto::StoredSnapshot {
            meta: Some(raft_types::snapshot_meta_to_proto(meta)),
            data: data.to_vec().into(),
        };
        write_atomic(&self.shared.dir, "snapshot", &stored.encode_to_vec())
    }
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachine {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError> {
        let (meta, data) = {
            let mut st = self.shared.state.lock().unwrap();
            st.snapshot_seq += 1;
            let snapshot_id = match st.last_applied {
                Some(l) => format!("{}-{}-{}", l.leader_id.term, l.index, st.snapshot_seq),
                None => format!("none-{}", st.snapshot_seq),
            };
            let meta = SnapshotMeta {
                last_log_id: st.last_applied,
                last_membership: st.membership.clone(),
                snapshot_id,
            };
            let data = st.encode();
            st.snapshot = Some((meta.clone(), data.clone()));
            (meta, data)
        };
        self.persist_snapshot(&meta, &data).map_err(|e| {
            StorageIOError::write_snapshot(Some(meta.signature()), AnyError::new(&e))
        })?;
        Ok(Snapshot {
            meta,
            snapshot: Box::new(data),
        })
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotBuilder = StateMachine;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<u64>>, StoredMembership<u64, BasicNode>), StorageError> {
        let st = self.shared.state.lock().unwrap();
        Ok((st.last_applied, st.membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<Applied>, StorageError>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        let mut out = Vec::new();
        let last = {
            let mut st = self.shared.state.lock().unwrap();
            for entry in entries {
                let applied = match entry.payload {
                    EntryPayload::Blank => Applied::Internal,
                    EntryPayload::Membership(m) => {
                        st.membership = StoredMembership::new(Some(entry.log_id), m);
                        Applied::Internal
                    }
                    EntryPayload::Normal(cmd) if raft_types::is_void(&entry.log_id, &cmd) => {
                        Applied::Void
                    }
                    EntryPayload::Normal(cmd) => {
                        st.forget_before(cmd.idempotency_floor_us);
                        let (failed, reply) = match cmd.kind {
                            Some(proto::command::Kind::Mutation(m)) => (false, m.reply),
                            Some(proto::command::Kind::Failure(f)) => (true, f.reply),
                            None => (true, Bytes::new()),
                        };
                        let rec = DecidedKey {
                            index: entry.log_id.index,
                            commit_time_us: cmd.commit_time_us,
                            failed,
                            reply: reply.clone(),
                        };
                        st.decide(cmd.idempotency_key, rec);
                        Applied::Decided {
                            failed,
                            commit_time_us: cmd.commit_time_us,
                            reply,
                        }
                    }
                };
                st.last_applied = Some(entry.log_id);
                out.push(applied);
            }
            st.last_applied.map(|l| l.index)
        };
        self.shared.applied.send_replace(last);
        Ok(out)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Vec<u8>>, StorageError> {
        Ok(Box::default())
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta,
        snapshot: Box<Vec<u8>>,
    ) -> Result<(), StorageError> {
        let err = |e: io::Error| {
            StorageIOError::write_snapshot(Some(meta.signature()), AnyError::new(&e))
        };
        self.persist_snapshot(meta, &snapshot).map_err(err)?;
        let last = {
            let mut st = self.shared.state.lock().unwrap();
            st.restore(meta, &snapshot).map_err(err)?;
            st.last_applied.map(|l| l.index)
        };
        self.shared.applied.send_replace(last);
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<TypeConfig>>, StorageError> {
        let st = self.shared.state.lock().unwrap();
        Ok(st.snapshot.as_ref().map(|(meta, data)| Snapshot {
            meta: meta.clone(),
            snapshot: Box::new(data.clone()),
        }))
    }
}
