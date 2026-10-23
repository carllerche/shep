//! A client for the WAL's protocol, on shep-rpc.
//!
//! [`NodeClient`] talks to one node. [`WalClient`] knows a set of nodes and
//! routes each proposal to its partition's leader, re-sending the same
//! proposal when the outcome is unknown (always safe: every proposal has an
//! idempotency key), and tails the log by long-polling whichever replica
//! answers.
//!
//! Like shep-rpc's client, it is single-threaded: it runs on a
//! [`LocalRuntime`](tokio::runtime::LocalRuntime), and neither it nor its
//! futures are `Send`.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use futures::Stream;
use shep_rpc_client::{ConnectionCache, ReconnectConfig, RpcError};
use shep_wal_api::{
    AdminReply, CreatePartitionRequest, Decided, FetchReply, FetchRequest,
    InitializePartitionRequest, LogEntry, Member, PartitionId, PartitionStatus, Proposal,
    ProposeReply, ProposeRequest, Rejection, StatusReply, StatusRequest, method,
};
use shep_wal_controller::{
    ControllerState, CreateLogSetReply, CreateLogSetRequest, LogSet, LogSetId, StateReply,
    StateRequest, method as cm,
};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

pub use shep_wal_api as api;
pub use shep_wal_controller as controller;

/// Shep's reading of a log set: a universe, whose first partition is its
/// meta partition and the rest its data partitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Universe {
    pub id: LogSetId,
    pub name: String,
    pub meta: PartitionId,
    pub data: Vec<PartitionId>,
}

impl Universe {
    pub fn of(id: LogSetId, set: &LogSet) -> Option<Universe> {
        let (&meta, data) = set.partitions.split_first()?;
        Some(Universe {
            id,
            name: set.name.clone(),
            meta,
            data: data.to_vec(),
        })
    }

    /// Log set `id` in `state`, as a universe.
    pub fn find(state: &ControllerState, id: LogSetId) -> Option<Universe> {
        Universe::of(id, state.log_sets.get(&id)?)
    }

    /// Its meta partition, then its data partitions.
    pub fn partitions(&self) -> impl Iterator<Item = PartitionId> + '_ {
        std::iter::once(self.meta).chain(self.data.iter().copied())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid address {0:?}")]
    InvalidAddress(String),
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Decode(#[from] shep_codec::DecodeError),
    #[error("unknown partition")]
    UnknownPartition,
    #[error("partition already initialized")]
    AlreadyInitialized,
    #[error("{0}")]
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// How long `propose` keeps trying before giving up with `Unavailable`.
    pub propose_deadline: Duration,
    /// How long one RPC may take.
    pub rpc_timeout: Duration,
    /// The pause before trying again after a failure.
    pub retry_backoff: Duration,
    /// How long one fetch waits for entries; also how long a tail goes
    /// without entries before checking whether another replica is ahead.
    pub tail_idle: Duration,
    /// About how many bytes of entries one fetch returns.
    pub fetch_max_bytes: u32,
    pub reconnect: ReconnectConfig,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            propose_deadline: Duration::from_secs(10),
            rpc_timeout: Duration::from_secs(6),
            retry_backoff: Duration::from_millis(25),
            tail_idle: Duration::from_secs(1),
            fetch_max_bytes: 4 << 20,
            reconnect: ReconnectConfig {
                initial_backoff: Duration::from_millis(50),
                max_backoff: Duration::from_secs(1),
                connect_timeout: Duration::from_secs(1),
                ..ReconnectConfig::default()
            },
        }
    }
}

/// One WAL node.
#[derive(Clone)]
pub struct NodeClient {
    addr: String,
    sock: SocketAddr,
    cache: Rc<ConnectionCache>,
    rpc_timeout: Duration,
}

