//! openraft's type configuration for a partition's Raft group, and the
//! conversions between openraft's types and the peer protocol.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use openraft::{BasicNode, EntryPayload, LeaderId, LogId, Membership, StoredMembership, Vote};
use shep_wal_api::proto as api;

pub(crate) mod proto {
    tonic::include_proto!("shep.wal.raft.v1");
}

openraft::declare_raft_types!(
    pub(crate) TypeConfig:
        D = proto::Command,
        R = Applied,
        NodeId = u64,
        Node = BasicNode,
        Entry = openraft::Entry<TypeConfig>,
        SnapshotData = Vec<u8>,
        AsyncRuntime = openraft::TokioRuntime,
        Responder = openraft::impls::OneshotResponder<TypeConfig>,
);

pub(crate) type Raft = openraft::Raft<TypeConfig>;
pub(crate) type Entry = openraft::Entry<TypeConfig>;
pub(crate) type SnapshotMeta = openraft::SnapshotMeta<u64, BasicNode>;
pub(crate) type StorageError = openraft::StorageError<u64>;

/// What applying an entry decided.
#[derive(Clone, Debug)]
pub(crate) enum Applied {
    /// Raft's own entries.
    Internal,
    /// Appended under a term other than the one it was validated in.
    Void,
    /// The entry decided its idempotency key.
    Decided {
        failed: bool,
        commit_time_us: u64,
        reply: Bytes,
    },
}

/// A command takes effect only in the term its leader validated it in.
pub(crate) fn is_void(log_id: &LogId<u64>, cmd: &proto::Command) -> bool {
    cmd.validated_term != log_id.leader_id.term
}

#[derive(Debug, thiserror::Error)]
#[error("malformed peer message: {0}")]
pub(crate) struct CodecError(pub String);

fn missing(field: &str) -> CodecError {
    CodecError(format!("missing `{field}`"))
}

pub(crate) fn leader_id_to_proto(l: &LeaderId<u64>) -> proto::LeaderId {
    proto::LeaderId {
        term: l.term,
        node_id: l.node_id,
    }
}

fn leader_id_from_proto(l: Option<proto::LeaderId>) -> Result<LeaderId<u64>, CodecError> {
    let l = l.ok_or_else(|| missing("leader_id"))?;
    Ok(LeaderId::new(l.term, l.node_id))
}

pub(crate) fn vote_to_proto(v: &Vote<u64>) -> proto::Vote {
    proto::Vote {
        leader_id: Some(leader_id_to_proto(&v.leader_id)),
        committed: v.committed,
    }
}

pub(crate) fn vote_from_proto(v: Option<proto::Vote>) -> Result<Vote<u64>, CodecError> {
    let v = v.ok_or_else(|| missing("vote"))?;
    let l = leader_id_from_proto(v.leader_id)?;
    Ok(if v.committed {
        Vote::new_committed(l.term, l.node_id)
    } else {
        Vote::new(l.term, l.node_id)
    })
}

pub(crate) fn log_id_to_proto(l: &LogId<u64>) -> proto::LogId {
    proto::LogId {
        leader_id: Some(leader_id_to_proto(&l.leader_id)),
        index: l.index,
    }
}

pub(crate) fn log_id_from_proto(l: proto::LogId) -> Result<LogId<u64>, CodecError> {
    Ok(LogId::new(leader_id_from_proto(l.leader_id)?, l.index))
}

pub(crate) fn opt_log_id_from_proto(
    l: Option<proto::LogId>,
) -> Result<Option<LogId<u64>>, CodecError> {
    l.map(log_id_from_proto).transpose()
}

pub(crate) fn membership_to_proto(m: &Membership<u64, BasicNode>) -> proto::Membership {
    proto::Membership {
        configs: m
            .get_joint_config()
            .iter()
            .map(|set| proto::NodeSet {
                ids: set.iter().copied().collect(),
            })
            .collect(),
        nodes: m
            .nodes()
            .map(|(&node_id, node)| api::Member {
                node_id,
                addr: node.addr.clone(),
            })
            .collect(),
    }
}

pub(crate) fn membership_from_proto(
    m: Option<proto::Membership>,
) -> Result<Membership<u64, BasicNode>, CodecError> {
    let m = m.ok_or_else(|| missing("membership"))?;
    let configs: Vec<BTreeSet<u64>> = m
        .configs
        .into_iter()
        .map(|s| s.ids.into_iter().collect())
        .collect();
    let nodes: BTreeMap<u64, BasicNode> = m
        .nodes
        .into_iter()
        .map(|n| (n.node_id, BasicNode::new(n.addr)))
        .collect();
    Ok(Membership::new(configs, nodes))
}

pub(crate) fn stored_membership_to_proto(
    m: &StoredMembership<u64, BasicNode>,
) -> proto::StoredMembership {
    proto::StoredMembership {
        log_id: m.log_id().as_ref().map(log_id_to_proto),
        membership: Some(membership_to_proto(m.membership())),
    }
}

pub(crate) fn stored_membership_from_proto(
    m: Option<proto::StoredMembership>,
) -> Result<StoredMembership<u64, BasicNode>, CodecError> {
    let m = m.ok_or_else(|| missing("stored membership"))?;
    Ok(StoredMembership::new(
        opt_log_id_from_proto(m.log_id)?,
        membership_from_proto(m.membership)?,
    ))
}

pub(crate) fn entry_to_proto(e: &Entry) -> proto::Entry {
    use proto::entry::Payload;
    let payload = match &e.payload {
        EntryPayload::Blank => Payload::Blank(proto::Blank {}),
        EntryPayload::Normal(cmd) => Payload::Normal(cmd.clone()),
        EntryPayload::Membership(m) => Payload::Membership(membership_to_proto(m)),
    };
    proto::Entry {
        log_id: Some(log_id_to_proto(&e.log_id)),
        payload: Some(payload),
    }
}

pub(crate) fn entry_from_proto(e: proto::Entry) -> Result<Entry, CodecError> {
    use proto::entry::Payload;
    let log_id = log_id_from_proto(e.log_id.ok_or_else(|| missing("Entry.log_id"))?)?;
    let payload = match e.payload.ok_or_else(|| missing("Entry.payload"))? {
        Payload::Blank(_) => EntryPayload::Blank,
        Payload::Normal(cmd) => EntryPayload::Normal(cmd),
        Payload::Membership(m) => EntryPayload::Membership(membership_from_proto(Some(m))?),
    };
    Ok(Entry { log_id, payload })
}

pub(crate) fn snapshot_meta_to_proto(m: &SnapshotMeta) -> proto::SnapshotMeta {
    proto::SnapshotMeta {
        last_log_id: m.last_log_id.as_ref().map(log_id_to_proto),
        last_membership: Some(stored_membership_to_proto(&m.last_membership)),
        snapshot_id: m.snapshot_id.clone(),
    }
}

pub(crate) fn snapshot_meta_from_proto(
    m: Option<proto::SnapshotMeta>,
) -> Result<SnapshotMeta, CodecError> {
    let m = m.ok_or_else(|| missing("snapshot meta"))?;
    Ok(SnapshotMeta {
        last_log_id: opt_log_id_from_proto(m.last_log_id)?,
        last_membership: stored_membership_from_proto(m.last_membership)?,
        snapshot_id: m.snapshot_id,
    })
}
