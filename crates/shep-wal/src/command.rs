//! What a partition's log holds.

use shep_codec::{Decode, Encode};
use shep_wal_api::{Dvv, KeyRange};

/// A proposal as the leader admits it, with the write set the next leader
/// needs to rebuild its validation table. The admitting term, commit time and
/// idempotency key are shep-raft's, around it.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum PartitionCommand {
    Mutation {
        context: Dvv,
        write_set: Vec<KeyRange>,
        payload: Vec<u8>,
        reply: Vec<u8>,
    },
    Failure {
        reply: Vec<u8>,
    },
}

impl PartitionCommand {
    pub fn reply(&self) -> &Vec<u8> {
        match self {
            PartitionCommand::Mutation { reply, .. } | PartitionCommand::Failure { reply } => reply,
        }
    }

    pub fn failed(&self) -> bool {
        matches!(self, PartitionCommand::Failure { .. })
    }

    pub fn write_set(&self) -> &[KeyRange] {
        match self {
            PartitionCommand::Mutation { write_set, .. } => write_set,
            PartitionCommand::Failure { .. } => &[],
        }
    }
}
