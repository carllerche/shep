//! The write-ahead log (design §3, §10): one Raft group per partition, on
//! shep-raft, with the leader validating every proposal before appending it.
//! Shep is an embedder of shep-raft like any other: its partitions' machine
//! ([`PartitionSm`]) validates proposals in `admit`, and shep-raft does the
//! rest (void entries, rebuilding the table on a new leader, idempotency).
//!
//! A [`WalNode`] hosts replicas of any number of partitions on its cores and
//! serves two services on one shep-rpc address: Raft between replicas, and the
//! WAL's client protocol (`shep-wal-api`). Clients use `shep-wal-client`.
//!
//! What the WAL guarantees (§10.3): committed mutations are serializable in
//! each partition's log order. A proposal commits only if no entry its read
//! version doesn't contain wrote a key in its read set or a range it read,
//! including entries appended but not yet committed, and including entries a
//! previous leader appended. Each idempotency key is decided at most once.

mod command;
mod controller;
mod state_machine;
#[doc(hidden)]
pub mod table;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shep_raft::{
    Entry, Fetched, GroupId, LogRecord, ProposeError, RaftConfig, RaftNode, RaftNodeError,
    RecordKind,
};
use shep_rpc::{Request, Response, Server, Status};
use shep_smp::{Smp, SmpError};
use shep_wal_api::{
    AdminReply, CreatePartitionRequest, Decided, DecidedStatus, Dot, EntryKind, FetchReply,
    FetchRequest, InitializePartitionRequest, LogEntry, Member, MutationEntry, PartitionId,
    PartitionStatus, ProposalKind, ProposeReply, ProposeRequest, Rejection, StatusReply,
    StatusRequest, method,
};
use tokio::net::TcpListener;
use tokio::sync::watch;

pub use command::PartitionCommand;
pub use shep_raft::Faults;
pub use shep_wal_controller as controller_types;
pub use state_machine::{
    GroupCommand, GroupProposal, GroupRejection, GroupResponse, GroupSm, MAX_DATA_PARTITIONS,
    PartitionSm, universe_rules,
};

/// A WAL node's configuration. How proposals are handled is
/// `raft.proposals`: its admission window is how long write sets stay in the
/// leader's validation table (a proposal whose read version is older gets
/// `TooOld`), and how many it holds however recent.
#[derive(Clone, Debug)]
pub struct WalConfig {
    pub raft: RaftConfig,
    /// The founding members of the cluster, this node among them. A node
    /// with seeds hosts a replica of the WAL controller (§9.6); with none,
    /// only partitions created by hand. One seed, itself, makes a cluster
    /// of one.
    pub seeds: Vec<Member>,
    /// The longest a fetch waits for new entries, whatever it asks for.
    pub max_fetch_wait: Duration,
}

impl WalConfig {
    pub fn new(node_id: u64, data_dir: impl Into<std::path::PathBuf>) -> WalConfig {
        WalConfig {
            raft: RaftConfig::new(node_id, data_dir),
            seeds: Vec::new(),
            max_fetch_wait: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error(transparent)]
    Raft(#[from] RaftNodeError),
    #[error(transparent)]
    Smp(#[from] SmpError),
    #[error("partition {0} isn't hosted here")]
    UnknownPartition(PartitionId),
    /// The controller places this node's partitions; one created by hand
    /// could take an id the controller later allocates to a universe.
    #[error("partitions on this node are placed by the controller")]
    ControllerManaged,
}

/// A WAL node: replicas of some partitions on its cores, and the services for
/// them. Cheap to clone; usable from any thread.
#[derive(Clone)]
pub struct WalNode {
    shared: Arc<Shared>,
}

struct Shared {
    config: Arc<WalConfig>,
    raft: RaftNode<GroupSm>,
    shutdown: watch::Sender<bool>,
    servers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The controller's founding and reconciling.
    background: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// For calls to peers outside Raft, like founding's hellos.
    peers: shep_rpc::ConnectionCache,
}

fn group(p: PartitionId) -> GroupId {
    GroupId(p.0)
}

impl WalNode {
    /// Opens the node's storage on `smp`'s cores and reopens the partitions
    /// it hosted.
    pub async fn start(config: WalConfig, smp: Smp) -> Result<WalNode, WalError> {
        let raft = RaftNode::start(config.raft.clone(), smp, GroupSm::for_group).await?;
        let node = WalNode {
            shared: Arc::new(Shared {
                peers: shep_rpc::ConnectionCache::new(config.raft.reconnect.clone()),
                config: Arc::new(config),
                raft,
                shutdown: watch::channel(false).0,
                servers: Mutex::new(Vec::new()),
                background: Mutex::new(Vec::new()),
            }),
        };
        if !node.config().seeds.is_empty() {
            node.start_controller().await?;
        }
        Ok(node)
    }

    pub fn config(&self) -> &WalConfig {
        &self.shared.config
    }

    pub(crate) fn peers(&self) -> &shep_rpc::ConnectionCache {
        &self.shared.peers
    }

    /// Runs `f` until shutdown.
    pub(crate) fn spawn_background(
        &self,
        f: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let mut shutdown = self.shared.shutdown.subscribe();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = f => {}
                _ = async { let _ = shutdown.wait_for(|s| *s).await; } => {}
            }
        });
        self.shared.background.lock().unwrap().push(task);
    }

