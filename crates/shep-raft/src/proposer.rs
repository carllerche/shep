//! The leader's side of proposing: admitting proposals one at a time, in log
//! order, and handing them to Raft.
//!
//! One task per group, on the group's core, owns the leader-side state: the
//! machine's, through [`Machine::admit`], [`Machine::replay`] and
//! [`Machine::resolve`], and the idempotency keys of entries not yet applied.
//! When the replica starts leading a new term, the task rebuilds both from
//! the replica's own log (committed or not) before it admits anything. Every
//! command records the term it was admitted in, and an entry appended under
//! any other term is void: that closes the gap between this task reading the
//! term and Raft appending the entry, during which leadership could change.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot, watch};

use crate::machine::{Admit, Applied, Decided, Envelope, Machine, ProposeError, Resolution};
use crate::node::{GroupHandle, RaftConfig, RawPayload, WriteError, Written};

type Outcome<M> =
    Result<Decided<<M as Machine>::Response>, ProposeError<<M as Machine>::Rejection>>;
type Reply<M> = oneshot::Sender<Outcome<M>>;

/// Entries read at a time while rebuilding.
const REBUILD_CHUNK: u64 = 1024;

pub(crate) struct ProposerHandle<M: Machine> {
    tx: mpsc::UnboundedSender<Msg<M>>,
}

impl<M: Machine> Clone for ProposerHandle<M> {
    fn clone(&self) -> Self {
        ProposerHandle {
            tx: self.tx.clone(),
        }
    }
}

impl<M: Machine> ProposerHandle<M> {
    pub(crate) async fn propose(&self, key: Option<Vec<u8>>, proposal: M::Proposal) -> Outcome<M> {
        let (reply, rx) = oneshot::channel();
        let gone = || ProposeError::Unknown("the group is shutting down".into());
        self.tx
            .send(Msg::Propose {
                key,
                proposal,
                reply,
            })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }
}

enum Msg<M: Machine> {
    Propose {
        key: Option<Vec<u8>>,
        proposal: M::Proposal,
        reply: Reply<M>,
    },
    /// Raft's answer for an entry this task handed it.
    Resolved {
        generation: u64,
        seq: u64,
        key: Option<Vec<u8>>,
        fate: Fate<M>,
    },
}

/// What Raft did with an entry.
enum Fate<M: Machine> {
    Applied {
        index: u64,
        applied: Applied<M::Response>,
    },
    /// Refused before it was appended.
    Refused(ProposeError<M::Rejection>),
    /// It may or may not have been appended.
    Lost(ProposeError<M::Rejection>),
}

pub(crate) fn spawn<M: Machine>(
    group: GroupHandle<M>,
    config: Arc<RaftConfig>,
) -> ProposerHandle<M> {
    let (tx, rx) = mpsc::unbounded_channel();
    let proposer = Proposer {
        weak: tx.downgrade(),
        group,
        config,
        leading: None,
        generation: 0,
    };
    tokio::task::spawn_local(proposer.run(rx));
    ProposerHandle { tx }
}

struct Proposer<M: Machine> {
    group: GroupHandle<M>,
    config: Arc<RaftConfig>,
    weak: mpsc::WeakUnboundedSender<Msg<M>>,
    leading: Option<Leading<M>>,
    generation: u64,
}

/// What the task keeps while the replica leads one term.
struct Leading<M: Machine> {
    generation: u64,
    term: u64,
    next_seq: u64,
    last_commit_time_us: u64,
    /// Keys of entries in the log that aren't applied yet.
    keys: HashMap<Vec<u8>, KeyState<M>>,
}

enum KeyState<M: Machine> {
    /// Handed to Raft by this task. Retries wait for its answer here.
    Waiting {
        seq: u64,
        done: watch::Sender<Option<Outcome<M>>>,
    },
    /// Found in the log when leader-side state was rebuilt.
    AtIndex(u64),
}

