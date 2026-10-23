//! A node: Raft groups on every core of an [`Smp`] the node starts, and the
//! RPC service that routes peers' messages to the core that owns each group.
//!
//! Which core hosts a group is the node's own business (Redpanda's node-local
//! core assignment). Peers address groups, and the receiving node hops to
//! the owning core. Each core persists the groups it hosts in its key-value
//! store and reopens them on restart.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use shep_raft_engine::async_runtime::WatchReceiver;
use shep_raft_engine::errors::{ClientWriteError, InitializeError, RaftError};
use shep_raft_engine::impls::BasicNode;
use shep_raft_engine::raft::{AppendEntriesRequest, TransferLeaderRequest, VoteRequest};
use shep_raft_engine::{Config, Raft, ServerState, SnapshotPolicy};
use shep_rpc_client::ReconnectConfig;
use shep_rpc_server::{Request, Response, Server, ServerConfig, service_fn};
use shep_smp::{CoreId, Smp, SmpConfig, SmpError};
use shep_storage::{KeySpace, Log, Storage, StorageConfig};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

use crate::log_storage::GroupLog;
use crate::machine::{Decided, Envelope, Fetched, LogRecord, Machine, ProposeError, RecordKind};
use crate::network::{Faults, NetConfig, NetStats, NetworkFactory, Peers};
use crate::proposer::ProposerHandle;
use crate::state_machine::SmAdapter;
use crate::types::{
    GroupId, LogId, NodeId, RaftSnapshot, TypeConfig, Vote, WireEntry, from_opt_log_id, opt_log_id,
};
use crate::wire::{self, AppendReply, AppendRequest};

#[derive(Clone, Debug)]
pub struct RaftConfig {
    pub node_id: NodeId,
    /// Group logs live in `logs/<group>`, shared by the cores; each core's
    /// key-value store in `cores/<n>`. On local disk: the node locks it
    /// against other processes, which network filesystems may not honor.
    pub data_dir: PathBuf,
    /// The engine also uses this as each AppendEntries RPC's deadline, and a
    /// follower answers only once entries are synced, so it must be well
    /// above the disk's sync latency.
    pub heartbeat_interval: Duration,
    pub election_timeout: Range<Duration>,
    /// AppendEntries in flight per follower per group.
    pub append_window: usize,
    /// About the most entry bytes one AppendEntries carries; a larger entry
    /// goes alone. Peers' `server.max_payload` must exceed the largest entry.
    pub max_append_bytes: usize,
    /// How long a core gathers heartbeats for one peer before sending them.
    pub heartbeat_linger: Duration,
    pub rpc_timeout: Duration,
    /// How long a follower holds a replication request whose predecessors
    /// haven't arrived before rejecting it.
    pub append_gap_timeout: Duration,
    pub reconnect: ReconnectConfig,
    pub storage: StorageConfig,
    pub server: ServerConfig,
    pub proposals: ProposalConfig,
}

/// How a group's leader handles proposals.
#[derive(Clone, Debug)]
pub struct ProposalConfig {
    /// How long a proposal waits to commit before its proposer is told the
    /// outcome is unknown.
    pub commit_timeout: Duration,
    /// How long a decided idempotency key is remembered.
    pub idempotency_window: Duration,
    /// The largest entry a leader appends. Replication and fetch messages
    /// must each fit at least one.
    pub max_entry_bytes: usize,
    /// How far back a new leader replays its log into its machine
    /// ([`Machine::replay`]), by commit time and by count; entries not yet
    /// applied are replayed regardless.
    pub admission_window: Duration,
    pub admission_window_entries: usize,
}

impl Default for ProposalConfig {
    fn default() -> ProposalConfig {
        ProposalConfig {
            commit_timeout: Duration::from_secs(5),
            idempotency_window: Duration::from_secs(24 * 60 * 60),
            max_entry_bytes: 16 << 20,
            admission_window: Duration::from_secs(5),
            admission_window_entries: 1 << 20,
        }
    }
}

impl RaftConfig {
    pub fn new(node_id: NodeId, data_dir: impl Into<PathBuf>) -> RaftConfig {
        RaftConfig {
            node_id,
            data_dir: data_dir.into(),
            heartbeat_interval: Duration::from_millis(100),
            election_timeout: Duration::from_millis(1000)..Duration::from_millis(2000),
            append_window: 16,
            max_append_bytes: 1 << 20,
            heartbeat_linger: Duration::from_millis(2),
            rpc_timeout: Duration::from_secs(5),
            append_gap_timeout: Duration::from_millis(500),
            reconnect: ReconnectConfig::default(),
            storage: StorageConfig::default(),
            server: ServerConfig::default(),
            proposals: ProposalConfig::default(),
        }
    }

