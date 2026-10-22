//! The leader's side of §10.3: validating proposals one at a time, in log
//! order, and handing them to Raft.
//!
//! One task per partition owns the validation table and the idempotency keys
//! of entries not yet applied. When this node becomes leader in a new term, it
//! rebuilds both from its own log (committed or not) before it accepts a
//! proposal. Every command records the term it was validated in, and an entry
//! appended under any other term is void (`raft_types::is_void`): that closes
//! the gap between reading the term from Raft's metrics and Raft appending the
//! entry, during which leadership could change.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use openraft::error::ClientWriteError;
use openraft::{EntryPayload, RaftMetrics, ServerState};
use prost::Message;
use shep_wal_api::{
    Decided, DecidedStatus, Dot, KeyRange, Member, PartitionId, Proposal, ProposalKind, Rejection,
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::WalConfig;
use crate::log_store::LogStore;
use crate::raft_types::{self, Applied, Raft, proto};
use crate::state_machine::StateMachine;
use crate::table::{SlotIndex, Table};

type Reply = oneshot::Sender<Result<Decided, Rejection>>;

/// Entries read at a time while rebuilding.
const REBUILD_CHUNK: u64 = 1024;

#[derive(Clone)]
pub(crate) struct ProposerHandle {
    tx: mpsc::UnboundedSender<Msg>,
}

impl ProposerHandle {
    pub(crate) async fn propose(&self, proposal: Proposal) -> Result<Decided, Rejection> {
        let (reply, rx) = oneshot::channel();
        let gone = || Rejection::UnknownOutcome("partition is shutting down".into());
        self.tx
            .send(Msg::Propose { proposal, reply })
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }
}

enum Msg {
    Propose {
        proposal: Proposal,
        reply: Reply,
    },
    /// Raft's answer for an entry this task handed it.
    Resolved {
        generation: u64,
        seq: u64,
        key: Bytes,
        outcome: Outcome,
    },
}

#[derive(Clone)]
enum Outcome {
    Applied { index: u64, applied: Applied },
    Lost(Rejection),
}

pub(crate) struct Context {
    pub(crate) partition: PartitionId,
    pub(crate) node_id: u64,
    pub(crate) raft: Raft,
    pub(crate) log: LogStore,
    pub(crate) sm: StateMachine,
    pub(crate) config: Arc<WalConfig>,
}

pub(crate) fn spawn(cx: Context) -> ProposerHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let proposer = Proposer {
        weak: tx.downgrade(),
        cx,
        leading: None,
        generation: 0,
    };
    tokio::spawn(proposer.run(rx));
    ProposerHandle { tx }
}

struct Proposer {
    cx: Context,
    weak: mpsc::WeakUnboundedSender<Msg>,
    leading: Option<Leading>,
    generation: u64,
}

/// What the task keeps while this node leads in one term.
struct Leading {
    generation: u64,
    table: Table,
    /// Keys of entries in the log that aren't applied yet.
    keys: HashMap<Bytes, KeyState>,
}

enum KeyState {
    /// Handed to Raft by this task. Retries wait for its answer here.
    Waiting {
        seq: u64,
        done: watch::Sender<Option<Result<Decided, Rejection>>>,
    },
    /// Found in the log when the table was rebuilt.
    AtIndex(u64),
}