impl<M: Machine> Proposer<M> {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg<M>>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Propose {
                    key,
                    proposal,
                    reply,
                } => self.propose(key, proposal, reply).await,
                Msg::Resolved {
                    generation,
                    seq,
                    key,
                    fate,
                } => self.resolved(generation, seq, key, fate),
            }
        }
    }

    async fn propose(&mut self, key: Option<Vec<u8>>, proposal: M::Proposal, reply: Reply<M>) {
        let lead = self.group.leadership();
        if !lead.is_leader {
            self.leading = None;
            let _ = reply.send(Err(ProposeError::NotLeader {
                leader: lead.leader,
            }));
            return;
        }
        if self.leading.as_ref().is_none_or(|l| l.term != lead.term) {
            match self.rebuild(lead.term).await {
                Ok(leading) => self.leading = Some(leading),
                Err(e) => {
                    self.leading = None;
                    let _ = reply.send(Err(ProposeError::Unknown(format!(
                        "rebuilding leader state: {e}"
                    ))));
                    return;
                }
            }
        }
        let config = self.config.clone();
        let timeout = config.proposals.commit_timeout;
        let leading = self.leading.as_mut().unwrap();

        // Idempotency comes first: a key with an outcome gets that outcome.
        let key = key.filter(|k| !k.is_empty());
        if let Some(key) = &key {
            if let Some(decided) = self.group.lookup(key) {
                let _ = reply.send(Ok(decided));
                return;
            }
            match leading.keys.get(key) {
                Some(KeyState::Waiting { done, .. }) => {
                    tokio::task::spawn_local(wait_for_retry::<M>(done.subscribe(), timeout, reply));
                    return;
                }
                Some(KeyState::AtIndex(index)) => {
                    let (g, key, index) = (self.group.clone(), key.clone(), *index);
                    tokio::task::spawn_local(wait_for_key(g, key, index, timeout, reply));
                    return;
                }
                None => {}
            }
        }

        let now = now_us();
        leading.last_commit_time_us = leading.last_commit_time_us.max(now);
        let commit_time_us = leading.last_commit_time_us;
        let seq = leading.next_seq;
        leading.next_seq += 1;
        let admit = Admit {
            term: leading.term,
            seq,
            commit_time_us,
            next_index: self.group.log_range().end,
            window_start_us: now.saturating_sub(as_us(config.proposals.admission_window)),
            window_entries: config.proposals.admission_window_entries,
        };
        let command = match self.group.with_state_machine(|m| m.admit(&admit, proposal)) {
            Ok(command) => command,
            Err(rejection) => {
                let _ = reply.send(Err(ProposeError::Rejected(rejection)));
                return;
            }
        };
        let payload = shep_codec::to_bytes(&Envelope {
            validated_term: leading.term,
            commit_time_us,
            idempotency_floor_us: commit_time_us
                .saturating_sub(as_us(config.proposals.idempotency_window)),
            key: key.clone().unwrap_or_default(),
            command,
        });
        // Any one entry must fit in a replication or fetch batch.
        let limit = config.proposals.max_entry_bytes;
        if payload.len() > limit {
            self.group
                .with_state_machine(|m| m.resolve(seq, Resolution::NotAppended));
            let _ = reply.send(Err(ProposeError::TooLarge {
                bytes: payload.len(),
                limit,
            }));
            return;
        }
        if let Some(key) = &key {
            leading.keys.insert(
                key.clone(),
                KeyState::Waiting {
                    seq,
                    done: watch::channel(None).0,
                },
            );
        }

        // Raft appends in the order it's handed entries, which is the order
        // they were admitted in.
        match self.group.append(payload).await {
            Ok(written) => {
                let generation = leading.generation;
                let weak = self.weak.clone();
                tokio::task::spawn_local(async move {
                    let fate = await_fate::<M>(written, timeout, reply).await;
                    if let Some(tx) = weak.upgrade() {
                        let _ = tx.send(Msg::Resolved {
                            generation,
                            seq,
                            key,
                            fate,
                        });
                    }
                });
            }
            Err(e) => {
                // Raft is stopping; nothing was appended.
                self.leading = None;
                let _ = reply.send(Err(propose_error(e)));
            }
        }
    }

    fn resolved(&mut self, generation: u64, seq: u64, key: Option<Vec<u8>>, fate: Fate<M>) {
        let Some(leading) = self.leading.as_mut().filter(|l| l.generation == generation) else {
            return;
        };
        let resolution = match &fate {
            Fate::Applied {
                index,
                applied: Applied::Void,
            } => Resolution::Void(*index),
            Fate::Applied { index, .. } => Resolution::Appended(*index),
            Fate::Refused(_) => Resolution::NotAppended,
            Fate::Lost(_) => Resolution::Unknown,
        };
        self.group
            .with_state_machine(|m| m.resolve(seq, resolution));
        let Some(key) = key else { return };
        match leading.keys.remove(&key) {
            Some(KeyState::Waiting { seq: s, done }) if s == seq => {
                done.send_replace(Some(outcome(&fate, true)));
            }
            Some(other) => {
                leading.keys.insert(key, other);
            }
            None => {}
        }
    }

    /// Rebuilds leader-side state for a new term from the replica's log:
    /// every entry since the admission window's start, committed or not,
    /// plus every entry not yet applied, whose keys aren't decided yet.
    async fn rebuild(&mut self, term: u64) -> Result<Leading<M>, String> {
        self.generation += 1;
        let range = self.group.log_range();
        let lowest = range.start;
        let applied = self.group.applied_index();
        let proposals = &self.config.proposals;
        let cutoff = now_us().saturating_sub(as_us(proposals.admission_window));
        let max = proposals.admission_window_entries;

        let mut newest_first = Vec::new();
        let mut start = range.end;
        let mut hi = range.end;
        'walk: while hi > lowest {
            let lo = hi.saturating_sub(REBUILD_CHUNK).max(lowest);
            let mut chunk = Vec::new();
            let mut next = lo;
            while next < hi {
                let records = self
                    .group
                    .read_raw(next..hi, usize::MAX)
                    .await
                    .map_err(|e| e.to_string())?;
                let Some(last) = records.last() else { break };
                next = last.index + 1;
                chunk.extend(records);
            }
            for r in chunk.into_iter().rev() {
                let env = match r.payload {
                    RawPayload::Normal(b) => {
                        match shep_codec::from_bytes::<Envelope<M::Command>>(&b) {
                            Ok(env) => Some(env),
                            Err(e) => {
                                tracing::error!(index = r.index, "undecodable entry: {e}");
                                None
                            }
                        }
                    }
                    RawPayload::Internal => None,
                };
                let unapplied = applied.is_none_or(|a| r.index > a);
                let old = env.as_ref().is_some_and(|e| e.commit_time_us < cutoff);
                if !unapplied && (old || newest_first.len() >= max) {
                    break 'walk;
                }
                start = r.index;
                newest_first.push((r.index, r.term, env));
            }
            hi = lo;
        }

        // Every entry above `start - 1` is replayed.
        let floor = start.saturating_sub(1);
        self.group.with_state_machine(|m| m.lead(term, floor));
        // Commit times continue from every entry applied here, not only the
        // replayed tail: entries below the floor may be later than this
        // node's clock.
        let mut last_commit_time_us = self.group.max_commit_time_us();
        let mut keys = HashMap::new();
        for (index, entry_term, env) in newest_first.into_iter().rev() {
            let Some(env) = env else { continue };
            last_commit_time_us = last_commit_time_us.max(env.commit_time_us);
            if env.is_void(entry_term) {
                continue;
            }
            let entry = env.into_entry(index, entry_term);
            if let Some(key) = &entry.key
                && applied.is_none_or(|a| index > a)
            {
                keys.insert(key.clone(), KeyState::AtIndex(index));
            }
            self.group.with_state_machine(|m| m.replay(&entry));
        }
        tracing::debug!(group = %self.group.id(), term, floor, "rebuilt leader state");
        Ok(Leading {
            generation: self.generation,
            term,
            next_seq: 0,
            last_commit_time_us,
            keys,
        })
    }
}