    fn engine_config(&self) -> Result<Arc<Config>, RaftNodeError> {
        let config = Config {
            cluster_name: "shep".into(),
            heartbeat_interval: self.heartbeat_interval.as_millis() as u64,
            election_timeout_min: self.election_timeout.start.as_millis() as u64,
            election_timeout_max: self.election_timeout.end.as_millis() as u64,
            // Each core's heartbeat ticker triggers its leaders together, so
            // their heartbeats share one RPC per peer.
            enable_heartbeat: false,
            // Every reader of the log needs it; nothing is purged until
            // archiving exists.
            snapshot_policy: SnapshotPolicy::Never,
            max_in_snapshot_log_to_keep: u64::MAX,
            // A restarted leader wins a new term before it leads again.
            // Replication session ids restart with the process, so followers
            // would take a restored leader's sessions for stale ones; and
            // state machines rebuild from the log rather than persist, which
            // openraft says restoring needs.
            enable_leader_restore: Some(false),
            ..Default::default()
        };
        Ok(Arc::new(
            config
                .validate()
                .map_err(|e| RaftNodeError::Raft(e.to_string()))?,
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RaftNodeError {
    #[error("storage: {0}")]
    Storage(#[from] shep_storage::Error),
    #[error("raft: {0}")]
    Raft(String),
    #[error("no group {0} on this node")]
    NoSuchGroup(GroupId),
    #[error("group {0} is already initialized")]
    AlreadyInitialized(GroupId),
    #[error(transparent)]
    Smp(#[from] SmpError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Another process, or another node in this one, holds the data
    /// directory.
    #[error("{0} is locked: another node is running on this directory")]
    DataDirLocked(PathBuf),
    /// The node has fewer cores than when `core` took on groups. Replicas
    /// don't move between cores yet, so start with at least `core + 1`.
    #[error("core {core} hosts {groups} groups, but the node has {cores} cores")]
    CoreRemoved {
        core: usize,
        groups: usize,
        cores: usize,
    },
}

/// Why an append wasn't committed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum WriteError {
    /// Refused before it was appended: this replica isn't the leader, or its
    /// lease isn't valid yet. Nothing was written.
    #[error("not the leader (leader: {leader:?})")]
    NotLeader { leader: Option<(NodeId, String)> },
    /// Appended, then truncated or purged before it was known to commit;
    /// another leader may have committed it.
    #[error("discarded (leader: {leader:?})")]
    Discarded { leader: Option<(NodeId, String)> },
    #[error("{0}")]
    Other(String),
}

/// A committed append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Written {
    pub term: u64,
    pub index: u64,
    /// What the state machine returned for it.
    pub response: Vec<u8>,
}

/// What a node knows of one of its groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStatus {
    pub core: CoreId,
    pub term: u64,
    pub leader: Option<NodeId>,
    pub is_leader: bool,
    pub last_log_index: Option<u64>,
    pub committed: Option<u64>,
    pub applied: Option<u64>,
    pub voters: BTreeSet<NodeId>,
}

type MakeSm<M> = dyn Fn(GroupId) -> M + Send + Sync;

/// A Raft node. Cheap to clone; usable from any thread.
pub struct RaftNode<M: Machine> {
    shared: Arc<Shared<M>>,
}

impl<M: Machine> Clone for RaftNode<M> {
    fn clone(&self) -> Self {
        RaftNode {
            shared: self.shared.clone(),
        }
    }
}

struct Shared<M: Machine> {
    config: Arc<RaftConfig>,
    cores: Smp<Core<M>>,
    /// Which core hosts each group.
    routes: RwLock<HashMap<GroupId, CoreId>>,
    shutdown: watch::Sender<bool>,
    servers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    creating: tokio::sync::Mutex<()>,
    stats: Arc<NetStats>,
    faults: Arc<Faults>,
    /// The data directory's lock, held until shutdown.
    lock: Mutex<Option<std::fs::File>>,
}

/// Counters for a node's outbound Raft traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RaftStats {
    /// RPCs carrying heartbeats, and the heartbeats they carried.
    pub heartbeat_rpcs: u64,
    pub heartbeats: u64,
    pub append_rpcs: u64,
    /// The most AppendEntries one replication stream has had in flight.
    pub max_append_in_flight: u64,
}

impl<M: Machine> RaftNode<M> {
    /// Starts the cores `smp` describes and, on each, opens its storage and
    /// reopens the groups it hosted. Then creates each of `required` that no
    /// core hosts, on the core given; one hosted on another core fails the
    /// start. Blocks until all that is done, so when it returns the node is
    /// set up and nothing has been served: the caller needn't be async.
    /// Serving ([`RaftNode::serve`]) needs a
    /// [`LocalRuntime`](tokio::runtime::LocalRuntime) on the calling thread.
    pub fn start(
        config: RaftConfig,
        smp: SmpConfig,
        make_sm: impl Fn(GroupId) -> M + Send + Sync + 'static,
        required: &[(GroupId, CoreId)],
    ) -> Result<RaftNode<M>, RaftNodeError> {
        let config = Arc::new(config);
        let cores = smp.cores.max(1);
        // Fail on a bad config here, not on every core.
        config.engine_config()?;
        if let Some(&(_, core)) = required.iter().find(|(_, core)| core.0 >= cores) {
            return Err(SmpError::NoSuchCore(core).into());
        }
        let lock = lock_data_dir(&config.data_dir)?;
        check_removed_cores(&config, cores)?;
        let make_sm: Arc<MakeSm<M>> = Arc::new(make_sm);
        let stats = Arc::new(NetStats::default());
        let faults = Arc::new(Faults::default());
        let hosted = Arc::new(Mutex::new(Vec::new()));
        let (c, m, st, f, h) = (
            config.clone(),
            make_sm.clone(),
            stats.clone(),
            faults.clone(),
            hosted.clone(),
        );
        let cores = Smp::start(smp, move |id| {
            let (config, make_sm, stats, faults, hosted) =
                (c.clone(), m.clone(), st.clone(), f.clone(), h.clone());
            async move {
                let core = Core::open(id, config, make_sm, stats, faults).await?;
                hosted.lock().unwrap().push((id, core.group_ids()));
                Ok::<_, RaftNodeError>(core)
            }
        })?;
        let hosted = std::mem::take(&mut *hosted.lock().unwrap());
        let routes = match create_required(&cores, hosted, required) {
            Ok(routes) => routes,
            Err(e) => {
                stop_cores_blocking(cores);
                return Err(e);
            }
        };
        Ok(RaftNode {
            shared: Arc::new(Shared {
                config,
                cores,
                routes: RwLock::new(routes),
                shutdown: watch::channel(false).0,
                servers: Mutex::new(Vec::new()),
                creating: tokio::sync::Mutex::new(()),
                lock: Mutex::new(Some(lock)),
                stats,
                faults,
            }),
        })
    }

    pub fn node_id(&self) -> NodeId {
        self.shared.config.node_id
    }

    pub fn faults(&self) -> &Faults {
        &self.shared.faults
    }

    /// Whether `method` is one of the Raft service's, for a server that hosts
    /// several services.
    pub fn handles(method: u32) -> bool {
        (method >> 16) as u16 == wire::SERVICE
    }

    /// Serves one Raft RPC; for a server that hosts several services.
    pub async fn handle(&self, req: Request) -> Response {
        self.shared.dispatch(req).await
    }

    pub fn stats(&self) -> RaftStats {
        let s = &self.shared.stats;
        let load = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
        RaftStats {
            heartbeat_rpcs: load(&s.heartbeat_rpcs),
            heartbeats: load(&s.heartbeats),
            append_rpcs: load(&s.append_rpcs),
            max_append_in_flight: load(&s.max_append_in_flight),
        }
    }

    /// Serves peers on `listener` until [`RaftNode::shutdown`], from a task
    /// on the calling thread, which must run a
    /// [`LocalRuntime`](tokio::runtime::LocalRuntime).
    pub fn serve(&self, listener: TcpListener) {
        let weak = Arc::downgrade(&self.shared);
        let service = service_fn(move |req: Request| {
            let shared = weak.upgrade();
            async move {
                match shared {
                    Some(s) => s.dispatch(req).await,
                    None => Response::error(wire::status::UNAVAILABLE),
                }
            }
        });
        let server = Server::new(self.shared.config.server.clone(), service);
        let mut shutdown = self.shared.shutdown.subscribe();
        let task = tokio::task::spawn_local(async move {
            tokio::select! {
                _ = server.serve(listener) => {}
                _ = async { let _ = shutdown.wait_for(|s| *s).await; } => {
                    server.shutdown(Duration::from_millis(100)).await
                }
            }
        });
        self.shared.servers.lock().unwrap().push(task);
    }

    /// The groups this node hosts.
    pub fn groups(&self) -> Vec<GroupId> {
        let mut g: Vec<_> = self.shared.routes.read().unwrap().keys().copied().collect();
        g.sort();
        g
    }

    pub fn core_of(&self, group: GroupId) -> Option<CoreId> {
        self.shared.routes.read().unwrap().get(&group).copied()
    }

    /// Creates this node's replica of `group`, empty, on the core with the
    /// fewest groups, or does nothing if it has one.
    pub async fn create_group(&self, group: GroupId) -> Result<CoreId, RaftNodeError> {
        self.create_group_where(group, None).await
    }

    /// Like [`RaftNode::create_group`], on a chosen core.
    pub async fn create_group_on(
        &self,
        group: GroupId,
        core: CoreId,
    ) -> Result<CoreId, RaftNodeError> {
        self.create_group_where(group, Some(core)).await
    }

    async fn create_group_where(
        &self,
        group: GroupId,
        core: Option<CoreId>,
    ) -> Result<CoreId, RaftNodeError> {
        // One creation at a time, so a group can't be created on two cores.
        let _creating = self.shared.creating.lock().await;
        if let Some(core) = self.core_of(group) {
            return Ok(core);
        }
        let core = match core {
            Some(core) => core,
            None => {
                let counts = self
                    .shared
                    .cores
                    .invoke_on_all(|c| async move { c.groups.borrow().len() })
                    .await?;
                CoreId(
                    counts
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, n)| **n)
                        .map_or(0, |(i, _)| i),
                )
            }
        };
        self.shared
            .cores
            .submit_to(core, move |c| async move {
                c.create(group).await.map_err(|e| e.to_string())
            })
            .await?
            .map_err(RaftNodeError::Raft)?;
        self.shared.routes.write().unwrap().insert(group, core);
        Ok(core)
    }

