//! Applies a group's committed entries to its [`Machine`]: unwraps each
//! command, voids those appended under another term than they were admitted
//! in, and records idempotency keys.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::rc::Rc;

use shep_codec::{Decode, Encode};
use shep_futures::Stream;
use shep_futures::StreamExt;
use shep_raft_engine::EntryPayload;
use shep_raft_engine::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine, Snapshot};

use crate::machine::{Applied, Decided, Envelope, Machine};
use crate::types::{LogId, RaftSnapshot, SnapshotMeta, StoredMembership, TypeConfig};

/// Adapts a [`Machine`] to the engine. The engine, its snapshot builder and
/// the group's proposer share it.
pub(crate) struct SmAdapter<M: Machine> {
    state: Rc<RefCell<SmState<M>>>,
}

impl<M: Machine> Clone for SmAdapter<M> {
    fn clone(&self) -> Self {
        SmAdapter {
            state: self.state.clone(),
        }
    }
}

struct SmState<M: Machine> {
    machine: M,
    keys: Keys<M::Response>,
    /// The latest commit time of any entry applied, void or not: a new
    /// leader's commit times start above it, so they never go backwards
    /// across terms, however far back it replays.
    max_commit_time_us: u64,
    /// The last index applied, for whoever waits on it.
    applied: tokio::sync::watch::Sender<Option<u64>>,
    last_applied: Option<LogId>,
    membership: StoredMembership,
    snapshot: Option<(SnapshotMeta, Vec<u8>)>,
}

/// Which entry decided each idempotency key, for the idempotency window.
struct Keys<R> {
    decided: HashMap<Vec<u8>, KeyRecord<R>>,
    /// Keys in the order they were decided, for forgetting them.
    order: VecDeque<(u64, Vec<u8>)>,
}

#[derive(Clone, Debug, Encode, Decode)]
struct KeyRecord<R> {
    index: u64,
    commit_time_us: u64,
    response: R,
}

#[derive(Encode, Decode)]
struct SnapshotData<R> {
    keys: Vec<(Vec<u8>, KeyRecord<R>)>,
    machine: Vec<u8>,
    max_commit_time_us: u64,
}

impl<R: Clone> Keys<R> {
    fn new() -> Keys<R> {
        Keys {
            decided: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn forget_before(&mut self, floor_us: u64) {
        while let Some((index, key)) = self.order.front() {
            match self.decided.get(key) {
                // Superseded or gone: only the newest record counts.
                Some(rec) if rec.index != *index => {}
                None => {}
                Some(rec) if rec.commit_time_us >= floor_us => break,
                Some(_) => {
                    self.decided.remove(key);
                }
            }
            self.order.pop_front();
        }
    }

    fn decide(&mut self, key: Vec<u8>, rec: KeyRecord<R>) {
        if let Some(prev) = self.decided.get(&key) {
            // The leader checks keys before appending, so this is a bug.
            tracing::error!(
                ?key,
                prev = prev.index,
                index = rec.index,
                "idempotency key decided twice"
            );
        }
        self.order.push_back((rec.index, key.clone()));
        self.decided.insert(key, rec);
    }
}

impl<M: Machine> SmAdapter<M> {
    pub(crate) fn new(machine: M) -> SmAdapter<M> {
        SmAdapter {
            state: Rc::new(RefCell::new(SmState {
                machine,
                keys: Keys::new(),
                max_commit_time_us: 0,
                applied: tokio::sync::watch::channel(None).0,
                last_applied: None,
                membership: StoredMembership::default(),
                snapshot: None,
            })),
        }
    }

    /// Runs `f` with the machine, which must not hold the borrow across an
    /// await.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut M) -> R) -> R {
        f(&mut self.state.borrow_mut().machine)
    }

    pub(crate) fn subscribe_applied(&self) -> tokio::sync::watch::Receiver<Option<u64>> {
        self.state.borrow().applied.subscribe()
    }

    /// The latest commit time of any applied entry.
    pub(crate) fn max_commit_time_us(&self) -> u64 {
        self.state.borrow().max_commit_time_us
    }

    /// The decision for `key`, if an applied entry made one in the window.
    pub(crate) fn lookup(&self, key: &[u8]) -> Option<Decided<M::Response>> {
        let st = self.state.borrow();
        st.keys.decided.get(key).map(|r| Decided {
            index: r.index,
            commit_time_us: r.commit_time_us,
            response: r.response.clone(),
            replayed: true,
        })
    }
}

impl<M: Machine> SmState<M> {
    fn apply_normal(&mut self, index: u64, term: u64, bytes: Vec<u8>) -> Applied<M::Response> {
        let env = match shep_codec::from_bytes::<Envelope<M::Command>>(&bytes) {
            Ok(env) => env,
            Err(e) => {
                // Only the leader writes entries, after encoding them.
                tracing::error!(index, "undecodable entry: {e}");
                return Applied::Void;
            }
        };
        self.max_commit_time_us = self.max_commit_time_us.max(env.commit_time_us);
        if env.is_void(term) {
            return Applied::Void;
        }
        self.keys.forget_before(env.idempotency_floor_us);
        let entry = env.into_entry(index, term);
        let response = self.machine.apply(&entry);
        if let Some(key) = entry.key {
            self.keys.decide(
                key,
                KeyRecord {
                    index,
                    commit_time_us: entry.commit_time_us,
                    response: response.clone(),
                },
            );
        }
        Applied::Decided {
            commit_time_us: entry.commit_time_us,
            response,
        }
    }
}

impl<M: Machine> RaftStateMachine<TypeConfig> for SmAdapter<M> {
    type SnapshotData = Vec<u8>;
    type SnapshotBuilder = SmAdapter<M>;

