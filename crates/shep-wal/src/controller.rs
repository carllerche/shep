//! The node's side of the WAL controller (§9.6): founding the controller
//! group from the seed list, serving the controller's methods, and
//! reconciling this node's replicas with the controller's state.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use shep_raft::{ProposeError, RaftNodeError};
use shep_smp::CoreId;
use shep_wal_api::{Member, PartitionId};
use shep_wal_controller::{
    CONTROLLER_GROUP, ControllerState, CreateLogSetReply, CreateLogSetRequest, HelloReply,
    HelloRequest, NodeInfo, Outcome, Proposal, method, node_set,
};

use crate::{GroupProposal, GroupRejection, GroupResponse, WalError, WalNode};

/// How often founding and reconciling retry what didn't work yet.
const RETRY: Duration = Duration::from_millis(100);

impl WalNode {
    /// The controller's state as this node has applied it, if it hosts the
    /// controller.
    pub async fn controller_state(&self) -> Option<ControllerState> {
        let r = self
            .raft()
            .with_group(CONTROLLER_GROUP, |g| async move {
                g.with_state_machine(|sm| sm.controller().map(|c| c.state().clone()))
            })
            .await;
        r.ok().flatten()
    }

    /// Creates a log set: `partitions` partitions, `replication` replicas
    /// each, if Shep's rules allow it ([`crate::universe_rules`]). Only the
    /// controller's leader does it.
    pub async fn create_log_set(&self, req: CreateLogSetRequest) -> CreateLogSetReply {
        let r = self
            .raft()
            .with_group(CONTROLLER_GROUP, move |g| async move {
                let proposal = GroupProposal::Controller(Proposal::CreateLogSet {
                    name: req.name,
                    partitions: req.partitions,
                    replication: req.replication,
                });
                match g.propose(None, proposal).await {
                    Ok(d) => match d.response {
                        GroupResponse::Controller(Outcome::LogSetCreated(id)) => {
                            let state = g.with_state_machine(|sm| {
                                sm.controller().map(|c| c.state().clone())
                            });
                            CreateLogSetReply::Created {
                                id,
                                state: state.unwrap_or_default(),
                            }
                        }
                        GroupResponse::Controller(Outcome::Rejected(m)) => {
                            CreateLogSetReply::Rejected(m)
                        }
                        other => {
                            CreateLogSetReply::Rejected(format!("unexpected outcome {other:?}"))
                        }
                    },
                    Err(ProposeError::Rejected(GroupRejection::Controller(m))) => {
                        CreateLogSetReply::Rejected(m)
                    }
                    Err(ProposeError::NotLeader { leader }) => CreateLogSetReply::NotLeader {
                        leader: leader.map(|(node_id, addr)| NodeInfo { node_id, addr }),
                    },
                    Err(e) => CreateLogSetReply::Rejected(format!("{e:?}")),
                }
            })
            .await;
        r.unwrap_or_else(|e| CreateLogSetReply::Rejected(e.to_string()))
    }

    /// Whether this node belongs to a cluster, for a seed deciding whether
    /// to found one.
    pub async fn hello(&self) -> HelloReply {
        let node_id = self.node_id();
        let seeds = self.seeds();
        let fallback = seeds.clone();
        let r = self
            .raft()
            .with_group(CONTROLLER_GROUP, move |g| async move {
                let cluster_id = g.with_state_machine(|sm| {
                    sm.controller().and_then(|c| c.state().cluster_id.clone())
                });
                HelloReply {
                    node_id,
                    cluster_id: cluster_id.clone(),
                    empty: cluster_id.is_none() && g.log_range().end == 0,
                    seeds,
                }
            })
            .await;
        r.unwrap_or(HelloReply {
            node_id,
            cluster_id: None,
            empty: true,
            seeds: fallback,
        })
    }

    /// Opens this node's controller replica, on core 0, and starts founding
    /// and reconciling in the background.
    pub(crate) async fn start_controller(&self) -> Result<(), WalError> {
        self.raft()
            .create_group_on(CONTROLLER_GROUP, CoreId(0))
            .await?;
        let node = self.clone();
        self.spawn_background(async move { node.found().await });
        let node = self.clone();
        self.spawn_background(async move { node.reconcile().await });
        Ok(())
    }

    fn seeds(&self) -> Vec<NodeInfo> {
        let mut seeds: Vec<NodeInfo> = self
            .config()
            .seeds
            .iter()
            .map(|m| NodeInfo {
                node_id: m.node_id,
                addr: m.addr.clone(),
            })
            .collect();
        seeds.sort_by_key(|n| n.node_id);
        seeds
    }