    /// Initializes `group` with its first voters. Call it on one member,
    /// after every member created its replica.
    pub async fn initialize_group(
        &self,
        group: GroupId,
        members: BTreeMap<NodeId, String>,
    ) -> Result<(), RaftNodeError> {
        let core = self.route(group)?;
        self.shared
            .cores
            .submit_to(core, move |c| async move {
                let g = c.group(group).ok_or(RaftNodeError::NoSuchGroup(group))?;
                let nodes: BTreeMap<NodeId, BasicNode> = members
                    .into_iter()
                    .map(|(id, a)| (id, BasicNode::new(a)))
                    .collect();
                g.raft.initialize(nodes).await.map_err(|e| match e {
                    RaftError::APIError(InitializeError::NotAllowed(_)) => {
                        RaftNodeError::AlreadyInitialized(group)
                    }
                    e => RaftNodeError::Raft(e.to_string()),
                })
            })
            .await?
    }

    /// Proposes to `group`, which must be led by this node, and waits for it
    /// to be decided: see [`GroupHandle::propose`].
    pub async fn propose(
        &self,
        group: GroupId,
        key: Option<Vec<u8>>,
        proposal: M::Proposal,
    ) -> Result<Decided<M::Response>, ProposeError<M::Rejection>>
    where
        M::Proposal: Send,
        M::Response: Send,
        M::Rejection: Send,
    {
        let Some(core) = self.core_of(group) else {
            return Err(ProposeError::NoSuchGroup(group));
        };
        self.shared
            .cores
            .submit_to(core, move |c| async move {
                let g = c.group(group).ok_or(ProposeError::NoSuchGroup(group))?;
                GroupHandle { g, id: group }.propose(key, proposal).await
            })
            .await
            .unwrap_or_else(|e| Err(ProposeError::Unknown(e.to_string())))
    }

    /// Asks `group`'s leader, which must be this node, to hand leadership to
    /// `to`. Returns once asked; the transfer completes in the background.
    pub async fn transfer_leader(&self, group: GroupId, to: NodeId) -> Result<(), RaftNodeError> {
        let core = self.route(group)?;
        self.shared
            .cores
            .submit_to(core, move |c| async move {
                let g = c.group(group).ok_or(RaftNodeError::NoSuchGroup(group))?;
                g.raft
                    .trigger()
                    .transfer_leader(to)
                    .await
                    .map_err(|e| RaftNodeError::Raft(e.to_string()))
            })
            .await?
    }

    pub async fn status(&self, group: GroupId) -> Result<GroupStatus, RaftNodeError> {
        let core = self.route(group)?;
        self.shared
            .cores
            .submit_to(core, move |c| async move {
                let g = c.group(group).ok_or(RaftNodeError::NoSuchGroup(group))?;
                let m = g.raft.metrics().borrow_watched().clone();
                Ok(GroupStatus {
                    core: c.id,
                    term: m.current_term,
                    leader: m.current_leader,
                    is_leader: m.state == ServerState::Leader,
                    last_log_index: m.last_log_index,
                    committed: m.cluster_committed.map(|l| l.index),
                    applied: m.last_applied.map(|l| l.index),
                    voters: m.membership_config.membership().voter_ids().collect(),
                })
            })
            .await?
    }

