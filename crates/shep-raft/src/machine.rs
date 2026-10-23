//! What an embedder supplies for each group, and what proposing to one
//! decides.
//!
//! [`Machine`] is this crate's one trait: the embedder's state machine is the
//! one argument whose variants the crate can't know. An embedder that runs
//! several kinds of group implements it for an enum of them.
//!
//! The crate owns the parts every replicated log needs and that are easy to
//! get wrong:
//!
//! - **Admission happens on the leader, before append.** A command records
//!   the term it was admitted in, and an entry appended under any other term
//!   is void on every replica: leadership can change between admitting a
//!   proposal and Raft appending it.
//! - **Leader-side state is rebuilt from the log.** A replica that starts
//!   leading a term replays the recent log, committed or not, into its
//!   machine before admitting anything.
//! - **Idempotency keys** are decided at most once, and a retry within the
//!   window gets the original decision.
//! - **Commit times** never go backwards in a log, across terms too: a new
//!   leader's start above every entry it has applied or replayed.

use shep_codec::{Decode, Encode};

use crate::types::{GroupId, NodeId};

/// A group's application: admits proposals on the leader, and applies
/// committed commands on every replica.
///
/// One instance per replica, on the core that hosts it; the crate never calls
/// it from two places at once.
pub trait Machine: 'static {
    /// What a proposer asks for. It never reaches the log: [`Machine::admit`]
    /// turns it into a command.
    type Proposal: 'static;
    /// What the log holds.
    type Command: Encode + Decode + 'static;
    /// Why [`Machine::admit`] refused a proposal.
    type Rejection: Clone + 'static;
    /// What applying a command decided, for its proposer. It's kept with the
    /// command's idempotency key for the idempotency window, so keep it
    /// small: anything a retry needs can be read back from the entry.
    type Response: Clone + Encode + Decode + 'static;

    /// Leader only: checks `proposal` against every entry appended before it,
    /// in this term or replayed from earlier ones, and returns the command to
    /// append. Runs once per proposal, in the order they're appended.
    ///
    /// A command it returns may still not be appended, or be void: its fate
    /// arrives at [`Machine::resolve`] under `admit.seq`.
    fn admit(
        &mut self,
        admit: &Admit,
        proposal: Self::Proposal,
    ) -> Result<Self::Command, Self::Rejection>;

    /// Leader only: this replica now leads `term`. Drop any leader-side state
    /// from earlier terms; the entries above `floor` are replayed next, and
    /// nothing at or below it will be.
    fn lead(&mut self, term: u64, floor: u64) {
        let _ = (term, floor);
    }

    /// Leader only: an entry already in the log above [`Machine::lead`]'s
    /// floor, committed or not, oldest first. Void entries are skipped.
    fn replay(&mut self, entry: &Entry<Self::Command>) {
        let _ = entry;
    }

    /// Leader only: what became of the command admitted as `seq` this term.
    fn resolve(&mut self, seq: u64, resolution: Resolution) {
        let _ = (seq, resolution);
    }

    /// Every replica: applies a committed command. Commands arrive in log
    /// order, once each, void ones never; a restarted replica replays its
    /// log, so this must be deterministic.
    fn apply(&mut self, entry: &Entry<Self::Command>) -> Self::Response;

    /// The machine's whole replicated state, for a snapshot.
    fn snapshot(&self) -> Vec<u8>;

    /// Replaces the replicated state with a snapshot's.
    fn restore(&mut self, snapshot: Vec<u8>);
}

/// What [`Machine::admit`] knows about the entry it would become.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admit {
    pub term: u64,
    /// Numbers this term's admissions from 0, for [`Machine::resolve`].
    pub seq: u64,
    /// The entry's commit time: the leader's clock, but never before an
    /// earlier entry's.
    pub commit_time_us: u64,
    /// One past the log's last entry; the command will land at or above it.
    pub next_index: u64,
    /// Leader-side state needs entries since this time, and at most
    /// `window_entries` of them: a new leader replays no more than that.
    pub window_start_us: u64,
    pub window_entries: usize,
}

/// A command in the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry<C> {
    pub index: u64,
    pub term: u64,
    pub commit_time_us: u64,
    pub key: Option<Vec<u8>>,
    pub command: C,
}

/// What became of an admitted command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Appended at this index under the term it was admitted in.
    Appended(u64),
    /// Appended at this index, but void: it decides nothing.
    Void(u64),
    /// Never appended.
    NotAppended,
    /// It may or may not have been appended; this term is likely over.
    Unknown,
}

/// A decided proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decided<R> {
    pub index: u64,
    pub commit_time_us: u64,
    pub response: R,
    /// An earlier proposal with the same idempotency key decided it; this is
    /// its decision, and the entry at `index` is its command.
    pub replayed: bool,
}

/// Why a proposal wasn't decided.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProposeError<J> {
    /// [`Machine::admit`] refused it. Nothing was appended.
    #[error("rejected")]
    Rejected(J),
    /// This replica doesn't lead the group, or can't yet. Nothing was
    /// appended.
    #[error("not the leader (leader: {leader:?})")]
    NotLeader { leader: Option<(NodeId, String)> },
    /// Its entry would be larger than the group accepts.
    #[error("proposal too large: {bytes} bytes, limit {limit}")]
    TooLarge { bytes: usize, limit: usize },
    /// It may or may not commit; send it again with the same key to learn
    /// which.
    #[error("unknown outcome: {0}")]
    Unknown(String),
    #[error("no group {0} on this node")]
    NoSuchGroup(GroupId),
}

/// An entry read back from a group's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord<C> {
    pub index: u64,
    pub term: u64,
    pub kind: RecordKind<C>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordKind<C> {
    /// Raft's own: a new leader's no-op, or a membership change.
    Internal,
    /// Admitted in another term than it was appended in; decides nothing.
    Void,
    Entry(Entry<C>),
}

/// What [`crate::GroupHandle::fetch`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetched<C> {
    /// Committed records from the requested index, oldest first, perhaps
    /// none; and the last index applied here.
    Records {
        records: Vec<LogRecord<C>>,
        applied_index: Option<u64>,
    },
    /// The log no longer holds the requested index; it starts here.
    OutOfRange { start: u64 },
}

/// How a command is stored in the log.
#[derive(Encode, Decode)]
pub(crate) struct Envelope<C> {
    /// The term of the leader that admitted it.
    pub validated_term: u64,
    pub commit_time_us: u64,
    /// Keys decided before this time are forgotten.
    pub idempotency_floor_us: u64,
    /// Empty for none.
    pub key: Vec<u8>,
    pub command: C,
}

impl<C> Envelope<C> {
    pub(crate) fn is_void(&self, entry_term: u64) -> bool {
        self.validated_term != entry_term
    }

    pub(crate) fn into_entry(self, index: u64, term: u64) -> Entry<C> {
        Entry {
            index,
            term,
            commit_time_us: self.commit_time_us,
            key: (!self.key.is_empty()).then_some(self.key),
            command: self.command,
        }
    }
}

/// What applying an entry decided, as the engine hands it to the proposer.
#[derive(Encode, Decode)]
pub(crate) enum Applied<R> {
    /// Raft's own entries.
    Internal,
    /// Admitted in another term than it was appended in.
    Void,
    Decided {
        commit_time_us: u64,
        response: R,
    },
}