impl NodeClient {
    /// A client for `addr` (`host:port`). It connects on first use.
    pub fn new(addr: impl Into<String>) -> Result<NodeClient, ClientError> {
        let config = ClientConfig::default();
        NodeClient::with(
            addr.into(),
            Rc::new(ConnectionCache::new(config.reconnect)),
            config.rpc_timeout,
        )
    }

    fn with(
        addr: String,
        cache: Rc<ConnectionCache>,
        rpc_timeout: Duration,
    ) -> Result<NodeClient, ClientError> {
        let sock = addr
            .parse()
            .map_err(|_| ClientError::InvalidAddress(addr.clone()))?;
        Ok(NodeClient {
            addr,
            sock,
            cache,
            rpc_timeout,
        })
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    async fn call<Req: shep_codec::Encode, Resp: shep_codec::Decode>(
        &self,
        method: u32,
        req: &Req,
        timeout: Duration,
    ) -> Result<Resp, ClientError> {
        let body = self
            .cache
            .call(self.sock, method, shep_codec::to_bytes(req), timeout)
            .await?;
        Ok(shep_codec::from_bytes(&body)?)
    }

    /// Sends one proposal to this node, whatever its role.
    pub async fn propose(
        &self,
        partition: PartitionId,
        proposal: &Proposal,
    ) -> Result<Result<Decided, Rejection>, ClientError> {
        self.propose_within(partition, proposal, self.rpc_timeout)
            .await
    }

    async fn propose_within(
        &self,
        partition: PartitionId,
        proposal: &Proposal,
        timeout: Duration,
    ) -> Result<Result<Decided, Rejection>, ClientError> {
        let req = ProposeRequest {
            partition,
            proposal: proposal.clone(),
        };
        let reply: ProposeReply = self.call(method::PROPOSE, &req, timeout).await?;
        Ok(reply.into())
    }

    /// One long poll: committed entries from `from_index` on, waiting up to
    /// `wait` for any.
    pub async fn fetch(
        &self,
        partition: PartitionId,
        from_index: u64,
        max_bytes: u32,
        wait: Duration,
    ) -> Result<(Vec<LogEntry>, Option<u64>), ClientError> {
        let req = FetchRequest {
            partition,
            from_index,
            max_bytes,
            max_wait_ms: wait.as_millis().min(u32::MAX as u128) as u32,
        };
        match self
            .call(method::FETCH, &req, wait + self.rpc_timeout)
            .await?
        {
            FetchReply::Entries {
                entries,
                applied_index,
            } => Ok((entries, applied_index)),
            FetchReply::OutOfRange { start } => Err(ClientError::Failed(format!(
                "index {from_index} is no longer held; the log starts at {start}"
            ))),
            FetchReply::UnknownPartition => Err(ClientError::UnknownPartition),
        }
    }

    pub async fn status(&self, partition: PartitionId) -> Result<PartitionStatus, ClientError> {
        match self
            .call(
                method::STATUS,
                &StatusRequest { partition },
                self.rpc_timeout,
            )
            .await?
        {
            StatusReply::Status(s) => Ok(s),
            StatusReply::UnknownPartition => Err(ClientError::UnknownPartition),
        }
    }

    pub async fn create_partition(&self, partition: PartitionId) -> Result<(), ClientError> {
        admin(
            self.call(
                method::CREATE_PARTITION,
                &CreatePartitionRequest { partition },
                self.rpc_timeout,
            )
            .await?,
        )
    }

    pub async fn initialize_partition(
        &self,
        partition: PartitionId,
        members: &[Member],
    ) -> Result<(), ClientError> {
        let req = InitializePartitionRequest {
            partition,
            members: members.to_vec(),
        };
        admin(
            self.call(method::INITIALIZE_PARTITION, &req, self.rpc_timeout)
                .await?,
        )
    }
}

fn admin(reply: AdminReply) -> Result<(), ClientError> {
    match reply {
        AdminReply::Ok => Ok(()),
        AdminReply::AlreadyInitialized => Err(ClientError::AlreadyInitialized),
        AdminReply::UnknownPartition => Err(ClientError::UnknownPartition),
        AdminReply::Failed(m) => Err(ClientError::Failed(m)),
    }
}

/// Why `propose` gave up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProposeError {
    /// Re-execute the mutation at a newer version.
    #[error("contended")]
    Contended,
    /// Re-execute the mutation at a newer version.
    #[error("read version too old")]
    TooOld,
    #[error("invalid proposal: {0}")]
    Invalid(String),
    /// No leader decided it before the deadline. The outcome is unknown:
    /// propose it again, under the same key, to learn it.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

/// A client for a set of WAL nodes.
#[derive(Clone)]
pub struct WalClient {
    shared: Rc<Shared>,
}

struct Shared {
    config: ClientConfig,
    cache: Rc<ConnectionCache>,
    nodes: RefCell<Vec<NodeClient>>,
    leaders: RefCell<std::collections::HashMap<PartitionId, String>>,
}

impl WalClient {
    pub fn new<I, S>(addrs: I) -> Result<WalClient, ClientError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        WalClient::with_config(addrs, ClientConfig::default())
    }