    /// Runs `f` on `group`'s core with a handle to it.
    pub async fn with_group<F, Fut, R>(&self, group: GroupId, f: F) -> Result<R, RaftNodeError>
    where
        F: FnOnce(GroupHandle<M>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = R> + 'static,
        R: Send + 'static,
    {
        let core = self.route(group)?;
        self.shared
            .cores
            .submit_to(core, move |c| async move {
                match c.group(group) {
                    Some(g) => Ok(f(GroupHandle { g, id: group }).await),
                    None => Err(RaftNodeError::NoSuchGroup(group)),
                }
            })
            .await?
    }

    /// Runs `f` on `group`'s core with its state machine.
    pub async fn with_state_machine<F, R>(&self, group: GroupId, f: F) -> Result<R, RaftNodeError>
    where
        F: FnOnce(&mut M) -> R + Send + 'static,
        R: Send + 'static,
    {
        let core = self.route(group)?;
        self.shared
            .cores
            .submit_to(core, move |c| async move {
                let g = c.group(group).ok_or(RaftNodeError::NoSuchGroup(group))?;
                Ok(g.sm.with(f))
            })
            .await?
    }

    /// Stops serving, shuts every group down, closing logs cleanly, and
    /// stops the cores.
    pub async fn shutdown(&self) {
        self.shared.shutdown.send_replace(true);
        let servers = std::mem::take(&mut *self.shared.servers.lock().unwrap());
        for s in servers {
            let _ = s.await;
        }
        let _ = self
            .shared
            .cores
            .invoke_on_all(|c| async move { c.shutdown().await })
            .await;
        stop_cores(self.shared.cores.clone()).await;
        // Only now may another process open the directory.
        self.shared.lock.lock().unwrap().take();
    }

    fn route(&self, group: GroupId) -> Result<CoreId, RaftNodeError> {
        self.core_of(group).ok_or(RaftNodeError::NoSuchGroup(group))
    }
}

impl<M: Machine> Shared<M> {
    fn route(&self, group: GroupId) -> Option<CoreId> {
        self.routes.read().unwrap().get(&group).copied()
    }

    /// Serves one peer RPC, on the core that owns its group.
    async fn dispatch(&self, req: Request) -> Response {
        if self.faults.is_partitioned() {
            return Response::error(wire::status::UNAVAILABLE);
        }
        if req.version != wire::VERSION {
            return Response::error(wire::status::MALFORMED);
        }
        match req.method {
            wire::APPEND => {
                let Ok(r) = shep_codec::from_bytes::<AppendRequest>(&req.body) else {
                    return Response::error(wire::status::MALFORMED);
                };
                let reply = self
                    .on_group(r.group, move |c| async move { c.append(r).await })
                    .await;
                Response::ok(shep_codec::to_bytes(
                    &reply.unwrap_or_else(AppendReply::Rejected),
                ))
            }
            wire::HEARTBEATS => {
                let Ok(batch) = shep_codec::from_bytes::<wire::Heartbeats>(&req.body) else {
                    return Response::error(wire::status::MALFORMED);
                };
                Response::ok(shep_codec::to_bytes(&self.heartbeats(batch).await))
            }
            wire::VOTE | wire::PRE_VOTE => {
                let Ok(r) = shep_codec::from_bytes::<wire::VoteRequest>(&req.body) else {
                    return Response::error(wire::status::MALFORMED);
                };
                let pre = req.method == wire::PRE_VOTE;
                let reply = self
                    .on_group(r.group, move |c| async move { Ok(c.vote(r, pre).await) })
                    .await;
                Response::ok(shep_codec::to_bytes(
                    &reply.unwrap_or_else(wire::VoteReply::Rejected),
                ))
            }
            wire::SNAPSHOT => {
                let Ok(r) = shep_codec::from_bytes::<wire::SnapshotRequest>(&req.body) else {
                    return Response::error(wire::status::MALFORMED);
                };
                let reply = self
                    .on_group(r.group, move |c| async move { Ok(c.snapshot(r).await) })
                    .await;
                Response::ok(shep_codec::to_bytes(
                    &reply.unwrap_or_else(wire::SnapshotReply::Rejected),
                ))
            }
            wire::TRANSFER_LEADER => {
                let Ok(r) = shep_codec::from_bytes::<wire::TransferLeaderRequest>(&req.body) else {
                    return Response::error(wire::status::MALFORMED);
                };
                let reply = self
                    .on_group(
                        r.group,
                        move |c| async move { Ok(c.transfer_leader(r).await) },
                    )
                    .await;
                Response::ok(shep_codec::to_bytes(
                    &reply.unwrap_or_else(wire::TransferLeaderReply::Rejected),
                ))
            }
            _ => Response::error(wire::status::UNKNOWN_METHOD),
        }
    }

