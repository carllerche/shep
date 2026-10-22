//! The machines a WAL node's groups run: Shep's partitions, which validate
//! proposals against a leader-side table (§10.3), and the controller.

use std::collections::HashMap;

use bytes::Bytes;
use shep_codec::{Decode, Encode};
use shep_raft::{Admit, Entry, GroupId, Machine, Resolution};
use shep_wal_api::{PartitionId, ProposalKind, Rejection};
use shep_wal_controller::{CONTROLLER_GROUP, ControllerSm, LogSetRules, NameRule, Outcome};

use crate::command::PartitionCommand;
use crate::table::{SlotIndex, Table};

/// The most data partitions one universe can have. A universe is a log set
/// whose first partition is its meta partition and the rest its data.
pub const MAX_DATA_PARTITIONS: usize = 1024;

/// Shep's rules for log sets: universes, named by DNS domains (§14).
pub fn universe_rules() -> LogSetRules {
    LogSetRules {
        max_partitions: 1 + MAX_DATA_PARTITIONS,
        names: NameRule::Domain,
    }
}

/// A partition: the log is what readers consume, so the replicated state is
/// nothing (shep-raft keeps the idempotency keys). The leader-side state is
/// the validation table.
pub struct PartitionSm {
    partition: PartitionId,
    leader: Option<Leader>,
}

struct Leader {
    table: Table,
    /// Admissions not yet resolved, to their slots in the table.
    slots: HashMap<u64, u64>,
}

impl PartitionSm {
    pub fn new(partition: PartitionId) -> PartitionSm {
        PartitionSm {
            partition,
            leader: None,
        }
    }
}

impl Machine for PartitionSm {
    type Proposal = ProposalKind;
    type Command = PartitionCommand;
    type Rejection = Rejection;
    /// Whether the key's outcome is a failure.
    type Response = bool;

    fn lead(&mut self, term: u64, floor: u64) {
        self.leader = Some(Leader {
            table: Table::new(self.partition, term, floor),
            slots: HashMap::new(),
        });
    }

    fn replay(&mut self, entry: &Entry<PartitionCommand>) {
        if let Some(l) = &mut self.leader {
            l.table.push(
                SlotIndex::Known(entry.index),
                entry.command.write_set(),
                entry.commit_time_us,
            );
        }
    }

    /// Validates a proposal (§10.3): it commits only if no entry its read
    /// version doesn't contain wrote a key in its read set or a range it
    /// read, including entries appended but not yet committed.
    fn admit(
        &mut self,
        admit: &Admit,
        proposal: ProposalKind,
    ) -> Result<PartitionCommand, Rejection> {
        let p = self.partition;
        let Some(l) = self
            .leader
            .as_mut()
            .filter(|l| l.table.term() == admit.term)
        else {
            return Err(Rejection::UnknownOutcome("no validation table".into()));
        };
        l.table.evict(admit.window_start_us, admit.window_entries);
        let command = match proposal {
            ProposalKind::Mutation(m) => {
                if !m.context.includes(&m.read_version) {
                    let msg = "context must include the read version".into();
                    return Err(Rejection::Invalid(msg));
                }
                let next = admit.next_index;
                if m.read_version.prefix(p) >= next || m.read_version.extras(p).any(|i| i >= next) {
                    let msg = "read version names entries past the log's end".into();
                    return Err(Rejection::Invalid(msg));
                }
                if m.read_version.prefix(p) < l.table.floor() {
                    return Err(Rejection::TooOld);
                }
                if l.table.conflicts(&m.read_set, &m.read_version) {
                    return Err(Rejection::Contended);
                }
                PartitionCommand::Mutation {
                    context: m.context,
                    write_set: m.write_set,
                    payload: m.payload,
                    reply: m.reply,
                }
            }
            ProposalKind::Failure { reply } => PartitionCommand::Failure { reply },
        };
        let slot = l.table.push(
            SlotIndex::Pending,
            command.write_set(),
            admit.commit_time_us,
        );
        l.slots.insert(admit.seq, slot);
        Ok(command)
    }

    fn resolve(&mut self, seq: u64, resolution: Resolution) {
        let Some(l) = &mut self.leader else { return };
        let Some(slot) = l.slots.remove(&seq) else {
            return;
        };
        let index = match resolution {
            Resolution::Appended(i) => SlotIndex::Known(i),
            Resolution::Void(i) => SlotIndex::Void(i),
            Resolution::NotAppended => SlotIndex::NotAppended,
            // Its slot stays unseen-by-everyone, which only costs conflicts:
            // the term is usually over, and the table with it.
            Resolution::Unknown => return,
        };
        l.table.resolve(slot, index);
    }

