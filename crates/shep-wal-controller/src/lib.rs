//! The WAL controller (design §9.6): the WAL service's own Raft group, with
//! every WAL node a member, after Redpanda's controller (`raft0`).
//!
//! Its log holds typed [`Command`]s. Every node applies them in order to the
//! same [`ControllerState`]: the members table, the log sets, and the
//! partition table, as Redpanda's `members_table` and `topic_table` are. A
//! command's index in the log is its **revision**, the epoch every decision
//! derived from it carries.
//!
//! A **log set** is a named group of partitions, created together and
//! placed by the controller. What its partitions mean is the embedder's:
//! Shep reads its first as a universe's meta partition and the rest as its
//! data partitions. The embedder's rules for log sets, how many partitions
//! one may have and what names it takes, are data ([`LogSetRules`]), the
//! same on every node.
//!
//! This crate is the state and the decisions only: the controller group's
//! [`ControllerSm`], which admits proposals on the leader (choosing where
//! a new log set's replicas go) and applies commands everywhere. The WAL
//! node runs the group and reconciles its replicas with the state.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use shep_codec::{Decode, Encode};
use shep_raft::{Admit, Entry, GroupId, Machine, NodeId};
use shep_wal_api::PartitionId;
use tokio::sync::watch;

/// The controller's Raft group. Partition groups take every other ID.
pub const CONTROLLER_GROUP: GroupId = GroupId(0);

/// The longest domain name a [`NameRule::Domain`] accepts, and the longest
/// of its labels.
pub const MAX_DOMAIN_NAME: usize = 253;
pub const MAX_DOMAIN_LABEL: usize = 63;

/// The longest name a [`NameRule::Any`] accepts, in bytes.
pub const MAX_NAME: usize = 255;

/// What log sets an embedder allows. Every node must have the same rules:
/// applying a command checks them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogSetRules {
    /// The most partitions one log set may have. It bounds the work and the
    /// size of the command that creates one.
    pub max_partitions: usize,
    pub names: NameRule,
}

impl Default for LogSetRules {
    fn default() -> LogSetRules {
        LogSetRules {
            max_partitions: 1024,
            names: NameRule::Any,
        }
    }
}

/// What a log set may be named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameRule {
    /// 1 to [`MAX_NAME`] bytes.
    Any,
    /// A DNS domain: labels of letters, digits and inner hyphens, compared
    /// without case and stored lowercased.
    Domain,
}

impl NameRule {
    /// `name` as stored, if the rule allows it.
    pub fn check(self, name: &str) -> Result<String, String> {
        match self {
            NameRule::Any => {
                if name.is_empty() || name.len() > MAX_NAME {
                    return Err(format!(
                        "a name takes 1 to {MAX_NAME} bytes, not {}",
                        name.len()
                    ));
                }
                Ok(name.to_string())
            }
            NameRule::Domain => {
                if name.is_empty() || name.len() > MAX_DOMAIN_NAME {
                    return Err(format!(
                        "a domain takes 1 to {MAX_DOMAIN_NAME} characters, not {}",
                        name.len()
                    ));
                }
                for label in name.split('.') {
                    let valid = (1..=MAX_DOMAIN_LABEL).contains(&label.len())
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        && !label.starts_with('-')
                        && !label.ends_with('-');
                    if !valid {
                        return Err(format!(
                            "{name:?} isn't a domain: label {label:?} needs 1 to \
                             {MAX_DOMAIN_LABEL} letters, digits or inner hyphens"
                        ));
                    }
                }
                Ok(name.to_ascii_lowercase())
            }
        }
    }

    /// Whether two stored names name the same log set.
    fn same(self, a: &str, b: &str) -> bool {
        match self {
            NameRule::Any => a == b,
            NameRule::Domain => a.eq_ignore_ascii_case(b),
        }
    }
}

/// A log set's ID: assigned by the controller, never reused.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
#[codec(transparent)]
pub struct LogSetId(pub u64);

/// What a node asks the controller for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proposal {
    /// The first command: the cluster's identity and founding members.
    Found {
        cluster_id: Bytes,
        members: Vec<NodeInfo>,
    },
    AddNode(NodeInfo),
    /// A log set of `partitions` partitions, `replication` replicas each,
    /// placed by the leader.
    CreateLogSet {
        name: String,
        partitions: u32,
        replication: u32,
    },
}