    /// Runs `f` on `group`'s core, or explains why it couldn't.
    async fn on_group<F, Fut, T>(&self, group: GroupId, f: F) -> Result<T, String>
    where
        F: FnOnce(Rc<Core<M>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, String>> + 'static,
        T: Send + 'static,
    {
        let core = self
            .route(group)
            .ok_or_else(|| format!("no group {group} on this node"))?;
        self.cores
            .submit_to(core, f)
            .await
            .map_err(|e| e.to_string())?
    }

    /// Splits a batch by core, one hop per core, and answers in order.
    async fn heartbeats(&self, batch: wire::Heartbeats) -> wire::HeartbeatReplies {
        let n = batch.items.len();
        let mut by_core: BTreeMap<CoreId, Vec<(usize, AppendRequest)>> = BTreeMap::new();
        let mut replies: Vec<Option<AppendReply>> = (0..n).map(|_| None).collect();
        for (i, item) in batch.items.into_iter().enumerate() {
            match self.route(item.group) {
                Some(core) => by_core.entry(core).or_default().push((i, item)),
                None => {
                    replies[i] = Some(AppendReply::Rejected(format!(
                        "no group {} on this node",
                        item.group
                    )))
                }
            }
        }
        let calls = by_core.into_iter().map(|(core, items)| {
            self.cores.submit_to(core, move |c| async move {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items {
                    out.push((i, c.heartbeat(item).await));
                }
                out
            })
        });
        for done in shep_futures::future::join_all(calls).await {
            match done {
                Ok(items) => {
                    for (i, reply) in items {
                        replies[i] = Some(reply);
                    }
                }
                Err(e) => {
                    for r in replies.iter_mut().filter(|r| r.is_none()) {
                        *r = Some(AppendReply::Rejected(e.to_string()));
                    }
                }
            }
        }
        wire::HeartbeatReplies {
            items: replies
                .into_iter()
                .map(|r| r.unwrap_or_else(|| AppendReply::Rejected("dropped".into())))
                .collect(),
        }
    }
}

/// One group, used on the core that hosts it: see [`RaftNode::with_group`].
/// Not `Send`.
pub struct GroupHandle<M: Machine> {
    g: Rc<Group<M>>,
    id: GroupId,
}

impl<M: Machine> Clone for GroupHandle<M> {
    fn clone(&self) -> Self {
        GroupHandle {
            g: self.g.clone(),
            id: self.id,
        }
    }
}

/// Who leads a group, as one replica sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leadership {
    pub term: u64,
    /// This replica leads.
    pub is_leader: bool,
    pub leader: Option<(NodeId, String)>,
}

/// An entry as the log stores it.
pub(crate) struct RawRecord {
    pub index: u64,
    pub term: u64,
    pub payload: RawPayload,
}

pub(crate) enum RawPayload {
    /// Raft's own entries.
    Internal,
    Normal(Vec<u8>),
}

/// Follows a group's leadership as it changes.
pub struct LeadershipWatch {
    metrics: shep_raft_engine::type_config::alias::WatchReceiverOf<
        TypeConfig,
        shep_raft_engine::RaftMetrics<TypeConfig>,
    >,
}

impl LeadershipWatch {
    pub fn current(&self) -> Leadership {
        leadership(&self.metrics.borrow_watched())
    }

    /// Waits for the group's metrics to change, which they do whenever its
    /// leadership does (and more often). False once the group has stopped.
    pub async fn changed(&mut self) -> bool {
        self.metrics.changed().await.is_ok()
    }
}

fn leadership(m: &shep_raft_engine::RaftMetrics<TypeConfig>) -> Leadership {
    let membership = m.membership_config.membership();
    Leadership {
        term: m.current_term,
        is_leader: m.state == ServerState::Leader,
        leader: m
            .current_leader
            .and_then(|id| membership.get_node(&id).map(|n| (id, n.addr.clone()))),
    }
}

impl<M: Machine> GroupHandle<M> {
    pub fn id(&self) -> GroupId {
        self.id
    }

    pub fn leadership(&self) -> Leadership {
        leadership(&self.g.raft.metrics().borrow_watched())
    }

    pub fn watch_leadership(&self) -> LeadershipWatch {
        LeadershipWatch {
            metrics: self.g.raft.metrics(),
        }
    }

    /// Proposes to the group, which this replica must lead, and waits for it
    /// to be decided. The leader admits proposals one at a time
    /// ([`Machine::admit`]); one whose `key` was decided within the
    /// idempotency window gets that decision instead.
    pub async fn propose(
        &self,
        key: Option<Vec<u8>>,
        proposal: M::Proposal,
    ) -> Result<Decided<M::Response>, ProposeError<M::Rejection>> {
        self.proposer().propose(key, proposal).await
    }

    fn proposer(&self) -> ProposerHandle<M> {
        let mut p = self.g.proposer.borrow_mut();
        p.get_or_insert_with(|| crate::proposer::spawn(self.clone(), self.g.config.clone()))
            .clone()
    }

    /// The decision for `key`, if an applied entry made one in the window.
    pub(crate) fn lookup(&self, key: &[u8]) -> Option<Decided<M::Response>> {
        self.g.sm.lookup(key)
    }

    /// The latest commit time of any applied entry.
    pub(crate) fn max_commit_time_us(&self) -> u64 {
        self.g.sm.max_commit_time_us()
    }

    /// Hands `payload` to Raft to append, if this replica leads. Payloads
    /// handed over in turn are appended in that order. The returned future
    /// resolves once it's committed and applied, with the state machine's
    /// response, or with why it wasn't.
    pub(crate) async fn append(
        &self,
        payload: Vec<u8>,
    ) -> Result<impl std::future::Future<Output = Result<Written, WriteError>> + 'static, WriteError>
    {
        use shep_raft_engine::impls::ProgressResponder;
        let (responder, _committed, rx) = ProgressResponder::new();
        self.g
            .raft
            .client_write_ff(crate::types::Payload(payload), Some(responder))
            .await
            .map_err(|e| WriteError::Other(e.to_string()))?;
        Ok(async move {
            match rx.await {
                Ok(Ok(resp)) => Ok(Written {
                    term: resp.log_id.leader_id.term,
                    index: resp.log_id.index,
                    response: resp.data.0,
                }),
                Ok(Err(ClientWriteError::ForwardToLeader(f))) => Err(WriteError::NotLeader {
                    leader: f.leader_id.zip(f.leader_node).map(|(id, n)| (id, n.addr)),
                }),
                Ok(Err(ClientWriteError::LogEntryDiscarded(f))) => Err(WriteError::Discarded {
                    leader: f.leader_id.zip(f.leader_node).map(|(id, n)| (id, n.addr)),
                }),
                Ok(Err(e)) => Err(WriteError::Other(e.to_string())),
                Err(_) => Err(WriteError::Other("raft dropped the write".into())),
            }
        })
    }

    /// The indexes the log holds: `start..next`. Not all are committed.
    pub fn log_range(&self) -> Range<u64> {
        self.g.log.start_index()..self.g.log.next_index()
    }

    /// Entries in `range` as stored, up to about `max_bytes` (at least one,
    /// if any).
    pub(crate) async fn read_raw(
        &self,
        range: Range<u64>,
        max_bytes: usize,
    ) -> Result<Vec<RawRecord>, RaftNodeError> {
        let records = self.g.log.read(range, max_bytes).await?;
        records
            .into_iter()
            .map(|r| {
                let e = crate::types::record_to_entry(r)?;
                let payload = match e.payload {
                    shep_raft_engine::EntryPayload::Normal(d) => RawPayload::Normal(d.0),
                    _ => RawPayload::Internal,
                };
                Ok(RawRecord {
                    index: e.log_id.index,
                    term: e.log_id.leader_id.term,
                    payload,
                })
            })
            .collect()
    }