    pub fn with_config<I, S>(addrs: I, config: ClientConfig) -> Result<WalClient, ClientError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let cache = Rc::new(ConnectionCache::new(config.reconnect.clone()));
        let nodes = addrs
            .into_iter()
            .map(|a| NodeClient::with(a.into(), cache.clone(), config.rpc_timeout))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(WalClient {
            shared: Rc::new(Shared {
                config,
                cache,
                nodes: RefCell::new(nodes),
                leaders: RefCell::new(Default::default()),
            }),
        })
    }

    /// The client for one node, added to the set if it's new.
    pub fn node(&self, addr: &str) -> Result<NodeClient, ClientError> {
        let mut nodes = self.shared.nodes.borrow_mut();
        if let Some(n) = nodes.iter().find(|n| n.addr == addr) {
            return Ok(n.clone());
        }
        let n = NodeClient::with(
            addr.to_string(),
            self.shared.cache.clone(),
            self.shared.config.rpc_timeout,
        )?;
        nodes.push(n.clone());
        Ok(n)
    }

    /// The addresses of every node the client knows.
    pub fn addrs(&self) -> Vec<String> {
        self.nodes().iter().map(|n| n.addr.clone()).collect()
    }

    fn nodes(&self) -> Vec<NodeClient> {
        self.shared.nodes.borrow_mut().clone()
    }

    /// Proposes to `partition`'s leader until a leader decides it, or it's
    /// rejected in a way only re-execution can fix, or the deadline passes.
    /// No RPC or pause runs past the deadline.
    pub async fn propose(
        &self,
        partition: PartitionId,
        proposal: &Proposal,
    ) -> Result<Decided, ProposeError> {
        let config = &self.shared.config;
        let deadline = Instant::now() + config.propose_deadline;
        let nodes = self.nodes();
        let mut next = 0usize;
        let mut target = self.shared.leaders.borrow_mut().get(&partition).cloned();
        let mut last = String::from("no attempt");
        let mut unknown = 0;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ProposeError::Unavailable(last));
            }
            let node = match target.take() {
                Some(addr) => self
                    .node(&addr)
                    .map_err(|e| ProposeError::Unavailable(e.to_string()))?,
                None if nodes.is_empty() => {
                    return Err(ProposeError::Unavailable("no nodes".into()));
                }
                None => {
                    next += 1;
                    nodes[next % nodes.len()].clone()
                }
            };
            let wait = config.rpc_timeout.min(remaining);
            let rpc =
                tokio::time::timeout(wait, node.propose_within(partition, proposal, wait)).await;
            let mut pause = true;
            match rpc {
                Ok(Ok(Ok(decided))) => {
                    self.shared
                        .leaders
                        .borrow_mut()
                        .insert(partition, node.addr.clone());
                    return Ok(decided);
                }
                Ok(Ok(Err(Rejection::Contended))) => return Err(ProposeError::Contended),
                Ok(Ok(Err(Rejection::TooOld))) => return Err(ProposeError::TooOld),
                Ok(Ok(Err(Rejection::Invalid(m)))) => return Err(ProposeError::Invalid(m)),
                Ok(Ok(Err(Rejection::NotLeader { leader }))) => {
                    last = format!("{} isn't the leader", node.addr);
                    if let Some(leader) = leader.filter(|l| l.addr != node.addr) {
                        self.shared
                            .leaders
                            .borrow_mut()
                            .insert(partition, leader.addr.clone());
                        target = Some(leader.addr);
                        pause = false;
                    }
                }
                Ok(Ok(Err(Rejection::UnknownOutcome(m)))) => {
                    // Usually still the leader, so ask it again, but not
                    // forever: a leader cut off from its peers can't commit.
                    last = format!("{}: {m}", node.addr);
                    unknown += 1;
                    if unknown < 2 {
                        target = Some(node.addr.clone());
                    } else {
                        unknown = 0;
                        self.shared.leaders.borrow_mut().remove(&partition);
                    }
                }
                Ok(Ok(Err(r @ Rejection::UnknownPartition))) => {
                    last = format!("{}: {r}", node.addr)
                }
                Ok(Err(e)) => last = format!("{}: {e}", node.addr),
                Err(_) => last = format!("{}: timed out", node.addr),
            }
            if pause {
                tokio::time::sleep_until((Instant::now() + config.retry_backoff).min(deadline))
                    .await;
            }
        }
    }

    /// Every committed entry of `partition` from `from_index` on, in order,
    /// from whichever replica serves them. It moves to another replica when
    /// one fails, or when one has nothing new while another has applied
    /// further (say, it was cut off from its leader). The stream never ends
    /// while it's polled.
    pub fn tail(
        &self,
        partition: PartitionId,
        from_index: u64,
    ) -> impl Stream<Item = LogEntry> + use<> {
        let (tx, rx) = mpsc::channel(256);
        let client = self.clone();
        tokio::task::spawn_local(async move {
            let config = client.shared.config.clone();
            let mut next = from_index;
            let mut n = 0usize;
            let mut preferred: Option<String> = None;
            'nodes: loop {
                let node = match preferred.take().map(|addr| client.node(&addr)) {
                    Some(Ok(node)) => node,
                    _ => {
                        let nodes = client.nodes();
                        n += 1;
                        nodes[(n - 1) % nodes.len()].clone()
                    }
                };
                loop {
                    let fetched = node
                        .fetch(partition, next, config.fetch_max_bytes, config.tail_idle)
                        .await;
                    let entries = match fetched {
                        Ok((entries, _)) => entries,
                        Err(e) => {
                            tracing::debug!(addr = node.addr, "tail: {e}");
                            break;
                        }
                    };
                    if entries.is_empty() {
                        // Nothing new here: either nothing committed, or this
                        // replica fell behind. Ask the others.
                        if let Some(addr) = client.replica_ahead(partition, next, &node.addr).await
                        {
                            preferred = Some(addr);
                            continue 'nodes;
                        }
                        if tx.is_closed() {
                            return;
                        }
                        continue;
                    }
                    for entry in entries {
                        if entry.dot.index < next {
                            continue;
                        }
                        if entry.dot.index > next {
                            tracing::warn!(
                                addr = node.addr,
                                next,
                                got = entry.dot.index,
                                "tail: gap"
                            );
                            break;
                        }
                        next += 1;
                        if tx.send(entry).await.is_err() {
                            return;
                        }
                    }
                }
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(config.retry_backoff).await;
            }
        });
        ReceiverStream::new(rx)
    }

    /// The WAL controller's state, from the first node that answers.
    pub async fn controller_state(&self) -> Result<ControllerState, ClientError> {
        let mut last = ClientError::Failed("no nodes".into());
        for node in self.nodes() {
            match node
                .call(cm::STATE, &StateRequest {}, self.shared.config.rpc_timeout)
                .await
            {
                Ok(StateReply::State(s)) => return Ok(s),
                Ok(StateReply::NoController) => {
                    last = ClientError::Failed(format!("{} hosts no controller", node.addr))
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Creates a universe, `data_partitions` data partitions and a meta
    /// partition with `replication` replicas each: see
    /// [`WalClient::create_log_set`].
    pub async fn create_universe(
        &self,
        name: &str,
        data_partitions: u32,
        replication: u32,
    ) -> Result<(Universe, ControllerState), ClientError> {
        let partitions = data_partitions.saturating_add(1);
        let (id, state) = self.create_log_set(name, partitions, replication).await?;
        let universe = Universe::find(&state, id)
            .ok_or_else(|| ClientError::Failed(format!("log set {} isn't a universe", id.0)))?;
        Ok((universe, state))
    }

    /// Creates a log set through the controller's leader, following
    /// redirects until `propose_deadline`. No RPC or pause runs past it.
    pub async fn create_log_set(
        &self,
        name: &str,
        partitions: u32,
        replication: u32,
    ) -> Result<(LogSetId, ControllerState), ClientError> {
        let config = &self.shared.config;
        let deadline = Instant::now() + config.propose_deadline;
        let req = CreateLogSetRequest {
            name: name.to_string(),
            partitions,
            replication,
        };
        let nodes = self.nodes();
        let mut target: Option<String> = None;
        let mut n = 0usize;
        let mut last = String::from("no attempt");
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ClientError::Failed(format!(
                    "no controller leader created it in time: {last}"
                )));
            }
            let node = match target.take() {
                Some(addr) => self.node(&addr)?,
                None if nodes.is_empty() => return Err(ClientError::Failed("no nodes".into())),
                None => {
                    n += 1;
                    nodes[n % nodes.len()].clone()
                }
            };
            let wait = config.rpc_timeout.min(remaining);
            let rpc = tokio::time::timeout(wait, node.call(cm::CREATE_LOG_SET, &req, wait)).await;
            match rpc {
                Ok(Ok(CreateLogSetReply::Created { id, state })) => return Ok((id, state)),
                Ok(Ok(CreateLogSetReply::Rejected(m))) => return Err(ClientError::Failed(m)),
                Ok(Ok(CreateLogSetReply::NotLeader { leader: Some(l) })) if l.addr != node.addr => {
                    target = Some(l.addr);
                    continue;
                }
                Ok(Ok(CreateLogSetReply::NotLeader { .. })) => {
                    last = format!("{} isn't the leader", node.addr)
                }
                Ok(Err(e)) => last = format!("{}: {e}", node.addr),
                Err(_) => last = format!("{}: timed out", node.addr),
            }
            tokio::time::sleep_until((Instant::now() + config.retry_backoff).min(deadline)).await;
        }
    }

    /// Another node that has applied `index`, the most advanced one.
    async fn replica_ahead(
        &self,
        partition: PartitionId,
        index: u64,
        current: &str,
    ) -> Option<String> {
        let timeout = self.shared.config.rpc_timeout;
        let others: Vec<NodeClient> = self
            .nodes()
            .into_iter()
            .filter(|n| n.addr != current)
            .collect();
        let statuses = futures::future::join_all(
            others
                .iter()
                .map(|n| tokio::time::timeout(timeout, n.status(partition))),
        )
        .await;
        others
            .iter()
            .zip(statuses)
            .filter_map(|(n, s)| Some((n.addr.clone(), s.ok()?.ok()?.applied_index?)))
            .filter(|(_, applied)| *applied >= index)
            .max_by_key(|(_, applied)| *applied)
            .map(|(addr, _)| addr)
    }
}
