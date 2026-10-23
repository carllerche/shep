//! The WAL's client protocol (design §10.3), on shep-rpc.
//!
//! Front ends propose mutations to a partition's leader; materializers and
//! other readers fetch the committed log. This crate holds the messages, in
//! shep-codec, and the method ids. The WAL itself treats an entry's content
//! as opaque bytes: it validates only keys and key ranges.

use bytes::{BufMut, Bytes, BytesMut};
use shep_codec::{Decode, Encode};

pub use shep_version::{Dot, Dvv, PartitionId};

/// A WAL node's ID within its fleet.
pub type NodeId = u64;

/// The WAL service's methods (shep-rpc service 2).
pub mod method {
    pub const SERVICE: u16 = 2;
    pub const PROPOSE: u32 = shep_rpc::method_id(SERVICE, 1);
    pub const FETCH: u32 = shep_rpc::method_id(SERVICE, 2);
    pub const STATUS: u32 = shep_rpc::method_id(SERVICE, 3);
    pub const CREATE_PARTITION: u32 = shep_rpc::method_id(SERVICE, 4);
    pub const INITIALIZE_PARTITION: u32 = shep_rpc::method_id(SERVICE, 5);

    /// Whether `method` belongs to the WAL service.
    pub fn is_wal(method: u32) -> bool {
        shep_rpc::service_of(method) == SERVICE
    }
}

/// The version of the WAL's protocols (the WAL service's and the
/// controller's) this crate speaks.
pub const VERSION: u8 = 0;

/// The statuses of the WAL's services, the WAL and its controller.
pub mod status {
    /// The method isn't one the server knows.
    pub const UNKNOWN_METHOD: u32 = 1000;
    /// The request is at a version the server doesn't speak, or its body
    /// doesn't decode.
    pub const MALFORMED: u32 = 1002;
}

/// A range of keys in the WAL's conflict space: `[start, end)`, or
/// `[start, ∞)` without an end.
///
/// Read sets and write sets are lists of ranges. Front ends encode facts as
/// keys so that every range predicate a mutation reads is a key range (§10.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct KeyRange {
    start: Bytes,
    end: Option<Bytes>,
}

impl KeyRange {
    /// `[start, end)`.
    pub fn new(start: impl Into<Bytes>, end: impl Into<Bytes>) -> KeyRange {
        KeyRange {
            start: start.into(),
            end: Some(end.into()),
        }
    }

    /// `[start, ∞)`.
    pub fn starting_at(start: impl Into<Bytes>) -> KeyRange {
        KeyRange {
            start: start.into(),
            end: None,
        }
    }

    /// Exactly one key.
    pub fn point(key: impl Into<Bytes>) -> KeyRange {
        let start = key.into();
        let mut end = BytesMut::with_capacity(start.len() + 1);
        end.put_slice(&start);
        end.put_u8(0);
        KeyRange {
            start,
            end: Some(end.freeze()),
        }
    }

    /// Every key that starts with `prefix`.
    pub fn prefix(prefix: impl Into<Bytes>) -> KeyRange {
        let start = prefix.into();
        // The successor of a prefix: drop trailing 0xff bytes, then increment
        // the last byte. A prefix of only 0xff bytes has no successor.
        let mut end = start.to_vec();
        while end.last() == Some(&0xff) {
            end.pop();
        }
        match end.last_mut() {
            Some(last) => {
                *last += 1;
                KeyRange {
                    start,
                    end: Some(end.into()),
                }
            }
            None => KeyRange { start, end: None },
        }
    }

    pub fn start(&self) -> &Bytes {
        &self.start
    }

    /// The exclusive end, or `None` if the range is unbounded.
    pub fn end(&self) -> Option<&Bytes> {
        self.end.as_ref()
    }

    /// The key, if this range holds exactly one.
    pub fn as_point(&self) -> Option<&Bytes> {
        let end = self.end.as_ref()?;
        let is_point = end.len() == self.start.len() + 1
            && end.starts_with(&self.start)
            && end.last() == Some(&0);
        is_point.then_some(&self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.end.as_ref().is_some_and(|end| *end <= self.start)
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        key >= &self.start[..] && self.end.as_ref().is_none_or(|end| key < &end[..])
    }

    pub fn intersects(&self, other: &KeyRange) -> bool {
        let below = |end: &Option<Bytes>, start: &Bytes| end.as_ref().is_some_and(|e| e <= start);
        !self.is_empty()
            && !other.is_empty()
            && !below(&self.end, &other.start)
            && !below(&other.end, &self.start)
    }
}

/// A WAL replica's address, as Raft membership records it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct Member {
    pub node_id: NodeId,
    pub addr: String,
}