    async fn applied_state(&mut self) -> io::Result<(Option<LogId>, StoredMembership)> {
        let st = self.state.borrow();
        Ok((st.last_applied, st.membership.clone()))
    }

    async fn apply<Strm>(&mut self, mut entries: Strm) -> io::Result<()>
    where
        Strm: Stream<Item = io::Result<EntryResponder<TypeConfig>>> + Unpin,
    {
        while let Some(next) = entries.next().await {
            let (entry, responder) = next?;
            let mut st = self.state.borrow_mut();
            let (index, term) = (entry.log_id.index, entry.log_id.leader_id.term);
            let applied = match entry.payload {
                EntryPayload::Blank => Applied::Internal,
                EntryPayload::Normal(d) => st.apply_normal(index, term, d.0),
                EntryPayload::Membership(m) => {
                    st.membership = StoredMembership::new(Some(entry.log_id), m);
                    Applied::Internal
                }
            };
            st.last_applied = Some(entry.log_id);
            st.applied.send_replace(Some(index));
            drop(st);
            if let Some(responder) = responder {
                responder.send(crate::types::Payload(shep_codec::to_bytes(&applied)));
            }
        }
        Ok(())
    }

    async fn get_snapshot_builder(&mut self) -> SmAdapter<M> {
        self.clone()
    }

    async fn install_snapshot(&mut self, meta: &SnapshotMeta, snapshot: Vec<u8>) -> io::Result<()> {
        let data: SnapshotData<M::Response> = shep_codec::from_bytes(&snapshot.clone())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut st = self.state.borrow_mut();
        st.machine.restore(data.machine);
        st.max_commit_time_us = data.max_commit_time_us;
        st.keys = Keys::new();
        for (key, rec) in data.keys {
            st.keys.decide(key, rec);
        }
        st.last_applied = meta.last_log_id;
        st.applied.send_replace(meta.last_log_id.map(|l| l.index));
        st.membership = meta.last_membership.clone();
        st.snapshot = Some((meta.clone(), snapshot));
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> io::Result<Option<RaftSnapshot>> {
        let st = self.state.borrow();
        Ok(st.snapshot.as_ref().map(|(meta, data)| Snapshot {
            meta: meta.clone(),
            snapshot: data.clone(),
        }))
    }
}

impl<M: Machine> RaftSnapshotBuilder<TypeConfig> for SmAdapter<M> {
    type SnapshotData = Vec<u8>;

    async fn build_snapshot(&mut self) -> io::Result<RaftSnapshot> {
        let mut st = self.state.borrow_mut();
        let mut keys: Vec<_> = st
            .keys
            .decided
            .iter()
            .map(|(k, r)| (k.clone(), r.clone()))
            .collect();
        keys.sort_by_key(|(_, r)| r.index);
        let data = shep_codec::to_bytes(&SnapshotData {
            keys,
            machine: st.machine.snapshot(),
            max_commit_time_us: st.max_commit_time_us,
        });
        let meta = SnapshotMeta {
            last_log_id: st.last_applied,
            last_membership: st.membership.clone(),
        };
        st.snapshot = Some((meta.clone(), data.clone()));
        Ok(Snapshot {
            meta,
            snapshot: data,
        })
    }
}