impl Proposer {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Propose { proposal, reply } => self.propose(proposal, reply).await,
                Msg::Resolved {
                    generation,
                    seq,
                    key,
                    outcome,
                } => self.resolved(generation, seq, key, outcome),
            }
        }
    }

    async fn propose(&mut self, proposal: Proposal, reply: Reply) {
        let metrics = self.cx.raft.metrics().borrow().clone();
        if metrics.state != ServerState::Leader || metrics.current_leader != Some(self.cx.node_id) {
            self.leading = None;
            let _ = reply.send(Err(Rejection::NotLeader {
                leader: leader_of(&metrics),
            }));
            return;
        }
        // A leader that hasn't heard from a quorum for an election timeout has
        // probably been replaced. Accepting would be safe, since it can't
        // commit, but the sender would only learn that by timing out.
        let lease = self.cx.config.election_timeout.end;
        if metrics
            .millis_since_quorum_ack
            .is_some_and(|ms| Duration::from_millis(ms) > lease)
        {
            let _ = reply.send(Err(Rejection::NotLeader { leader: None }));
            return;
        }
        if self
            .leading
            .as_ref()
            .is_none_or(|l| l.table.term() != metrics.current_term)
        {
            match self.rebuild(metrics.current_term) {
                Ok(leading) => self.leading = Some(leading),
                Err(e) => {
                    self.leading = None;
                    let _ = reply.send(Err(Rejection::UnknownOutcome(format!(
                        "rebuilding the table: {e}"
                    ))));
                    return;
                }
            }
        }

        let cx = &self.cx;
        let leading = self.leading.as_mut().unwrap();
        let now = now_us();
        leading.table.evict(
            now.saturating_sub(as_us(cx.config.validation_window)),
            cx.config.validation_window_entries,
        );

        // Idempotency comes first: a key with an outcome gets that outcome.
        let key = proposal.idempotency_key;
        if key.is_empty() {
            let _ = reply.send(Err(Rejection::Invalid("empty idempotency key".into())));
            return;
        }
        if let Some(decided) = cx.sm.lookup(&key) {
            let _ = reply.send(Ok(decided));
            return;
        }
        match leading.keys.get_mut(&key) {
            Some(KeyState::Waiting { done, .. }) => {
                tokio::spawn(wait_for_retry(
                    done.subscribe(),
                    cx.config.commit_timeout,
                    reply,
                ));
                return;
            }
            Some(KeyState::AtIndex(index)) => {
                tokio::spawn(wait_for_key(
                    cx.sm.clone(),
                    key,
                    *index,
                    cx.config.commit_timeout,
                    reply,
                ));
                return;
            }
            None => {}
        }

        let p = cx.partition;
        let (write_set, kind) = match proposal.kind {
            ProposalKind::Mutation(m) => {
                if !m.context.includes(&m.read_version) {
                    let _ = reply.send(Err(Rejection::Invalid(
                        "context must include the read version".into(),
                    )));
                    return;
                }
                let last = cx.log.last_log_id().map_or(0, |l| l.index);
                if m.read_version.prefix(p) > last || m.read_version.extras(p).any(|i| i > last) {
                    let _ = reply.send(Err(Rejection::Invalid(
                        "read version names entries past the log's end".into(),
                    )));
                    return;
                }
                if m.read_version.prefix(p) < leading.table.floor() {
                    let _ = reply.send(Err(Rejection::TooOld));
                    return;
                }
                if leading.table.conflicts(&m.read_set, &m.read_version) {
                    let _ = reply.send(Err(Rejection::Contended));
                    return;
                }
                let kind = proto::command::Kind::Mutation(proto::MutationCommand {
                    context: Some((&m.context).into()),
                    write_set: m.write_set.iter().map(Into::into).collect(),
                    payload: m.payload,
                    reply: m.reply,
                });
                (m.write_set, kind)
            }
            ProposalKind::Failure { reply } => (
                Vec::new(),
                proto::command::Kind::Failure(proto::FailureCommand { reply }),
            ),
        };

        let commit_time_us = leading.table.next_commit_time(now);
        let cmd = proto::Command {
            validated_term: leading.table.term(),
            commit_time_us,
            idempotency_floor_us: commit_time_us
                .saturating_sub(as_us(cx.config.idempotency_window)),
            idempotency_key: key.clone(),
            kind: Some(kind),
        };
        // Any one entry must fit in a replication or tail batch.
        let limit = cx.config.max_message_size / 2;
        if cmd.encoded_len() > limit {
            let msg = format!(
                "proposal too large: {} bytes, limit {limit}",
                cmd.encoded_len()
            );
            let _ = reply.send(Err(Rejection::Invalid(msg)));
            return;
        }
        let seq = leading
            .table
            .push(SlotIndex::Pending, &write_set, commit_time_us);
        leading.keys.insert(
            key.clone(),
            KeyState::Waiting {
                seq,
                done: watch::channel(None).0,
            },
        );

        // Raft appends in the order it receives entries, which is the order
        // they were validated in.
        match cx.raft.client_write_ff(cmd).await {
            Ok(rx) => {
                let generation = leading.generation;
                let weak = self.weak.clone();
                let timeout = cx.config.commit_timeout;
                tokio::spawn(async move {
                    let outcome = await_outcome(rx, timeout, reply, p).await;
                    if let Some(tx) = weak.upgrade() {
                        let _ = tx.send(Msg::Resolved {
                            generation,
                            seq,
                            key,
                            outcome,
                        });
                    }
                });
            }
            Err(_) => {
                // Raft is shutting down; nothing was appended.
                self.leading = None;
                let _ = reply.send(Err(Rejection::NotLeader { leader: None }));
            }
        }
    }

    fn resolved(&mut self, generation: u64, seq: u64, key: Bytes, outcome: Outcome) {
        let Some(leading) = self.leading.as_mut().filter(|l| l.generation == generation) else {
            return;
        };
        match &outcome {
            Outcome::Applied {
                index,
                applied: Applied::Void,
            } => leading.table.resolve(seq, SlotIndex::Void(*index)),
            Outcome::Applied { index, .. } => leading.table.resolve(seq, SlotIndex::Known(*index)),
            // Leadership changed, so this table is done with.
            Outcome::Lost(_) => {}
        }
        match leading.keys.remove(&key) {
            Some(KeyState::Waiting { seq: s, done }) if s == seq => {
                done.send_replace(Some(outcome_result(self.cx.partition, &outcome)));
            }
            Some(other) => {
                leading.keys.insert(key, other);
            }
            None => {}
        }
    }

    /// Builds the tables for a new term from every entry in this replica's
    /// log since the window's start, committed or not, plus every entry not
    /// yet applied, whose keys aren't in the state machine yet.
    fn rebuild(&mut self, term: u64) -> std::io::Result<Leading> {
        self.generation += 1;
        let cx = &self.cx;
        let lowest = cx.log.purged().map_or(0, |p| p.index + 1);
        let applied = cx.sm.applied_index();
        let cutoff = now_us().saturating_sub(as_us(cx.config.validation_window));
        let max = cx.config.validation_window_entries;

        let mut newest_first = Vec::new();
        let mut start = cx.log.last_log_id().map_or(lowest, |l| l.index + 1);
        let mut hi = start;
        'walk: while hi > lowest {
            let lo = hi.saturating_sub(REBUILD_CHUNK).max(lowest);
            for e in cx.log.read(lo..hi)?.into_iter().rev() {
                let unapplied = applied.is_none_or(|a| e.log_id.index > a);
                let old =
                    matches!(&e.payload, EntryPayload::Normal(c) if c.commit_time_us < cutoff);
                if !unapplied && (old || newest_first.len() >= max) {
                    break 'walk;
                }
                start = e.log_id.index;
                newest_first.push(e);
            }
            hi = lo;
        }

        // Everything above `start - 1` is in the table. With nothing purged,
        // index 0 is the group's first membership, which wrote nothing.
        let mut table = Table::new(cx.partition, term, start.saturating_sub(1));
        let mut keys = HashMap::new();
        for e in newest_first.into_iter().rev() {
            let EntryPayload::Normal(cmd) = &e.payload else {
                continue;
            };
            table.observe_commit_time(cmd.commit_time_us);
            if raft_types::is_void(&e.log_id, cmd) {
                continue;
            }
            let write_set: Vec<KeyRange> = match &cmd.kind {
                Some(proto::command::Kind::Mutation(m)) => {
                    m.write_set.iter().cloned().map(Into::into).collect()
                }
                _ => Vec::new(),
            };
            table.push(
                SlotIndex::Known(e.log_id.index),
                &write_set,
                cmd.commit_time_us,
            );
            if applied.is_none_or(|a| e.log_id.index > a) {
                keys.insert(
                    cmd.idempotency_key.clone(),
                    KeyState::AtIndex(e.log_id.index),
                );
            }
        }
        tracing::debug!(partition = %cx.partition, term, floor = table.floor(), "rebuilt validation table");
        Ok(Leading {
            generation: self.generation,
            table,
            keys,
        })
    }
}

