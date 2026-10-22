//! The WAL's client protocol (design §10.3).
//!
//! Front ends propose mutations to a partition's leader; materializers and
//! other readers stream the committed log. This crate holds the gRPC
//! definitions ([`proto`]) and the Rust types both sides use, with the
//! conversions between them. The WAL itself treats an entry's content as
//! opaque bytes: it validates only keys and key ranges.

use bytes::{BufMut, Bytes, BytesMut};

pub use shep_version::{Dot, Dvv, PartitionId};

pub mod proto {
    tonic::include_proto!("shep.wal.v1");
}

/// A WAL node's ID within its fleet.
pub type NodeId = u64;

/// A malformed message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed message: {0}")]
pub struct DecodeError(pub String);

fn missing(field: &str) -> DecodeError {
    DecodeError(format!("missing `{field}`"))
}

/// A range of keys in the WAL's conflict space: `[start, end)`, or
/// `[start, ∞)` without an end.
///
/// Read sets and write sets are lists of ranges. Front ends encode facts as
/// keys so that every range predicate a mutation reads is a key range (§10.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Member {
    pub node_id: NodeId,
    pub addr: String,
}

/// What a front end sends a partition's leader (§10.3).
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    /// Every proposal has one, so re-sending a proposal is always safe. A
    /// key's first decided outcome is its only outcome.
    pub idempotency_key: Bytes,
    pub kind: ProposalKind,
}

#[derive(Clone, Debug, PartialEq)]
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

#[derive(Clone, Debug, PartialEq)]
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
    /// What the front end returns to its client, kept with the idempotency
    /// key for retries.
    pub reply: Bytes,
}

/// An idempotency key's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decided {
    /// The entry that decided it.
    pub dot: Dot,
    /// The leader's clock when it appended the entry, in microseconds since
    /// the Unix epoch. Non-decreasing in log order.
    pub commit_time_us: u64,
    pub status: DecidedStatus,
    pub reply: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecidedStatus {
    /// The mutation committed at `dot`.
    Committed,
    /// A failure was recorded at `dot`.
    Failed,
}

/// Why a proposal wasn't decided.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
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
#[derive(Clone, Debug, PartialEq)]
pub struct LogEntry {
    pub dot: Dot,
    /// The Raft term it was appended in.
    pub term: u64,
    pub kind: EntryKind,
}

#[derive(Clone, Debug, PartialEq)]
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

#[derive(Clone, Debug, PartialEq)]
pub struct MutationEntry {
    pub context: Dvv,
    pub commit_time_us: u64,
    pub idempotency_key: Bytes,
    pub payload: Bytes,
}

/// What one node knows about one partition's Raft group.
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionStatus {
    pub node_id: NodeId,
    pub term: u64,
    pub leader: Option<Member>,
    pub last_log_index: u64,
    pub applied_index: Option<u64>,
    pub voters: Vec<Member>,
}

// ---- conversions ----

impl From<Dot> for proto::Dot {
    fn from(dot: Dot) -> Self {
        proto::Dot {
            partition: dot.partition.0,
            index: dot.index,
        }
    }
}

impl From<proto::Dot> for Dot {
    fn from(dot: proto::Dot) -> Self {
        Dot::new(PartitionId(dot.partition), dot.index)
    }
}

impl From<&Dvv> for proto::Dvv {
    fn from(v: &Dvv) -> Self {
        proto::Dvv {
            parts: v
                .partitions()
                .map(|p| proto::DvvPart {
                    partition: p.0,
                    prefix: v.prefix(p),
                    extras: v.extras(p).collect(),
                })
                .collect(),
        }
    }
}

impl From<proto::Dvv> for Dvv {
    fn from(v: proto::Dvv) -> Self {
        let mut out = Dvv::new();
        for part in v.parts {
            let p = PartitionId(part.partition);
            out.raise_prefix(p, part.prefix);
            for i in part.extras {
                out.insert(Dot::new(p, i));
            }
        }
        out
    }
}

/// Decodes an optional message field that's required, as a version.
pub fn dvv_from(v: Option<proto::Dvv>, field: &str) -> Result<Dvv, DecodeError> {
    v.map(Dvv::from).ok_or_else(|| missing(field))
}