/// What a front end sends a partition's leader (§10.3).
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Proposal {
    /// Every proposal has one, so re-sending a proposal is always safe. A
    /// key's first decided outcome is its only outcome.
    pub idempotency_key: Bytes,
    pub kind: ProposalKind,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ProposalKind {
    Mutation(Mutation),
    /// The request failed when re-executed. The leader records the failure
    /// as the key's outcome, unless the key already has one, in which case
    /// that one is returned. A front end never reports a failure for a key
    /// without this, since an earlier attempt may still commit.
    Failure {
        reply: Bytes,
    },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Mutation {
    /// `S`: the causally closed version the mutation read at.
    pub read_version: Dvv,
    /// The mutation's causal past (§9.5). It must include `read_version`.
    pub context: Dvv,
    /// Every key and range the mutation read in this partition, including
    /// every key it overwrites.
    pub read_set: Vec<KeyRange>,
    /// Every key and range it writes in this partition.
    pub write_set: Vec<KeyRange>,
    /// The entry's content, opaque to the WAL.
    pub payload: Bytes,
    /// What the front end returns to its client, returned again to retries.
    pub reply: Bytes,
}

/// An idempotency key's outcome.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Decided {
    /// The entry that decided it.
    pub dot: Dot,
    /// The leader's clock when it validated the entry, in microseconds since
    /// the Unix epoch. Non-decreasing in log order.
    pub commit_time_us: u64,
    pub status: DecidedStatus,
    pub reply: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum DecidedStatus {
    /// The mutation committed at `dot`.
    Committed,
    /// A failure was recorded at `dot`.
    Failed,
}

/// Why a proposal wasn't decided.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Encode, Decode)]
pub enum Rejection {
    /// A write the mutation didn't see touched its read set. Re-execute at a
    /// newer version.
    #[error("contended")]
    Contended,
    /// The read version is older than the leader's validation window.
    /// Re-execute at a newer version.
    #[error("read version too old")]
    TooOld,
    /// Send the same proposal to the leader.
    #[error("not the leader (leader: {leader:?})")]
    NotLeader { leader: Option<Member> },
    /// The node doesn't host the partition.
    #[error("unknown partition")]
    UnknownPartition,
    /// It may or may not commit. Send the same proposal again.
    #[error("unknown outcome: {0}")]
    UnknownOutcome(String),
    #[error("invalid proposal: {0}")]
    Invalid(String),
}

/// A committed log entry, as readers of the log see it.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct LogEntry {
    pub dot: Dot,
    /// The Raft term it was appended in.
    pub term: u64,
    pub kind: EntryKind,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum EntryKind {
    /// Raft's own entries: a new leader's no-op, membership changes.
    Internal,
    /// Appended under a later term than the one it was validated in, so it
    /// stamps nothing and decides nothing.
    Void,
    Mutation(MutationEntry),
    /// A failure recorded under an idempotency key. It stamps nothing.
    Failure {
        commit_time_us: u64,
        idempotency_key: Bytes,
    },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct MutationEntry {
    pub context: Dvv,
    pub commit_time_us: u64,
    pub idempotency_key: Bytes,
    pub payload: Bytes,
}

/// What one node knows about one partition's Raft group.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct PartitionStatus {
    pub node_id: NodeId,
    pub term: u64,
    pub leader: Option<Member>,
    /// The highest index in this replica's log.
    pub last_log_index: u64,
    /// The highest index this replica has applied, which is committed.
    pub applied_index: Option<u64>,
    pub voters: Vec<NodeId>,
}

// ---- requests and replies ----

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ProposeRequest {
    pub partition: PartitionId,
    pub proposal: Proposal,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ProposeReply {
    Decided(Decided),
    Rejected(Rejection),
}

impl From<Result<Decided, Rejection>> for ProposeReply {
    fn from(r: Result<Decided, Rejection>) -> Self {
        match r {
            Ok(d) => ProposeReply::Decided(d),
            Err(r) => ProposeReply::Rejected(r),
        }
    }
}

impl From<ProposeReply> for Result<Decided, Rejection> {
    fn from(r: ProposeReply) -> Self {
        match r {
            ProposeReply::Decided(d) => Ok(d),
            ProposeReply::Rejected(r) => Err(r),
        }
    }
}

/// Committed entries from `from_index` on: a long poll that waits up to
/// `max_wait_ms` for at least one.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct FetchRequest {
    pub partition: PartitionId,
    pub from_index: u64,
    /// About how many bytes of entries to return; at least one entry.
    pub max_bytes: u32,
    pub max_wait_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum FetchReply {
    /// Contiguous entries from `from_index`, possibly none if the wait ran
    /// out, and the replica's applied index when it answered.
    Entries {
        entries: Vec<LogEntry>,
        applied_index: Option<u64>,
    },
    /// The replica no longer holds `from_index`; its log starts at `start`.
    OutOfRange {
        start: u64,
    },
    UnknownPartition,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct StatusRequest {
    pub partition: PartitionId,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum StatusReply {
    Status(PartitionStatus),
    UnknownPartition,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct CreatePartitionRequest {
    pub partition: PartitionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct InitializePartitionRequest {
    pub partition: PartitionId,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum AdminReply {
    Ok,
    AlreadyInitialized,
    UnknownPartition,
    Failed(String),
}