    fn apply(&mut self, entry: &Entry<PartitionCommand>) -> bool {
        entry.command.failed()
    }

    fn snapshot(&self) -> Bytes {
        Bytes::new()
    }

    fn restore(&mut self, _: Bytes) {}
}

/// Every group a WAL node hosts is the controller or a partition (an enum,
/// not a trait: the node knows both kinds).
pub enum GroupSm {
    Controller(ControllerSm),
    Partition(PartitionSm),
}

pub enum GroupProposal {
    Controller(shep_wal_controller::Proposal),
    Partition(ProposalKind),
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum GroupCommand {
    Controller(shep_wal_controller::Command),
    Partition(PartitionCommand),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupRejection {
    Controller(String),
    Partition(Rejection),
    /// A proposal for the other kind of group.
    WrongGroup,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum GroupResponse {
    Controller(Outcome),
    Partition(bool),
    /// A command for the other kind of group, which admission never makes.
    WrongGroup,
}

impl GroupSm {
    pub fn for_group(group: GroupId) -> GroupSm {
        if group == CONTROLLER_GROUP {
            GroupSm::Controller(ControllerSm::new(universe_rules()))
        } else {
            GroupSm::Partition(PartitionSm::new(PartitionId(group.0)))
        }
    }

    pub(crate) fn controller(&self) -> Option<&ControllerSm> {
        match self {
            GroupSm::Controller(c) => Some(c),
            GroupSm::Partition(_) => None,
        }
    }
}

impl Machine for GroupSm {
    type Proposal = GroupProposal;
    type Command = GroupCommand;
    type Rejection = GroupRejection;
    type Response = GroupResponse;

    fn admit(
        &mut self,
        admit: &Admit,
        proposal: GroupProposal,
    ) -> Result<GroupCommand, GroupRejection> {
        match (self, proposal) {
            (GroupSm::Controller(c), GroupProposal::Controller(p)) => c
                .admit(admit, p)
                .map(GroupCommand::Controller)
                .map_err(GroupRejection::Controller),
            (GroupSm::Partition(s), GroupProposal::Partition(p)) => s
                .admit(admit, p)
                .map(GroupCommand::Partition)
                .map_err(GroupRejection::Partition),
            _ => Err(GroupRejection::WrongGroup),
        }
    }

    fn lead(&mut self, term: u64, floor: u64) {
        match self {
            GroupSm::Controller(c) => c.lead(term, floor),
            GroupSm::Partition(p) => p.lead(term, floor),
        }
    }

    fn replay(&mut self, entry: &Entry<GroupCommand>) {
        match (self, &entry.command) {
            (GroupSm::Controller(c), GroupCommand::Controller(cmd)) => c.replay(&with(entry, cmd)),
            (GroupSm::Partition(p), GroupCommand::Partition(cmd)) => p.replay(&with(entry, cmd)),
            _ => {}
        }
    }

    fn resolve(&mut self, seq: u64, resolution: Resolution) {
        match self {
            GroupSm::Controller(c) => c.resolve(seq, resolution),
            GroupSm::Partition(p) => p.resolve(seq, resolution),
        }
    }

    fn apply(&mut self, entry: &Entry<GroupCommand>) -> GroupResponse {
        match (self, &entry.command) {
            (GroupSm::Controller(c), GroupCommand::Controller(cmd)) => {
                GroupResponse::Controller(c.apply(&with(entry, cmd)))
            }
            (GroupSm::Partition(p), GroupCommand::Partition(cmd)) => {
                GroupResponse::Partition(p.apply(&with(entry, cmd)))
            }
            _ => {
                tracing::error!(index = entry.index, "a command for another kind of group");
                GroupResponse::WrongGroup
            }
        }
    }

    fn snapshot(&self) -> Bytes {
        match self {
            GroupSm::Controller(c) => c.snapshot(),
            GroupSm::Partition(p) => p.snapshot(),
        }
    }

    fn restore(&mut self, snapshot: Bytes) {
        match self {
            GroupSm::Controller(c) => c.restore(snapshot),
            GroupSm::Partition(p) => p.restore(snapshot),
        }
    }
}

/// `entry` with its command narrowed to one kind of group's.
fn with<C: Clone>(entry: &Entry<GroupCommand>, command: &C) -> Entry<C> {
    Entry {
        index: entry.index,
        term: entry.term,
        commit_time_us: entry.commit_time_us,
        key: entry.key.clone(),
        command: command.clone(),
    }
}