/// What the controller's log holds.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Command {
    /// The first command: the cluster's identity and founding members.
    Found {
        cluster_id: Bytes,
        members: Vec<NodeInfo>,
    },
    AddNode(NodeInfo),
    /// A log set whose partitions have the replicas the admitting leader
    /// chose, in order. Partition IDs are assigned when the command applies.
    CreateLogSet {
        name: String,
        partitions: Vec<Vec<NodeId>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct NodeInfo {
    pub node_id: NodeId,
    pub addr: String,
}

/// What applying a command decided: the state machine's response.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Outcome {
    Ok,
    LogSetCreated(LogSetId),
    Rejected(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Member {
    pub addr: String,
    /// The revision that added it.
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct LogSet {
    pub name: String,
    /// In the order the creating command listed them.
    pub partitions: Vec<PartitionId>,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Partition {
    pub log_set: LogSetId,
    /// The nodes that hold its replicas.
    pub replicas: Vec<NodeId>,
    /// The revision that created it.
    pub revision: u64,
}

/// The controller's state: a fold of its log.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct ControllerState {
    pub cluster_id: Option<Bytes>,
    /// The index of the last command applied.
    pub revision: u64,
    pub members: BTreeMap<NodeId, Member>,
    pub log_sets: BTreeMap<LogSetId, LogSet>,
    pub partitions: BTreeMap<PartitionId, Partition>,
    next_log_set: u64,
    next_partition: u64,
}

impl ControllerState {
    pub fn is_founded(&self) -> bool {
        self.cluster_id.is_some()
    }

    /// The log set `name` names, under `rules`.
    pub fn log_set_by_name(&self, rules: &LogSetRules, name: &str) -> Option<(LogSetId, &LogSet)> {
        self.log_sets
            .iter()
            .find(|(_, s)| rules.names.same(&s.name, name))
            .map(|(&id, s)| (id, s))
    }

    /// The partitions whose replicas include `node`.
    pub fn assigned_to(
        &self,
        node: NodeId,
    ) -> impl Iterator<Item = (PartitionId, &Partition)> + '_ {
        self.partitions
            .iter()
            .filter(move |(_, p)| p.replicas.contains(&node))
            .map(|(&id, p)| (id, p))
    }

    /// Every member's address, by node.
    pub fn addrs(&self) -> BTreeMap<NodeId, String> {
        self.members
            .iter()
            .map(|(&id, m)| (id, m.addr.clone()))
            .collect()
    }

    /// Applies one command at `revision`. Deterministic: every node gets the
    /// same state from the same log and rules.
    pub fn apply(&mut self, rules: &LogSetRules, revision: u64, cmd: Command) -> Outcome {
        self.revision = revision;
        match cmd {
            Command::Found {
                cluster_id,
                members,
            } => {
                if self.cluster_id.is_some() {
                    return Outcome::Rejected("already founded".into());
                }
                self.cluster_id = Some(cluster_id);
                self.next_partition = CONTROLLER_GROUP.0 + 1;
                for m in members {
                    self.members.insert(
                        m.node_id,
                        Member {
                            addr: m.addr,
                            revision,
                        },
                    );
                }
                Outcome::Ok
            }
            Command::AddNode(n) => {
                if self.members.contains_key(&n.node_id) {
                    return Outcome::Rejected(format!("node {} is already a member", n.node_id));
                }
                self.members.insert(
                    n.node_id,
                    Member {
                        addr: n.addr,
                        revision,
                    },
                );
                Outcome::Ok
            }
            Command::CreateLogSet { name, partitions } => {
                if !self.is_founded() {
                    return Outcome::Rejected("not founded".into());
                }
                let name = match rules.names.check(&name) {
                    Ok(name) => name,
                    Err(e) => return Outcome::Rejected(e),
                };
                if partitions.is_empty() || partitions.len() > rules.max_partitions {
                    return Outcome::Rejected(format!(
                        "{} partitions; a log set takes 1 to {}",
                        partitions.len(),
                        rules.max_partitions
                    ));
                }
                if self.log_set_by_name(rules, &name).is_some() {
                    return Outcome::Rejected(format!("log set {name:?} exists"));
                }
                let malformed = partitions
                    .iter()
                    .any(|r| r.is_empty() || r.iter().collect::<BTreeSet<_>>().len() != r.len());
                if malformed {
                    return Outcome::Rejected("a partition needs distinct replicas".into());
                }
                let unknown = partitions
                    .iter()
                    .flatten()
                    .find(|n| !self.members.contains_key(n));
                if let Some(n) = unknown {
                    return Outcome::Rejected(format!("node {n} isn't a member"));
                }
                let id = LogSetId(self.next_log_set);
                self.next_log_set += 1;
                let partitions = partitions
                    .into_iter()
                    .map(|replicas| {
                        let p = PartitionId(self.next_partition);
                        self.next_partition += 1;
                        self.partitions.insert(
                            p,
                            Partition {
                                log_set: id,
                                replicas,
                                revision,
                            },
                        );
                        p
                    })
                    .collect();
                self.log_sets.insert(
                    id,
                    LogSet {
                        name,
                        partitions,
                        revision,
                    },
                );
                Outcome::LogSetCreated(id)
            }
        }
    }

    /// Chooses `n` replicas for a new partition: the members with the fewest
    /// replicas, then the lowest IDs (Redpanda's allocator balances counts).
    /// `pending` counts replicas already chosen for this proposal.
    pub fn allocate(
        &self,
        n: usize,
        pending: &BTreeMap<NodeId, usize>,
    ) -> Result<Vec<NodeId>, String> {
        if n == 0 || n > self.members.len() {
            return Err(format!(
                "can't place {n} replicas on {} members",
                self.members.len()
            ));
        }
        let mut load: BTreeMap<NodeId, usize> = self.members.keys().map(|&id| (id, 0)).collect();
        for p in self.partitions.values() {
            for r in &p.replicas {
                *load.entry(*r).or_default() += 1;
            }
        }
        for (id, extra) in pending {
            *load.entry(*id).or_default() += extra;
        }
        let mut by_load: Vec<(usize, NodeId)> = load.into_iter().map(|(id, l)| (l, id)).collect();
        by_load.sort();
        let mut chosen: Vec<NodeId> = by_load.into_iter().take(n).map(|(_, id)| id).collect();
        chosen.sort();
        Ok(chosen)
    }

    /// The replicas for a new log set's `partitions` partitions, each with
    /// `replication` replicas, balanced. At most `rules.max_partitions`,
    /// checked before any are placed.
    pub fn plan(
        &self,
        rules: &LogSetRules,
        partitions: usize,
        replication: usize,
    ) -> Result<Vec<Vec<NodeId>>, String> {
        if partitions == 0 || partitions > rules.max_partitions {
            return Err(format!(
                "{partitions} partitions; a log set takes 1 to {}",
                rules.max_partitions
            ));
        }
        let mut pending: BTreeMap<NodeId, usize> = BTreeMap::new();
        (0..partitions)
            .map(|_| {
                let r = self.allocate(replication, &pending)?;
                for id in &r {
                    *pending.entry(*id).or_default() += 1;
                }
                Ok(r)
            })
            .collect()
    }
}

/// The controller group's state machine.
pub struct ControllerSm {
    rules: LogSetRules,
    state: ControllerState,
    revision: watch::Sender<u64>,
}

impl ControllerSm {
    pub fn new(rules: LogSetRules) -> ControllerSm {
        ControllerSm {
            rules,
            state: ControllerState::default(),
            revision: watch::channel(0).0,
        }
    }

    pub fn rules(&self) -> &LogSetRules {
        &self.rules
    }

    pub fn state(&self) -> &ControllerState {
        &self.state
    }

    /// Changes whenever a command applies, to its revision.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }
}

impl Machine for ControllerSm {
    type Proposal = Proposal;
    type Command = Command;
    type Rejection = String;
    type Response = Outcome;

    /// Places a new log set's replicas against the applied state, after
    /// checking it against the rules; applying checks again.
    fn admit(&mut self, _: &Admit, proposal: Proposal) -> Result<Command, String> {
        Ok(match proposal {
            Proposal::Found {
                cluster_id,
                members,
            } => Command::Found {
                cluster_id,
                members,
            },
            Proposal::AddNode(n) => Command::AddNode(n),
            Proposal::CreateLogSet {
                name,
                partitions,
                replication,
            } => {
                let name = self.rules.names.check(&name)?;
                let partitions =
                    self.state
                        .plan(&self.rules, partitions as usize, replication as usize)?;
                Command::CreateLogSet { name, partitions }
            }
        })
    }

    fn apply(&mut self, entry: &Entry<Command>) -> Outcome {
        let outcome = self
            .state
            .apply(&self.rules, entry.index, entry.command.clone());
        self.revision.send_replace(entry.index);
        outcome
    }

    fn snapshot(&self) -> Bytes {
        shep_codec::to_bytes(&self.state)
    }

    fn restore(&mut self, snapshot: Bytes) {
        match shep_codec::from_bytes(snapshot) {
            Ok(state) => {
                self.state = state;
                self.revision.send_replace(self.state.revision);
            }
            Err(e) => tracing::error!("undecodable controller snapshot: {e}"),
        }
    }
}

// ---- the controller's RPC service (shep-rpc service 3) ----

pub mod method {
    pub const SERVICE: u16 = 3;
    pub const STATE: u32 = shep_rpc::method_id(SERVICE, 1);
    pub const CREATE_LOG_SET: u32 = shep_rpc::method_id(SERVICE, 2);
    /// Whether this node is part of a cluster yet (used in founding).
    pub const HELLO: u32 = shep_rpc::method_id(SERVICE, 3);

    pub fn is_controller(method: u32) -> bool {
        (method >> 16) as u16 == SERVICE
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct StateRequest {}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum StateReply {
    State(ControllerState),
    /// This node hosts no controller.
    NoController,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct CreateLogSetRequest {
    pub name: String,
    pub partitions: u32,
    pub replication: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum CreateLogSetReply {
    Created {
        id: LogSetId,
        state: ControllerState,
    },
    NotLeader {
        leader: Option<NodeInfo>,
    },
    Rejected(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct HelloRequest {}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct HelloReply {
    pub node_id: NodeId,
    /// The cluster this node belongs to, if any.
    pub cluster_id: Option<Bytes>,
    /// Whether this node has any controller state, even before founding.
    pub empty: bool,
    /// The node's configured seed list. Founding requires every seed to
    /// report the same one, so nodes with overlapping but different lists
    /// can't each found a cluster.
    pub seeds: Vec<NodeInfo>,
}

/// The set of nodes in a list, for comparing seed lists.
pub fn node_set(nodes: &[NodeInfo]) -> BTreeSet<(NodeId, String)> {
    nodes.iter().map(|n| (n.node_id, n.addr.clone())).collect()
}