    pub fn node_id(&self) -> u64 {
        self.shared.raft.node_id()
    }

    pub fn raft(&self) -> &RaftNode<GroupSm> {
        &self.shared.raft
    }

    pub fn faults(&self) -> &Faults {
        self.shared.raft.faults()
    }

    /// Serves the Raft and WAL services on `listener` until
    /// [`WalNode::shutdown`].
    pub fn serve(&self, listener: TcpListener) {
        let node = self.clone();
        let server = Server::new(
            self.shared.config.raft.server.clone(),
            move |req: Request| {
                let node = node.clone();
                async move { node.handle(req).await }
            },
        );
        let mut shutdown = self.shared.shutdown.subscribe();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = server.serve(listener) => {}
                _ = async { let _ = shutdown.wait_for(|s| *s).await; } => {
                    server.shutdown(Duration::from_millis(100)).await
                }
            }
        });
        self.shared.servers.lock().unwrap().push(task);
    }

    /// Serves one request of either service; for a server hosting more.
    pub async fn handle(&self, req: Request) -> Response {
        if RaftNode::<GroupSm>::handles(req.method) {
            return self.shared.raft.handle(req).await;
        }
        if shep_wal_controller::method::is_controller(req.method) {
            return self.handle_controller(req).await;
        }
        fn reply<T: shep_codec::Encode>(t: &T) -> Response {
            Response::ok(shep_codec::to_bytes(t))
        }
        match req.method {
            method::PROPOSE => match shep_codec::from_bytes::<ProposeRequest>(req.body) {
                Ok(r) => reply(&ProposeReply::from(
                    self.propose(r.partition, r.proposal).await,
                )),
                Err(_) => Response::error(Status::ServerError),
            },
            method::FETCH => match shep_codec::from_bytes::<FetchRequest>(req.body) {
                Ok(r) => reply(&self.fetch(r).await),
                Err(_) => Response::error(Status::ServerError),
            },
            method::STATUS => match shep_codec::from_bytes::<StatusRequest>(req.body) {
                Ok(r) => reply(&match self.status(r.partition).await {
                    Some(s) => StatusReply::Status(s),
                    None => StatusReply::UnknownPartition,
                }),
                Err(_) => Response::error(Status::ServerError),
            },
            method::CREATE_PARTITION => {
                match shep_codec::from_bytes::<CreatePartitionRequest>(req.body) {
                    Ok(r) => reply(&match self.create_partition(r.partition).await {
                        Ok(()) => AdminReply::Ok,
                        Err(e) => AdminReply::Failed(e.to_string()),
                    }),
                    Err(_) => Response::error(Status::ServerError),
                }
            }
            method::INITIALIZE_PARTITION => {
                match shep_codec::from_bytes::<InitializePartitionRequest>(req.body) {
                    Ok(r) => reply(
                        &match self.initialize_partition(r.partition, &r.members).await {
                            Ok(()) => AdminReply::Ok,
                            Err(WalError::Raft(RaftNodeError::AlreadyInitialized(_))) => {
                                AdminReply::AlreadyInitialized
                            }
                            Err(
                                WalError::Raft(RaftNodeError::NoSuchGroup(_))
                                | WalError::UnknownPartition(_),
                            ) => AdminReply::UnknownPartition,
                            Err(e) => AdminReply::Failed(e.to_string()),
                        },
                    ),
                    Err(_) => Response::error(Status::ServerError),
                }
            }
            _ => Response::error(Status::MethodNotFound),
        }
    }

    async fn handle_controller(&self, req: Request) -> Response {
        use shep_wal_controller::{
            CreateLogSetRequest, HelloRequest, StateReply, StateRequest, method as cm,
        };
        fn reply<T: shep_codec::Encode>(t: &T) -> Response {
            Response::ok(shep_codec::to_bytes(t))
        }
        match req.method {
            cm::STATE => match shep_codec::from_bytes::<StateRequest>(req.body) {
                Ok(_) => reply(&match self.controller_state().await {
                    Some(s) => StateReply::State(s),
                    None => StateReply::NoController,
                }),
                Err(_) => Response::error(Status::ServerError),
            },
            cm::CREATE_LOG_SET => match shep_codec::from_bytes::<CreateLogSetRequest>(req.body) {
                Ok(r) => reply(&self.create_log_set(r).await),
                Err(_) => Response::error(Status::ServerError),
            },
            cm::HELLO => match shep_codec::from_bytes::<HelloRequest>(req.body) {
                Ok(_) => reply(&self.hello().await),
                Err(_) => Response::error(Status::ServerError),
            },
            _ => Response::error(Status::MethodNotFound),
        }
    }

    /// Creates this node's replica of `partition`, empty, or does nothing if
    /// it has one. Only on a node without seeds: elsewhere the controller
    /// allocates partitions and places replicas.
    pub async fn create_partition(&self, partition: PartitionId) -> Result<(), WalError> {
        self.unmanaged()?;
        self.create_replica(partition).await
    }

    /// Initializes `partition`'s group with its first members. Call it on one
    /// member, after every member created its replica. Only on a node
    /// without seeds, like [`WalNode::create_partition`].
    pub async fn initialize_partition(
        &self,
        partition: PartitionId,
        members: &[Member],
    ) -> Result<(), WalError> {
        self.unmanaged()?;
        self.initialize_group(partition, members).await
    }

    fn unmanaged(&self) -> Result<(), WalError> {
        if self.config().seeds.is_empty() {
            Ok(())
        } else {
            Err(WalError::ControllerManaged)
        }
    }

    pub(crate) async fn create_replica(&self, partition: PartitionId) -> Result<(), WalError> {
        self.shared.raft.create_group(group(partition)).await?;
        Ok(())
    }

    pub(crate) async fn initialize_group(
        &self,
        partition: PartitionId,
        members: &[Member],
    ) -> Result<(), WalError> {
        let members: BTreeMap<u64, String> = members
            .iter()
            .map(|m| (m.node_id, m.addr.clone()))
            .collect();
        self.shared
            .raft
            .initialize_group(group(partition), members)
            .await?;
        Ok(())
    }

    /// Validates `proposal` and, if it passes, commits it (§10.3). Only the
    /// partition's leader accepts.
    pub async fn propose(
        &self,
        partition: PartitionId,
        proposal: shep_wal_api::Proposal,
    ) -> Result<Decided, Rejection> {
        let key = proposal.idempotency_key;
        if key.is_empty() {
            return Err(Rejection::Invalid("empty idempotency key".into()));
        }
        let reply = match &proposal.kind {
            ProposalKind::Mutation(m) => m.reply.clone(),
            ProposalKind::Failure { reply } => reply.clone(),
        };
        let proposal = GroupProposal::Partition(proposal.kind);
        let decided = match self
            .shared
            .raft
            .propose(group(partition), Some(key), proposal)
            .await
        {
            Ok(d) => d,
            Err(e) => return Err(rejection(e)),
        };
        let GroupResponse::Partition(failed) = decided.response else {
            return Err(Rejection::UnknownPartition);
        };
        // A retry gets the reply of the proposal that decided its key.
        let reply = if decided.replayed {
            self.reply_at(partition, decided.index).await?
        } else {
            reply
        };
        Ok(Decided {
            dot: Dot::new(partition, decided.index),
            commit_time_us: decided.commit_time_us,
            status: if failed {
                DecidedStatus::Failed
            } else {
                DecidedStatus::Committed
            },
            reply,
        })
    }

    /// The reply recorded in the entry at `index`.
    async fn reply_at(
        &self,
        partition: PartitionId,
        index: u64,
    ) -> Result<bytes::Bytes, Rejection> {
        let records = self
            .shared
            .raft
            .with_group(group(partition), move |g| async move {
                g.entries(index..index + 1, usize::MAX).await
            })
            .await
            .map_err(|_| Rejection::UnknownPartition)?
            .map_err(|e| Rejection::UnknownOutcome(format!("reading entry {index}: {e}")))?;
        match records.into_iter().next().map(|r| r.kind) {
            Some(RecordKind::Entry(e)) => match e.command {
                GroupCommand::Partition(c) => Ok(c.reply().clone()),
                GroupCommand::Controller(_) => Err(Rejection::UnknownPartition),
            },
            _ => Err(Rejection::UnknownOutcome(format!(
                "entry {index} is no longer in the log"
            ))),
        }
    }

    /// Committed entries from `from_index` on, waiting up to the request's
    /// limit for there to be any.
    pub async fn fetch(&self, req: FetchRequest) -> FetchReply {
        let partition = req.partition;
        let wait =
            Duration::from_millis(req.max_wait_ms.into()).min(self.shared.config.max_fetch_wait);
        let (from, max_bytes) = (req.from_index, req.max_bytes as usize);
        let r = self
            .shared
            .raft
            .with_group(group(partition), move |g| async move {
                g.fetch(from, max_bytes, wait).await
            })
            .await;
        match r {
            Ok(Fetched::Records {
                records,
                applied_index,
            }) => FetchReply::Entries {
                entries: records
                    .into_iter()
                    .map(|r| log_entry(partition, r))
                    .collect(),
                applied_index,
            },
            Ok(Fetched::OutOfRange { start }) => FetchReply::OutOfRange { start },
            Err(_) => FetchReply::UnknownPartition,
        }
    }

    pub async fn status(&self, partition: PartitionId) -> Option<PartitionStatus> {
        let node_id = self.node_id();
        let status = self.shared.raft.status(group(partition)).await.ok()?;
        let r = self
            .shared
            .raft
            .with_group(group(partition), move |g| async move {
                let lead = g.leadership();
                PartitionStatus {
                    node_id,
                    term: lead.term,
                    leader: lead.leader.map(|(node_id, addr)| Member { node_id, addr }),
                    last_log_index: g.log_range().end.saturating_sub(1),
                    applied_index: g.applied_index(),
                    voters: status.voters.into_iter().collect(),
                }
            })
            .await;
        r.ok()
    }

    /// Stops serving and shuts every partition down. The `Smp` keeps running;
    /// it's the caller's.
    pub async fn shutdown(&self) {
        self.shared.shutdown.send_replace(true);
        let tasks: Vec<_> = std::mem::take(&mut *self.shared.servers.lock().unwrap())
            .into_iter()
            .chain(std::mem::take(&mut *self.shared.background.lock().unwrap()))
            .collect();
        for t in tasks {
            let _ = t.await;
        }
        self.shared.raft.shutdown().await;
    }
}