/// Waits for Raft's answer for an entry. The client is told after
/// `timeout` at the latest, but the wait goes on, because the table needs the
/// entry's index.
async fn await_outcome(
    rx: tokio::sync::oneshot::Receiver<
        openraft::raft::ClientWriteResult<crate::raft_types::TypeConfig>,
    >,
    timeout: Duration,
    reply: Reply,
    partition: PartitionId,
) -> Outcome {
    let mut reply = Some(reply);
    tokio::pin!(rx);
    let res = tokio::select! {
        res = &mut rx => res,
        _ = tokio::time::sleep(timeout) => {
            let r = Rejection::UnknownOutcome(format!("not committed within {timeout:?}"));
            let _ = reply.take().unwrap().send(Err(r));
            rx.await
        }
    };
    let outcome = match res {
        Ok(Ok(resp)) => Outcome::Applied {
            index: resp.log_id.index,
            applied: resp.data,
        },
        Ok(Err(ClientWriteError::ForwardToLeader(f))) => Outcome::Lost(Rejection::NotLeader {
            leader: f.leader_id.zip(f.leader_node).map(|(node_id, n)| Member {
                node_id,
                addr: n.addr,
            }),
        }),
        Ok(Err(e)) => Outcome::Lost(Rejection::UnknownOutcome(e.to_string())),
        Err(_) => Outcome::Lost(Rejection::UnknownOutcome("leadership changed".into())),
    };
    if let Some(reply) = reply {
        let _ = reply.send(outcome_result(partition, &outcome));
    }
    outcome
}

