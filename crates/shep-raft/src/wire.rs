//! Raft's messages between nodes, on shep-rpc.

use bytes::Bytes;
use shep_codec::{Decode, Encode};
use shep_rpc::method_id;

use crate::types::{
    AppendResponse, GroupId, Vote, WireEntry, WireLogId, WireSnapshotMeta, WireVote,
    from_opt_log_id, opt_log_id,
};

/// The Raft service's methods.
pub(crate) const SERVICE: u16 = 1;
pub(crate) const APPEND: u32 = method_id(SERVICE, 1);
pub(crate) const HEARTBEATS: u32 = method_id(SERVICE, 2);
pub(crate) const VOTE: u32 = method_id(SERVICE, 3);
pub(crate) const PRE_VOTE: u32 = method_id(SERVICE, 4);
pub(crate) const SNAPSHOT: u32 = method_id(SERVICE, 5);
pub(crate) const TRANSFER_LEADER: u32 = method_id(SERVICE, 6);

/// The version of the Raft service's protocol this node speaks.
pub(crate) const VERSION: u8 = 0;

/// The Raft service's statuses.
pub mod status {
    /// The method isn't one of the Raft service's.
    pub const UNKNOWN_METHOD: u32 = 1000;
    /// This node can't serve Raft right now: it is shutting down, or cut off
    /// by a test fault.
    pub const UNAVAILABLE: u32 = 1001;
    /// The request is at a version this node doesn't speak, or its body
    /// doesn't decode.
    pub const MALFORMED: u32 = 1002;
}

/// One AppendEntries. Requests of one replication stream share a `session`
/// and carry consecutive `seq`s from 0, so the follower applies them in order
/// however they arrive (Redpanda's `follower_req_seq`). Heartbeats have no
/// session.
#[derive(Clone, Debug, Encode, Decode)]
pub(crate) struct AppendRequest {
    pub group: GroupId,
    pub session: u64,
    pub seq: u64,
    pub vote: WireVote,
    pub prev_log_id: Option<WireLogId>,
    pub leader_commit: Option<WireLogId>,
    pub entries: Vec<WireEntry>,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(crate) enum AppendReply {
    Success,
    PartialSuccess(Option<WireLogId>),
    Conflict,
    HigherVote(WireVote),
    /// The receiver couldn't process it: no such group, or it's stopping.
    Rejected(String),
}

impl From<&AppendResponse> for AppendReply {
    fn from(r: &AppendResponse) -> Self {
        match r {
            AppendResponse::Success => AppendReply::Success,
            AppendResponse::PartialSuccess(l) => AppendReply::PartialSuccess(opt_log_id(l)),
            AppendResponse::Conflict => AppendReply::Conflict,
            AppendResponse::HigherVote(v) => AppendReply::HigherVote(v.into()),
        }
    }
}

impl AppendReply {
    pub(crate) fn into_response(self) -> Result<AppendResponse, String> {
        Ok(match self {
            AppendReply::Success => AppendResponse::Success,
            AppendReply::PartialSuccess(l) => AppendResponse::PartialSuccess(from_opt_log_id(l)),
            AppendReply::Conflict => AppendResponse::Conflict,
            AppendReply::HigherVote(v) => AppendResponse::HigherVote(Vote::from(v)),
            AppendReply::Rejected(m) => return Err(m),
        })
    }
}

/// Every heartbeat one core has for one peer, in one RPC (Redpanda's
/// `heartbeat_manager`).
#[derive(Clone, Debug, Encode, Decode)]
pub(crate) struct Heartbeats {
    pub items: Vec<AppendRequest>,
}

/// Replies in the order of the requests.
#[derive(Clone, Debug, Encode, Decode)]
pub(crate) struct HeartbeatReplies {
    pub items: Vec<AppendReply>,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(crate) struct VoteRequest {
    pub group: GroupId,
    pub vote: WireVote,
    pub last_log_id: Option<WireLogId>,
    pub leadership_transfer: bool,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(crate) enum VoteReply {
    Ok {
        vote: WireVote,
        vote_granted: bool,
        last_log_id: Option<WireLogId>,
    },
    Rejected(String),
}

#[derive(Clone, Debug, Encode, Decode)]
pub(crate) struct SnapshotRequest {
    pub group: GroupId,
    pub vote: WireVote,
    pub meta: WireSnapshotMeta,
    pub data: Bytes,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(crate) enum SnapshotReply {
    Ok(WireVote),
    Rejected(String),
}

#[derive(Clone, Debug, Encode, Decode)]
pub(crate) struct TransferLeaderRequest {
    pub group: GroupId,
    pub from_leader: WireVote,
    pub to: u64,
    pub last_log_id: Option<WireLogId>,
}

#[derive(Clone, Debug, Encode, Decode)]
pub(crate) enum TransferLeaderReply {
    Ok,
    VoteChanged {
        expected: WireVote,
        actual: WireVote,
    },
    LogNotFlushed {
        expected: Option<WireLogId>,
        actual: Option<WireLogId>,
    },
    Rejected(String),
}