impl From<&KeyRange> for proto::KeyRange {
    fn from(r: &KeyRange) -> Self {
        proto::KeyRange {
            start: r.start.clone(),
            end: r.end.clone(),
        }
    }
}

impl From<proto::KeyRange> for KeyRange {
    fn from(r: proto::KeyRange) -> Self {
        KeyRange {
            start: r.start,
            end: r.end,
        }
    }
}

impl From<&Member> for proto::Member {
    fn from(m: &Member) -> Self {
        proto::Member {
            node_id: m.node_id,
            addr: m.addr.clone(),
        }
    }
}

impl From<proto::Member> for Member {
    fn from(m: proto::Member) -> Self {
        Member {
            node_id: m.node_id,
            addr: m.addr,
        }
    }
}

impl From<&Proposal> for proto::Proposal {
    fn from(p: &Proposal) -> Self {
        let kind = match &p.kind {
            ProposalKind::Mutation(m) => proto::proposal::Kind::Mutation(proto::Mutation {
                read_version: Some((&m.read_version).into()),
                context: Some((&m.context).into()),
                read_set: m.read_set.iter().map(Into::into).collect(),
                write_set: m.write_set.iter().map(Into::into).collect(),
                payload: m.payload.clone(),
                reply: m.reply.clone(),
            }),
            ProposalKind::Failure { reply } => proto::proposal::Kind::Failure(proto::Failure {
                reply: reply.clone(),
            }),
        };
        proto::Proposal {
            idempotency_key: p.idempotency_key.clone(),
            kind: Some(kind),
        }
    }
}

impl TryFrom<proto::Proposal> for Proposal {
    type Error = DecodeError;

    fn try_from(p: proto::Proposal) -> Result<Self, DecodeError> {
        let kind = match p.kind.ok_or_else(|| missing("Proposal.kind"))? {
            proto::proposal::Kind::Mutation(m) => ProposalKind::Mutation(Mutation {
                read_version: dvv_from(m.read_version, "Mutation.read_version")?,
                context: dvv_from(m.context, "Mutation.context")?,
                read_set: m.read_set.into_iter().map(Into::into).collect(),
                write_set: m.write_set.into_iter().map(Into::into).collect(),
                payload: m.payload,
                reply: m.reply,
            }),
            proto::proposal::Kind::Failure(f) => ProposalKind::Failure { reply: f.reply },
        };
        Ok(Proposal {
            idempotency_key: p.idempotency_key,
            kind,
        })
    }
}

impl From<&Decided> for proto::Decided {
    fn from(d: &Decided) -> Self {
        let status = match d.status {
            DecidedStatus::Committed => proto::DecidedStatus::Committed,
            DecidedStatus::Failed => proto::DecidedStatus::Failed,
        };
        proto::Decided {
            dot: Some(d.dot.into()),
            commit_time_us: d.commit_time_us,
            status: status.into(),
            reply: d.reply.clone(),
        }
    }
}

impl TryFrom<proto::Decided> for Decided {
    type Error = DecodeError;

    fn try_from(d: proto::Decided) -> Result<Self, DecodeError> {
        let status = match d.status() {
            proto::DecidedStatus::Committed => DecidedStatus::Committed,
            proto::DecidedStatus::Failed => DecidedStatus::Failed,
            proto::DecidedStatus::Unspecified => return Err(missing("Decided.status")),
        };
        Ok(Decided {
            dot: d.dot.ok_or_else(|| missing("Decided.dot"))?.into(),
            commit_time_us: d.commit_time_us,
            status,
            reply: d.reply,
        })
    }
}

impl From<&Rejection> for proto::Rejected {
    fn from(r: &Rejection) -> Self {
        use proto::RejectReason as R;
        let (reason, leader, message) = match r {
            Rejection::Contended => (R::Contended, None, String::new()),
            Rejection::TooOld => (R::TooOld, None, String::new()),
            Rejection::NotLeader { leader } => {
                (R::NotLeader, leader.as_ref().map(Into::into), String::new())
            }
            Rejection::UnknownPartition => (R::UnknownPartition, None, String::new()),
            Rejection::UnknownOutcome(m) => (R::UnknownOutcome, None, m.clone()),
            Rejection::Invalid(m) => (R::Invalid, None, m.clone()),
        };
        proto::Rejected {
            reason: reason.into(),
            leader,
            message,
        }
    }
}