    /// Entries in `range` that the log holds, up to about `max_bytes` (at
    /// least one, if any). Entries past the commit index may yet be
    /// truncated.
    pub async fn entries(
        &self,
        range: Range<u64>,
        max_bytes: usize,
    ) -> Result<Vec<LogRecord<M::Command>>, RaftNodeError> {
        let records = self.read_raw(range, max_bytes).await?;
        Ok(records.into_iter().map(decode_record::<M>).collect())
    }

    /// Committed entries from `from` on, up to about `max_bytes` (at least
    /// one, if any), waiting up to `wait` for there to be any.
    pub async fn fetch(&self, from: u64, max_bytes: usize, wait: Duration) -> Fetched<M::Command> {
        let start = self.log_range().start;
        if from < start {
            return Fetched::OutOfRange { start };
        }
        let mut applied = self.subscribe_applied();
        let _ =
            tokio::time::timeout(wait, applied.wait_for(|a| a.is_some_and(|a| a >= from))).await;
        let applied_index = *applied.borrow();
        let Some(last) = applied_index.filter(|&a| a >= from) else {
            return Fetched::Records {
                records: Vec::new(),
                applied_index,
            };
        };
        match self.entries(from..last + 1, max_bytes).await {
            Ok(records) if records.first().is_none_or(|r| r.index == from) => Fetched::Records {
                records,
                applied_index,
            },
            Ok(_) | Err(_) => Fetched::OutOfRange {
                start: self.log_range().start,
            },
        }
    }

    /// The last index applied to the state machine, which is committed.
    pub fn applied_index(&self) -> Option<u64> {
        *self.g.sm.subscribe_applied().borrow()
    }

    pub fn subscribe_applied(&self) -> watch::Receiver<Option<u64>> {
        self.g.sm.subscribe_applied()
    }

    /// Runs `f` with the state machine. It mustn't be held across an await.
    pub fn with_state_machine<R>(&self, f: impl FnOnce(&mut M) -> R) -> R {
        self.g.sm.with(f)
    }
}

/// Locks the data directory for this process's lifetime (released at
/// shutdown or exit). While one process holds it, no other can start on the
/// directory, so a running node's predecessor is gone: the auth horizon's
/// single-replica restart relies on that (AH13), and so does storage. The
/// lock is only as good as the filesystem's: on network filesystems like NFS
/// it may not exclude other hosts, so the directory must be on local disk.
fn lock_data_dir(dir: &std::path::Path) -> Result<std::fs::File, RaftNodeError> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("LOCK");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(RaftNodeError::DataDirLocked(path)),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Stops `cores` and waits for their threads to exit. Stopping blocks, so it
/// runs on a thread of its own rather than the caller's runtime.
async fn stop_cores<S: 'static>(cores: Smp<S>) {
    let (done, stopped) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        cores.shutdown();
        let _ = done.send(());
    });
    let _ = stopped.await;
}

/// The routing table from what each core hosts, after creating each
/// required group no core hosts, on its core. Only once every core has
/// reported is it known that no other core has a group.
fn create_required<M: Machine>(
    cores: &Smp<Core<M>>,
    hosted: Vec<(CoreId, Vec<GroupId>)>,
    required: &[(GroupId, CoreId)],
) -> Result<HashMap<GroupId, CoreId>, RaftNodeError> {
    let mut routes = HashMap::new();
    for (core, groups) in hosted {
        for group in groups {
            if let Some(other) = routes.insert(group, core) {
                return Err(RaftNodeError::Raft(format!(
                    "group {group} is on both {other} and {core}"
                )));
            }
        }
    }
    for &(group, core) in required {
        match routes.get(&group) {
            Some(&on) if on == core => {}
            Some(&on) => {
                return Err(RaftNodeError::Raft(format!(
                    "group {group} is required on {core} but is on {on}"
                )));
            }
            None => {
                // The cores run it; this thread only waits.
                shep_futures::future::block_on(cores.submit_to(core, move |c| async move {
                    c.create(group).await.map_err(|e| e.to_string())
                }))?
                .map_err(RaftNodeError::Raft)?;
                routes.insert(group, core);
            }
        }
    }
    Ok(routes)
}

/// Stops the cores and waits, for `start`, which blocks anyway. The stop runs
/// on a thread of its own: the caller may be running a runtime, and
/// `Smp::shutdown` won't block one.
fn stop_cores_blocking<S: 'static>(cores: Smp<S>) {
    std::thread::scope(|scope| {
        scope.spawn(|| cores.shutdown());
    });
}

/// Refuses to start with fewer cores than host groups: a core's groups are
/// in its own directory, and no core would open them.
fn check_removed_cores(config: &RaftConfig, cores: usize) -> Result<(), RaftNodeError> {
    let cores_dir = config.data_dir.join("cores");
    let entries = match std::fs::read_dir(&cores_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut removed = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        if let Some(core) = name.to_str().and_then(|n| n.parse::<usize>().ok())
            && core >= cores
        {
            removed.push(core);
        }
    }
    if removed.is_empty() {
        return Ok(());
    }
    removed.sort();
    // Opening storage is async. The cores aren't running yet, and the caller
    // may be running a runtime of its own, so it gets a thread and runtime
    // to itself.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build_local(Default::default())?;
                rt.block_on(async {
                    for core in removed {
                        let dir = cores_dir.join(core.to_string());
                        let storage = Storage::open(dir, config.storage.clone()).await?;
                        let groups = storage.kvstore().scan(KeySpace::SHARD_PLACEMENT).len();
                        if groups > 0 {
                            return Err(RaftNodeError::CoreRemoved {
                                core,
                                groups,
                                cores,
                            });
                        }
                    }
                    Ok(())
                })
            })
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// One core's groups.
struct Core<M: Machine> {
    id: CoreId,
    config: Arc<RaftConfig>,
    storage: Storage,
    peers: Rc<Peers>,
    make_sm: Arc<MakeSm<M>>,
    groups: Rc<RefCell<HashMap<GroupId, Rc<Group<M>>>>>,
}

