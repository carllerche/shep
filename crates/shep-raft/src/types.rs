//! The engine's type configuration, and how entries are stored as records.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;

use shep_codec::{Decode, Encode};
use shep_raft_engine::impls::BasicNode;
use shep_raft_engine::impls::leader_id_std;
use shep_raft_engine::vote::RaftLeaderId;
use shep_raft_engine::{EntryPayload, Membership};
use shep_storage::Record;

shep_raft_engine::declare_raft_types!(
    /// Standard Raft (one leader per term), opaque payloads, nodes addressed
    /// by `host:port`.
    pub TypeConfig:
        D = Payload,
        R = Payload,
        NodeId = u64,
        Node = BasicNode,
        Term = u64,
        LeaderId = leader_id_std::LeaderId<u64, u64>,
);

/// An entry's application payload, and a state machine's response to it:
/// opaque to Raft.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Payload(pub Vec<u8>);

// The engine requires serde when its `serde` feature is on, which happens
// whenever its test-support crates are built alongside.
impl serde::Serialize for Payload {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Payload {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Payload, D::Error> {
        let v: Vec<u8> = serde::Deserialize::deserialize(d)?;
        Ok(Payload(v))
    }
}

impl fmt::Display for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0.len())
    }
}

type CommittedLeaderId = <leader_id_std::LeaderId<u64, u64> as RaftLeaderId>::Committed;

pub type NodeId = u64;
pub type Entry = <TypeConfig as shep_raft_engine::RaftTypeConfig>::Entry;
pub type LogId = shep_raft_engine::LogId<CommittedLeaderId>;
pub type Vote = shep_raft_engine::impls::Vote<leader_id_std::LeaderId<u64, u64>>;
pub type AppendResponse = shep_raft_engine::raft::AppendEntriesResponse<TypeConfig>;
pub type RaftMembership = Membership<u64, BasicNode>;
pub type StoredMembership = shep_raft_engine::StoredMembership<CommittedLeaderId, u64, BasicNode>;
pub type RaftSnapshot =
    shep_raft_engine::storage::Snapshot<CommittedLeaderId, u64, BasicNode, Vec<u8>>;
pub type SnapshotMeta = shep_raft_engine::SnapshotMeta<CommittedLeaderId, u64, BasicNode>;

/// A Raft group: a partition's or the controller's replicated log.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
#[codec(transparent)]
pub struct GroupId(pub u64);

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "g{}", self.0)
    }
}

pub fn log_id(term: u64, index: u64) -> LogId {
    LogId::new_term_index(term, index)
}

/// Record kinds for entry payloads.
const BLANK: u8 = 0;
const NORMAL: u8 = 1;
const MEMBERSHIP: u8 = 2;

pub(crate) fn entry_to_record(e: &Entry) -> Record {
    let (kind, payload) = match &e.payload {
        EntryPayload::Blank => (BLANK, Vec::<u8>::new()),
        EntryPayload::Normal(d) => (NORMAL, d.0.clone()),
        EntryPayload::Membership(m) => (MEMBERSHIP, shep_codec::to_bytes(&WireMembership::from(m))),
    };
    Record::new(e.log_id.index, e.log_id.leader_id.term, kind, payload)
}

pub(crate) fn record_to_entry(r: Record) -> io::Result<Entry> {
    let payload = match r.kind {
        BLANK => EntryPayload::Blank,
        NORMAL => EntryPayload::Normal(Payload(r.payload)),
        MEMBERSHIP => {
            let m: WireMembership = shep_codec::from_bytes(&r.payload).map_err(invalid)?;
            EntryPayload::Membership(m.into_membership()?)
        }
        k => return Err(invalid(format!("unknown entry kind {k}"))),
    };
    Ok(Entry {
        log_id: log_id(r.term, r.index),
        payload,
    })
}

pub(crate) fn invalid(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

// ---- wire forms of engine types ----

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) struct WireLogId {
    pub term: u64,
    pub index: u64,
}