    /// Founds the cluster, as Redpanda's `cluster_discovery` does: once
    /// every seed is reachable and empty, the lowest seed initializes the
    /// controller group with the seed list, and whichever seed then leads
    /// appends the founding command. A seed that finds a seed already in a
    /// cluster doesn't found one.
    async fn found(&self) {
        let seeds = self.seeds();
        let me = self.node_id();
        let initializer = seeds.first().map(|s| s.node_id) == Some(me);
        let members: BTreeMap<u64, String> =
            seeds.iter().map(|s| (s.node_id, s.addr.clone())).collect();
        loop {
            if self
                .controller_state()
                .await
                .is_some_and(|s| s.is_founded())
            {
                return;
            }
            if initializer && !self.controller_initialized().await {
                if self.seeds_empty(&seeds).await {
                    match self
                        .raft()
                        .initialize_group(CONTROLLER_GROUP, members.clone())
                        .await
                    {
                        Ok(()) | Err(RaftNodeError::AlreadyInitialized(_)) => {}
                        Err(e) => tracing::warn!("initializing the controller: {e}"),
                    }
                }
            } else {
                self.propose_founding(&seeds).await;
            }
            tokio::time::sleep(RETRY).await;
        }
    }

    async fn controller_initialized(&self) -> bool {
        let r = self
            .raft()
            .with_group(CONTROLLER_GROUP, |g| async move { g.log_range().end > 0 })
            .await;
        r.unwrap_or(false)
    }

    /// Whether every other seed answers, belongs to no cluster, and has the
    /// same seed list as this node.
    async fn seeds_empty(&self, seeds: &[NodeInfo]) -> bool {
        for s in seeds.iter().filter(|s| s.node_id != self.node_id()) {
            match self.peer_hello(&s.addr).await {
                Ok(h) if h.empty && node_set(&h.seeds) == node_set(seeds) => {}
                Ok(h) if h.empty => {
                    tracing::warn!(
                        seed = s.node_id,
                        "a seed has a different seed list; not founding"
                    );
                    return false;
                }
                Ok(h) => {
                    if h.cluster_id.is_some() {
                        tracing::warn!(
                            seed = s.node_id,
                            "a seed already belongs to a cluster; not founding one"
                        );
                    }
                    return false;
                }
                Err(e) => {
                    tracing::debug!(seed = s.node_id, "seed unreachable: {e}");
                    return false;
                }
            }
        }
        true
    }

    async fn peer_hello(&self, addr: &str) -> Result<HelloReply, String> {
        let addr = addr.parse().map_err(|e| format!("{addr}: {e}"))?;
        let body = self
            .peers()
            .call(
                addr,
                method::HELLO,
                shep_codec::to_bytes(&HelloRequest {}),
                Duration::from_secs(1),
            )
            .await
            .map_err(|e| e.to_string())?;
        shep_codec::from_bytes(&body).map_err(|e| e.to_string())
    }

    /// If this node leads the initialized, unfounded controller, appends the
    /// founding command. Only the first one applies.
    async fn propose_founding(&self, seeds: &[NodeInfo]) {
        let members = seeds.to_vec();
        let _ = self
            .raft()
            .with_group(CONTROLLER_GROUP, move |g| async move {
                if !g.leadership().is_leader {
                    return;
                }
                let cluster_id = rand::random::<[u8; 16]>().to_vec();
                let proposal = GroupProposal::Controller(Proposal::Found {
                    cluster_id,
                    members,
                });
                let _ = g.propose(None, proposal).await;
            })
            .await;
    }

    /// Opens a replica for every partition the controller assigns this node,
    /// as Redpanda's `controller_backend` does, and has the lowest assigned
    /// node initialize each new partition's group. Partitions are never
    /// removed yet: there are no moves or drops.
    async fn reconcile(&self) {
        let me = self.node_id();
        let mut initialized: BTreeSet<PartitionId> = BTreeSet::new();
        let mut revisions = loop {
            let rx = self
                .raft()
                .with_group(CONTROLLER_GROUP, |g| async move {
                    g.with_state_machine(|sm| sm.controller().map(|c| c.subscribe()))
                })
                .await;
            match rx {
                Ok(Some(rx)) => break rx,
                _ => tokio::time::sleep(RETRY).await,
            }
        };
        loop {
            let mut pending = false;
            if let Some(state) = self.controller_state().await.filter(|s| s.is_founded()) {
                let addrs = state.addrs();
                for (p, partition) in state.assigned_to(me) {
                    if self.raft().core_of(shep_raft::GroupId(p.0)).is_none()
                        && let Err(e) = self.create_replica(p).await
                    {
                        tracing::warn!(partition = %p, "creating a replica: {e}");
                        pending = true;
                        continue;
                    }
                    if initialized.contains(&p) || partition.replicas.iter().min() != Some(&me) {
                        continue;
                    }
                    let members: Vec<Member> = partition
                        .replicas
                        .iter()
                        .filter_map(|id| {
                            addrs.get(id).map(|addr| Member {
                                node_id: *id,
                                addr: addr.clone(),
                            })
                        })
                        .collect();
                    match self.initialize_group(p, &members).await {
                        Ok(()) | Err(WalError::Raft(RaftNodeError::AlreadyInitialized(_))) => {
                            initialized.insert(p);
                        }
                        Err(e) => {
                            tracing::warn!(partition = %p, "initializing: {e}");
                            pending = true;
                        }
                    }
                }
            }
            let wait = if pending {
                RETRY
            } else {
                Duration::from_secs(1)
            };
            if let Ok(Err(_)) = tokio::time::timeout(wait, revisions.changed()).await {
                return;
            }
        }
    }
}