impl TryFrom<proto::Rejected> for Rejection {
    type Error = DecodeError;

    fn try_from(r: proto::Rejected) -> Result<Self, DecodeError> {
        use proto::RejectReason as R;
        Ok(match r.reason() {
            R::Contended => Rejection::Contended,
            R::TooOld => Rejection::TooOld,
            R::NotLeader => Rejection::NotLeader {
                leader: r.leader.map(Into::into),
            },
            R::UnknownPartition => Rejection::UnknownPartition,
            R::UnknownOutcome => Rejection::UnknownOutcome(r.message),
            R::Invalid => Rejection::Invalid(r.message),
            R::Unspecified => return Err(missing("Rejected.reason")),
        })
    }
}

impl From<&Result<Decided, Rejection>> for proto::ProposeResponse {
    fn from(r: &Result<Decided, Rejection>) -> Self {
        let result = match r {
            Ok(d) => proto::propose_response::Result::Decided(d.into()),
            Err(r) => proto::propose_response::Result::Rejected(r.into()),
        };
        proto::ProposeResponse {
            result: Some(result),
        }
    }
}

impl TryFrom<proto::ProposeResponse> for Result<Decided, Rejection> {
    type Error = DecodeError;

    fn try_from(r: proto::ProposeResponse) -> Result<Self, DecodeError> {
        Ok(
            match r.result.ok_or_else(|| missing("ProposeResponse.result"))? {
                proto::propose_response::Result::Decided(d) => Ok(d.try_into()?),
                proto::propose_response::Result::Rejected(r) => Err(r.try_into()?),
            },
        )
    }
}

impl LogEntry {
    pub fn to_proto(&self) -> proto::LogEntry {
        use proto::log_entry::Kind;
        let kind = match &self.kind {
            EntryKind::Internal => Kind::Internal(proto::Internal {}),
            EntryKind::Void => Kind::Void(proto::Void {}),
            EntryKind::Mutation(m) => Kind::Mutation(proto::MutationEntry {
                context: Some((&m.context).into()),
                commit_time_us: m.commit_time_us,
                idempotency_key: m.idempotency_key.clone(),
                payload: m.payload.clone(),
            }),
            EntryKind::Failure {
                commit_time_us,
                idempotency_key,
            } => Kind::Failure(proto::FailureEntry {
                commit_time_us: *commit_time_us,
                idempotency_key: idempotency_key.clone(),
            }),
        };
        proto::LogEntry {
            index: self.dot.index,
            term: self.term,
            kind: Some(kind),
        }
    }

    pub fn from_proto(partition: PartitionId, e: proto::LogEntry) -> Result<LogEntry, DecodeError> {
        use proto::log_entry::Kind;
        let kind = match e.kind.ok_or_else(|| missing("LogEntry.kind"))? {
            Kind::Internal(_) => EntryKind::Internal,
            Kind::Void(_) => EntryKind::Void,
            Kind::Mutation(m) => EntryKind::Mutation(MutationEntry {
                context: dvv_from(m.context, "MutationEntry.context")?,
                commit_time_us: m.commit_time_us,
                idempotency_key: m.idempotency_key,
                payload: m.payload,
            }),
            Kind::Failure(f) => EntryKind::Failure {
                commit_time_us: f.commit_time_us,
                idempotency_key: f.idempotency_key,
            },
        };
        Ok(LogEntry {
            dot: Dot::new(partition, e.index),
            term: e.term,
            kind,
        })
    }
}

impl From<&PartitionStatus> for proto::StatusResponse {
    fn from(s: &PartitionStatus) -> Self {
        proto::StatusResponse {
            node_id: s.node_id,
            term: s.term,
            leader: s.leader.as_ref().map(Into::into),
            last_log_index: s.last_log_index,
            applied_index: s.applied_index,
            voters: s.voters.iter().map(Into::into).collect(),
        }
    }
}

impl From<proto::StatusResponse> for PartitionStatus {
    fn from(s: proto::StatusResponse) -> Self {
        PartitionStatus {
            node_id: s.node_id,
            term: s.term,
            leader: s.leader.map(Into::into),
            last_log_index: s.last_log_index,
            applied_index: s.applied_index,
            voters: s.voters.into_iter().map(Into::into).collect(),
        }
    }
}