struct Group<M: Machine> {
    config: Arc<RaftConfig>,
    raft: Raft<TypeConfig, SmAdapter<M>>,
    sm: SmAdapter<M>,
    log: Log,
    inbound: RefCell<Inbound>,
    /// Started by the first proposal.
    proposer: RefCell<Option<ProposerHandle<M>>>,
}

/// A stored record as readers see it: undecodable entries count as void,
/// as they do when applied.
fn decode_record<M: Machine>(r: RawRecord) -> LogRecord<M::Command> {
    let kind = match r.payload {
        RawPayload::Internal => RecordKind::Internal,
        RawPayload::Normal(b) => match shep_codec::from_bytes::<Envelope<M::Command>>(&b) {
            Ok(env) if !env.is_void(r.term) => RecordKind::Entry(env.into_entry(r.index, r.term)),
            Ok(_) | Err(_) => RecordKind::Void,
        },
    };
    LogRecord {
        index: r.index,
        term: r.term,
        kind,
    }
}

/// A follower's view of the leader's current replication session: requests
/// run in `seq` order however they arrive.
#[derive(Default)]
struct Inbound {
    session: u64,
    next_seq: u64,
    waiting: BTreeMap<u64, (AppendRequest, oneshot::Sender<AppendReply>)>,
}

impl<M: Machine> Core<M> {
    async fn open(
        id: CoreId,
        config: Arc<RaftConfig>,
        make_sm: Arc<MakeSm<M>>,
        stats: Arc<NetStats>,
        faults: Arc<Faults>,
    ) -> Result<Core<M>, RaftNodeError> {
        let dir = config.data_dir.join("cores").join(id.0.to_string());
        let storage = Storage::open(dir, config.storage.clone()).await?;
        let peers = Rc::new(Peers::new(
            NetConfig {
                append_window: config.append_window,
                heartbeat_linger: config.heartbeat_linger,
                rpc_timeout: config.rpc_timeout,
                reconnect: config.reconnect.clone(),
            },
            stats,
            faults,
        ));
        let core = Core {
            id,
            config: config.clone(),
            storage,
            peers,
            make_sm,
            groups: Rc::new(RefCell::new(HashMap::new())),
        };
        for (key, _) in core.storage().kvstore().scan(KeySpace::SHARD_PLACEMENT) {
            if let Some(group) = std::str::from_utf8(&key)
                .ok()
                .and_then(|k| k.parse::<u64>().ok())
            {
                core.open_group(GroupId(group)).await?;
            }
        }
        core.start_heartbeats();
        Ok(core)
    }

    fn storage(&self) -> &Storage {
        &self.storage
    }

    fn group_ids(&self) -> Vec<GroupId> {
        self.groups.borrow().keys().copied().collect()
    }

    fn group(&self, group: GroupId) -> Option<Rc<Group<M>>> {
        self.groups.borrow().get(&group).cloned()
    }

