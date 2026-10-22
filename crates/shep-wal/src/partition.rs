//! One partition's replica on this node: its Raft group, log, state machine,
//! and proposer.

use std::path::Path;
use std::sync::Arc;

use openraft::{EntryPayload, SnapshotPolicy};
use shep_wal_api::{Dot, EntryKind, LogEntry, Member, MutationEntry, PartitionId, PartitionStatus};

use crate::log_store::LogStore;
use crate::network::{Faults, NetworkFactory, Peers};
use crate::proposer::{self, ProposerHandle};
use crate::raft_types::{self, Entry, Raft, proto};
use crate::state_machine::StateMachine;
use crate::{WalConfig, WalError};

pub(crate) struct Partition {
    pub(crate) id: PartitionId,
    pub(crate) raft: Raft,
    pub(crate) log: LogStore,
    pub(crate) sm: StateMachine,
    pub(crate) proposer: ProposerHandle,
}

impl Partition {
    pub(crate) async fn open(
        id: PartitionId,
        dir: &Path,
        config: Arc<WalConfig>,
        peers: Arc<Peers>,
        faults: Arc<Faults>,
    ) -> Result<Partition, WalError> {
        let log = LogStore::open(dir, config.segment_size)?;
        let sm = StateMachine::open(id, dir)?;
        let raft_config = openraft::Config {
            cluster_name: format!("shep-wal-{id}"),
            heartbeat_interval: config.heartbeat_interval.as_millis() as u64,
            election_timeout_min: config.election_timeout.start.as_millis() as u64,
            election_timeout_max: config.election_timeout.end.as_millis() as u64,
            // Every reader of the log needs it, so nothing is purged until
            // archiving exists (§10.4).
            snapshot_policy: SnapshotPolicy::Never,
            max_in_snapshot_log_to_keep: u64::MAX,
            ..Default::default()
        }
        .validate()
        .map_err(|e| WalError::Raft(e.to_string()))?;
        let network = NetworkFactory {
            partition: id,
            peers,
            faults,
        };
        let raft = Raft::new(
            config.node_id,
            Arc::new(raft_config),
            network,
            log.clone(),
            sm.clone(),
        )
        .await
        .map_err(|e| WalError::Raft(e.to_string()))?;
        let proposer = proposer::spawn(proposer::Context {
            partition: id,
            node_id: config.node_id,
            raft: raft.clone(),
            log: log.clone(),
            sm: sm.clone(),
            config,
        });
        Ok(Partition {
            id,
            raft,
            log,
            sm,
            proposer,
        })
    }

    pub(crate) fn status(&self) -> PartitionStatus {
        let m = self.raft.metrics().borrow().clone();
        let membership = m.membership_config.membership();
        let member = |id: u64| {
            membership.get_node(&id).map(|n| Member {
                node_id: id,
                addr: n.addr.clone(),
            })
        };
        PartitionStatus {
            node_id: m.id,
            term: m.current_term,
            leader: m.current_leader.and_then(member),
            last_log_index: self.log.last_log_id().map_or(0, |l| l.index),
            applied_index: self.sm.applied_index(),
            voters: membership.voter_ids().filter_map(member).collect(),
        }
    }

    /// A log entry as readers see it.
    pub(crate) fn log_entry(&self, e: &Entry) -> LogEntry {
        let kind = match &e.payload {
            EntryPayload::Blank | EntryPayload::Membership(_) => EntryKind::Internal,
            EntryPayload::Normal(cmd) if raft_types::is_void(&e.log_id, cmd) => EntryKind::Void,
            EntryPayload::Normal(cmd) => match &cmd.kind {
                Some(proto::command::Kind::Mutation(m)) => EntryKind::Mutation(MutationEntry {
                    context: m.context.clone().map(Into::into).unwrap_or_default(),
                    commit_time_us: cmd.commit_time_us,
                    idempotency_key: cmd.idempotency_key.clone(),
                    payload: m.payload.clone(),
                }),
                _ => EntryKind::Failure {
                    commit_time_us: cmd.commit_time_us,
                    idempotency_key: cmd.idempotency_key.clone(),
                },
            },
        };
        LogEntry {
            dot: Dot::new(self.id, e.log_id.index),
            term: e.log_id.leader_id.term,
            kind,
        }
    }
}