fn propose_error<J>(e: WriteError) -> ProposeError<J> {
    match e {
        WriteError::NotLeader { leader } => ProposeError::NotLeader { leader },
        WriteError::Discarded { .. } => {
            ProposeError::Unknown("appended, then discarded; send it again".into())
        }
        WriteError::Other(m) => ProposeError::Unknown(m),
    }
}

/// Waits for Raft's answer for an entry. The proposer is told after
/// `timeout` at the latest, but the wait goes on, because the machine needs
/// to know the entry's fate.
async fn await_fate<M: Machine>(
    written: impl std::future::Future<Output = Result<Written, WriteError>>,
    timeout: Duration,
    reply: Reply<M>,
) -> Fate<M> {
    let mut reply = Some(reply);
    tokio::pin!(written);
    let res = tokio::select! {
        res = &mut written => res,
        _ = tokio::time::sleep(timeout) => {
            let e = ProposeError::Unknown(format!("not committed within {timeout:?}"));
            let _ = reply.take().unwrap().send(Err(e));
            written.await
        }
    };
    let fate = match res {
        Ok(w) => match shep_codec::from_bytes::<Applied<M::Response>>(&w.response) {
            Ok(applied) => Fate::Applied {
                index: w.index,
                applied,
            },
            Err(e) => Fate::Lost(ProposeError::Unknown(format!(
                "undecodable apply result: {e}"
            ))),
        },
        Err(e @ WriteError::NotLeader { .. }) => Fate::Refused(propose_error(e)),
        Err(e) => Fate::Lost(propose_error(e)),
    };
    if let Some(reply) = reply {
        let _ = reply.send(outcome(&fate, false));
    }
    fate
}

