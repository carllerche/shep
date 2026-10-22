//! Multi-raft, thread-per-core (design §3, §9.6, §10), after Redpanda's raft,
//! for embedding: each group runs the embedder's [`Machine`], which admits
//! proposals on the leader and applies committed commands on every replica.
//! The crate owns what every replicated log needs (see [`machine`]): void
//! entries, rebuilding leader-side state, idempotency keys, commit times,
//! and tailing the log.
//!
//! Groups run the openraft engine forked in `shep-raft-engine`, each on the
//! core that hosts it. What changes from openraft's own deployment model is
//! everything around the engine:
//!
//! - **Storage** is `shep-storage`: a segmented log per group, its vote in the
//!   core's key-value store, fsyncs shared by concurrent flushes.
//! - **Replication is pipelined:** a window of AppendEntries in flight per
//!   follower, ordered by session and sequence on the follower.
//! - **Heartbeats are batched:** each core triggers its leaders together, and
//!   one RPC per peer carries them.
//! - **Transport** is `shep-rpc`; a peer's message for a group is routed to
//!   the core that hosts it.

mod log_storage;
pub mod machine;
mod network;
mod node;
mod proposer;
mod state_machine;
mod types;
mod wire;

pub use machine::{
    Admit, Decided, Entry, Fetched, LogRecord, Machine, ProposeError, RecordKind, Resolution,
};
pub use network::Faults;
pub use node::{
    GroupHandle, GroupStatus, Leadership, LeadershipWatch, ProposalConfig, RaftConfig, RaftNode,
    RaftNodeError, RaftStats,
};
pub use types::{GroupId, NodeId};