    /// Triggers every leader's heartbeat together each interval, so they
    /// batch per peer (Redpanda's per-shard heartbeat timer).
    fn start_heartbeats(&self) {
        let interval = self.config.heartbeat_interval;
        // Weak, so the ticker ends with the core.
        let groups = Rc::downgrade(&self.groups);
        tokio::task::spawn_local(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(groups) = groups.upgrade() else {
                    return;
                };
                let leaders: Vec<_> = groups
                    .borrow()
                    .values()
                    .filter(|g| g.raft.is_leader())
                    .cloned()
                    .collect();
                drop(groups);
                for g in leaders {
                    let _ = g.raft.trigger().heartbeat().await;
                }
            }
        });
    }

    async fn create(&self, group: GroupId) -> Result<(), RaftNodeError> {
        if self.groups.borrow().contains_key(&group) {
            return Ok(());
        }
        // Recorded first, so a restart reopens it here.
        let key = group.0.to_string();
        self.storage()
            .kvstore()
            .put(KeySpace::SHARD_PLACEMENT, key.as_bytes(), Vec::<u8>::new())
            .await?;
        self.open_group(group).await
    }

    async fn open_group(&self, group: GroupId) -> Result<(), RaftNodeError> {
        let storage = self.storage();
        let dir = self.config.data_dir.join("logs").join(group.0.to_string());
        let log = Log::open(
            storage.disk(),
            storage.pool(),
            storage.kvstore(),
            dir,
            group.0,
            self.config.storage.log.clone(),
        )
        .await?;
        let group_log = GroupLog::new(
            group,
            log.clone(),
            storage.kvstore().clone(),
            self.config.max_append_bytes,
        );
        let sm = SmAdapter::new((self.make_sm)(group));
        let network = NetworkFactory {
            group,
            peers: self.peers.clone(),
        };
        let raft = Raft::new(
            self.config.node_id,
            self.config.engine_config()?,
            network,
            group_log,
            sm.clone(),
        )
        .await
        .map_err(|e| RaftNodeError::Raft(e.to_string()))?;
        let g = Rc::new(Group {
            config: self.config.clone(),
            raft,
            sm,
            log,
            inbound: RefCell::new(Inbound::default()),
            proposer: RefCell::new(None),
        });
        watch_leadership(&g);
        self.groups.borrow_mut().insert(group, g);
        Ok(())
    }

    async fn shutdown(&self) {
        let groups: Vec<_> = self.groups.borrow_mut().drain().map(|(_, g)| g).collect();
        for g in groups {
            // Its task holds the group; without senders, it ends.
            g.proposer.borrow_mut().take();
            let _ = g.raft.shutdown().await;
            if let Err(e) = g.log.close().await {
                tracing::warn!(core = %self.id, "closing a log: {e}");
            }
        }
    }

    /// One request of a replication stream: run in sequence within its
    /// session, newer sessions superseding older ones.
    async fn append(self: Rc<Self>, req: AppendRequest) -> Result<AppendReply, String> {
        let g = self
            .group(req.group)
            .ok_or_else(|| format!("no group {}", req.group))?;
        let (tx, rx) = oneshot::channel();
        {
            let mut inbound = g.inbound.borrow_mut();
            if req.session < inbound.session {
                return Ok(AppendReply::Rejected(
                    "superseded replication session".into(),
                ));
            }
            if req.session > inbound.session {
                for (_, (_, tx)) in std::mem::take(&mut inbound.waiting) {
                    let _ = tx.send(AppendReply::Rejected(
                        "superseded replication session".into(),
                    ));
                }
                inbound.session = req.session;
                inbound.next_seq = 0;
            }
            if req.seq < inbound.next_seq {
                return Ok(AppendReply::Rejected(format!(
                    "duplicate request {}",
                    req.seq
                )));
            }
            let (session, seq) = (req.session, req.seq);
            inbound.waiting.insert(seq, (req, tx));
            // Hand the engine every request that's now next, in order: tasks
            // spawned on a core start in spawn order.
            loop {
                let next = inbound.next_seq;
                let Some((req, tx)) = inbound.waiting.remove(&next) else {
                    break;
                };
                inbound.next_seq += 1;
                let g = g.clone();
                tokio::task::spawn_local(async move {
                    let _ = tx.send(run_append(&g, req).await);
                });
            }
            // Still waiting for an earlier request, which may never come: a
            // restarted follower lost the session's start, or a request was
            // lost. Rejecting it ends the leader's stream, and its next
            // session starts from 0.
            if inbound.waiting.contains_key(&seq) {
                let g = g.clone();
                let wait = self.config.append_gap_timeout;
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(wait).await;
                    let mut inbound = g.inbound.borrow_mut();
                    if inbound.session == session
                        && let Some((_, tx)) = inbound.waiting.remove(&seq)
                    {
                        let why = format!(
                            "request {seq} waited {wait:?} for request {}",
                            inbound.next_seq
                        );
                        let _ = tx.send(AppendReply::Rejected(why));
                    }
                });
            }
        }
        rx.await.map_err(|_| "stopped".to_string())
    }

    async fn heartbeat(&self, req: AppendRequest) -> AppendReply {
        match self.group(req.group) {
            Some(g) => run_append(&g, req).await,
            None => AppendReply::Rejected(format!("no group {}", req.group)),
        }
    }

    async fn vote(&self, req: wire::VoteRequest, pre: bool) -> wire::VoteReply {
        let Some(g) = self.group(req.group) else {
            return wire::VoteReply::Rejected(format!("no group {}", req.group));
        };
        let rpc = VoteRequest {
            vote: Vote::from(req.vote),
            last_log_id: from_opt_log_id(req.last_log_id),
            leadership_transfer: req.leadership_transfer,
        };
        let resp = if pre {
            g.raft.pre_vote(rpc).await
        } else {
            g.raft.vote(rpc).await
        };
        match resp {
            Ok(r) => wire::VoteReply::Ok {
                vote: (&r.vote).into(),
                vote_granted: r.vote_granted,
                last_log_id: opt_log_id(&r.last_log_id),
            },
            Err(e) => wire::VoteReply::Rejected(e.to_string()),
        }
    }

    async fn snapshot(&self, req: wire::SnapshotRequest) -> wire::SnapshotReply {
        let Some(g) = self.group(req.group) else {
            return wire::SnapshotReply::Rejected(format!("no group {}", req.group));
        };
        let meta = match req.meta.into_meta() {
            Ok(m) => m,
            Err(e) => return wire::SnapshotReply::Rejected(e.to_string()),
        };
        let snapshot = RaftSnapshot {
            meta,
            snapshot: req.data,
        };
        match g
            .raft
            .install_full_snapshot(Vote::from(req.vote), snapshot)
            .await
        {
            Ok(r) => wire::SnapshotReply::Ok((&r.vote).into()),
            Err(e) => wire::SnapshotReply::Rejected(e.to_string()),
        }
    }

    async fn transfer_leader(&self, req: wire::TransferLeaderRequest) -> wire::TransferLeaderReply {
        let Some(g) = self.group(req.group) else {
            return wire::TransferLeaderReply::Rejected(format!("no group {}", req.group));
        };
        let last: Option<LogId> = from_opt_log_id(req.last_log_id);
        let rpc = TransferLeaderRequest::new(Vote::from(req.from_leader), req.to, last);
        match g.raft.handle_transfer_leader(rpc).await {
            Ok(Ok(())) => wire::TransferLeaderReply::Ok,
            Ok(Err(shep_raft_engine::raft::TransferLeaderError::VoteChanged {
                expected,
                actual,
            })) => wire::TransferLeaderReply::VoteChanged {
                expected: (&expected).into(),
                actual: (&actual).into(),
            },
            Ok(Err(shep_raft_engine::raft::TransferLeaderError::LogNotFlushed {
                expected,
                actual,
            })) => wire::TransferLeaderReply::LogNotFlushed {
                expected: opt_log_id(&expected),
                actual: opt_log_id(&actual),
            },
            Err(e) => wire::TransferLeaderReply::Rejected(e.to_string()),
        }
    }
}

/// Heartbeats as soon as the group becomes leader, so its lease, which
/// gates writes, is established without waiting for the core's next tick.
fn watch_leadership<M: Machine>(g: &Rc<Group<M>>) {
    let mut metrics = g.raft.metrics();
    let raft = g.raft.clone();
    tokio::task::spawn_local(async move {
        let mut was_leader = false;
        loop {
            let is_leader = metrics.borrow_watched().state == ServerState::Leader;
            if is_leader && !was_leader {
                let _ = raft.trigger().heartbeat().await;
            }
            was_leader = is_leader;
            if metrics.changed().await.is_err() {
                return;
            }
        }
    });
}

async fn run_append<M: Machine>(g: &Group<M>, req: AppendRequest) -> AppendReply {
    let entries = match req
        .entries
        .into_iter()
        .map(WireEntry::into_entry)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(e) => e,
        Err(e) => return AppendReply::Rejected(e.to_string()),
    };
    let rpc = AppendEntriesRequest {
        vote: Vote::from(req.vote),
        prev_log_id: from_opt_log_id(req.prev_log_id),
        entries,
        leader_commit: from_opt_log_id(req.leader_commit),
    };
    match g.raft.append_entries(rpc).await {
        Ok(resp) => AppendReply::from(&resp),
        Err(e) => AppendReply::Rejected(e.to_string()),
    }
}