fn outcome<M: Machine>(fate: &Fate<M>, replayed: bool) -> Outcome<M> {
    match fate {
        Fate::Applied {
            index,
            applied:
                Applied::Decided {
                    commit_time_us,
                    response,
                },
        } => Ok(Decided {
            index: *index,
            commit_time_us: *commit_time_us,
            response: response.clone(),
            replayed,
        }),
        Fate::Applied {
            applied: Applied::Void,
            ..
        } => Err(ProposeError::Unknown(
            "appended under a later term; send it again".into(),
        )),
        Fate::Applied {
            applied: Applied::Internal,
            ..
        } => Err(ProposeError::Unknown("unexpected internal entry".into())),
        Fate::Refused(e) | Fate::Lost(e) => Err(e.clone()),
    }
}

/// Answers a retry of a key this task handed to Raft and hasn't heard back
/// about.
async fn wait_for_retry<M: Machine>(
    mut rx: watch::Receiver<Option<Outcome<M>>>,
    timeout: Duration,
    reply: Reply<M>,
) {
    let result = match tokio::time::timeout(timeout, rx.wait_for(Option::is_some)).await {
        Ok(Ok(r)) => r.clone().unwrap(),
        Ok(Err(_)) => Err(ProposeError::Unknown("leadership changed".into())),
        Err(_) => Err(ProposeError::Unknown(format!(
            "not committed within {timeout:?}"
        ))),
    };
    let _ = reply.send(result);
}

/// Answers a retry of a key whose entry is in the log but not yet applied.
async fn wait_for_key<M: Machine>(
    g: GroupHandle<M>,
    key: Vec<u8>,
    index: u64,
    timeout: Duration,
    reply: Reply<M>,
) {
    let mut applied = g.subscribe_applied();
    let wait = applied.wait_for(|a| a.is_some_and(|a| a >= index));
    let result = match tokio::time::timeout(timeout, wait).await {
        Ok(Ok(_)) => g
            .lookup(&key)
            .ok_or_else(|| ProposeError::Unknown("its entry was void; send it again".into())),
        Ok(Err(_)) => Err(ProposeError::Unknown("the group stopped".into())),
        Err(_) => Err(ProposeError::Unknown(format!(
            "not committed within {timeout:?}"
        ))),
    };
    let _ = reply.send(result);
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

fn as_us(d: Duration) -> u64 {
    d.as_micros().min(u64::MAX as u128) as u64
}