fn rejection(e: ProposeError<GroupRejection>) -> Rejection {
    match e {
        ProposeError::Rejected(GroupRejection::Partition(r)) => r,
        ProposeError::Rejected(r) => Rejection::Invalid(format!("{r:?}")),
        ProposeError::NotLeader { leader } => Rejection::NotLeader {
            leader: leader.map(|(node_id, addr)| Member { node_id, addr }),
        },
        e @ ProposeError::TooLarge { .. } => Rejection::Invalid(e.to_string()),
        ProposeError::Unknown(m) => Rejection::UnknownOutcome(m),
        ProposeError::NoSuchGroup(_) => Rejection::UnknownPartition,
    }
}

/// A log record as readers of the log see it.
fn log_entry(partition: PartitionId, r: LogRecord<GroupCommand>) -> LogEntry {
    let kind = match r.kind {
        RecordKind::Internal
        | RecordKind::Entry(Entry {
            command: GroupCommand::Controller(_),
            ..
        }) => EntryKind::Internal,
        RecordKind::Void => EntryKind::Void,
        RecordKind::Entry(Entry {
            commit_time_us,
            key,
            command: GroupCommand::Partition(command),
            ..
        }) => {
            let idempotency_key = key.unwrap_or_default();
            match command {
                PartitionCommand::Mutation {
                    context, payload, ..
                } => EntryKind::Mutation(MutationEntry {
                    context,
                    commit_time_us,
                    idempotency_key,
                    payload,
                }),
                PartitionCommand::Failure { .. } => EntryKind::Failure {
                    commit_time_us,
                    idempotency_key,
                },
            }
        }
    };
    LogEntry {
        dot: Dot::new(partition, r.index),
        term: r.term,
        kind,
    }
}