fn outcome_result(partition: PartitionId, outcome: &Outcome) -> Result<Decided, Rejection> {
    match outcome {
        Outcome::Applied {
            index,
            applied:
                Applied::Decided {
                    failed,
                    commit_time_us,
                    reply,
                },
        } => Ok(Decided {
            dot: Dot::new(partition, *index),
            commit_time_us: *commit_time_us,
            status: if *failed {
                DecidedStatus::Failed
            } else {
                DecidedStatus::Committed
            },
            reply: reply.clone(),
        }),
        Outcome::Applied {
            applied: Applied::Void,
            ..
        } => Err(Rejection::UnknownOutcome(
            "appended under a later term; send it again".into(),
        )),
        Outcome::Applied {
            applied: Applied::Internal,
            ..
        } => Err(Rejection::UnknownOutcome(
            "unexpected internal entry".into(),
        )),
        Outcome::Lost(r) => Err(r.clone()),
    }
}

/// Answers a retry of a key this task handed to Raft and hasn't heard back
/// about.
async fn wait_for_retry(
    mut rx: watch::Receiver<Option<Result<Decided, Rejection>>>,
    timeout: Duration,
    reply: Reply,
) {
    let result = match tokio::time::timeout(timeout, rx.wait_for(Option::is_some)).await {
        Ok(Ok(r)) => r.clone().unwrap(),
        Ok(Err(_)) => Err(Rejection::UnknownOutcome("leadership changed".into())),
        Err(_) => Err(Rejection::UnknownOutcome(format!(
            "not committed within {timeout:?}"
        ))),
    };
    let _ = reply.send(result);
}

/// Answers a retry of a key whose entry is in the log but not yet applied.
async fn wait_for_key(sm: StateMachine, key: Bytes, index: u64, timeout: Duration, reply: Reply) {
    let result = match tokio::time::timeout(timeout, sm.wait_applied(index)).await {
        Ok(()) => sm
            .lookup(&key)
            .ok_or_else(|| Rejection::UnknownOutcome("its entry was void; send it again".into())),
        Err(_) => Err(Rejection::UnknownOutcome(format!(
            "not committed within {timeout:?}"
        ))),
    };
    let _ = reply.send(result);
}

fn leader_of(m: &RaftMetrics<u64, openraft::BasicNode>) -> Option<Member> {
    let id = m.current_leader?;
    let node = m.membership_config.membership().get_node(&id)?;
    Some(Member {
        node_id: id,
        addr: node.addr.clone(),
    })
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

fn as_us(d: Duration) -> u64 {
    d.as_micros().min(u64::MAX as u128) as u64
}