impl From<&LogId> for WireLogId {
    fn from(l: &LogId) -> Self {
        WireLogId {
            term: l.leader_id.term,
            index: l.index,
        }
    }
}

impl From<WireLogId> for LogId {
    fn from(l: WireLogId) -> Self {
        log_id(l.term, l.index)
    }
}

pub(crate) fn opt_log_id(l: &Option<LogId>) -> Option<WireLogId> {
    l.as_ref().map(Into::into)
}

pub(crate) fn from_opt_log_id(l: Option<WireLogId>) -> Option<LogId> {
    l.map(Into::into)
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) struct WireVote {
    pub term: u64,
    pub voted_for: u64,
    pub committed: bool,
}

impl From<&Vote> for WireVote {
    fn from(v: &Vote) -> Self {
        WireVote {
            term: v.leader_id.term,
            voted_for: v.leader_id.voted_for,
            committed: v.committed,
        }
    }
}

impl From<WireVote> for Vote {
    fn from(v: WireVote) -> Self {
        if v.committed {
            Vote::new_committed(v.term, v.voted_for)
        } else {
            Vote::new(v.term, v.voted_for)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) struct WireMembership {
    pub configs: Vec<BTreeSet<u64>>,
    pub nodes: BTreeMap<u64, String>,
}

impl From<&RaftMembership> for WireMembership {
    fn from(m: &RaftMembership) -> Self {
        WireMembership {
            configs: m.get_joint_config().clone(),
            nodes: m.nodes().map(|(&id, n)| (id, n.addr.clone())).collect(),
        }
    }
}

impl WireMembership {
    pub(crate) fn into_membership(self) -> io::Result<RaftMembership> {
        let nodes: BTreeMap<u64, BasicNode> = self
            .nodes
            .into_iter()
            .map(|(id, addr)| (id, BasicNode::new(addr)))
            .collect();
        Membership::new(self.configs, nodes).map_err(invalid)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) struct WireEntry {
    pub term: u64,
    pub index: u64,
    pub kind: u8,
    pub payload: Vec<u8>,
}

impl From<&Entry> for WireEntry {
    fn from(e: &Entry) -> Self {
        let r = entry_to_record(e);
        WireEntry {
            term: r.term,
            index: r.index,
            kind: r.kind,
            payload: r.payload,
        }
    }
}

impl WireEntry {
    pub(crate) fn into_entry(self) -> io::Result<Entry> {
        record_to_entry(Record::new(self.index, self.term, self.kind, self.payload))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) struct WireStoredMembership {
    pub log_id: Option<WireLogId>,
    pub membership: WireMembership,
}

impl From<&StoredMembership> for WireStoredMembership {
    fn from(m: &StoredMembership) -> Self {
        WireStoredMembership {
            log_id: opt_log_id(m.log_id()),
            membership: m.membership().into(),
        }
    }
}

impl WireStoredMembership {
    pub(crate) fn into_stored(self) -> io::Result<StoredMembership> {
        Ok(StoredMembership::new(
            from_opt_log_id(self.log_id),
            self.membership.into_membership()?,
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub(crate) struct WireSnapshotMeta {
    pub last_log_id: Option<WireLogId>,
    pub last_membership: WireStoredMembership,
}

impl From<&SnapshotMeta> for WireSnapshotMeta {
    fn from(m: &SnapshotMeta) -> Self {
        WireSnapshotMeta {
            last_log_id: opt_log_id(&m.last_log_id),
            last_membership: (&m.last_membership).into(),
        }
    }
}

impl WireSnapshotMeta {
    pub(crate) fn into_meta(self) -> io::Result<SnapshotMeta> {
        Ok(SnapshotMeta {
            last_log_id: from_opt_log_id(self.last_log_id),
            last_membership: self.last_membership.into_stored()?,
        })
    }
}
