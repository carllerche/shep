//! A client for the WAL's gRPC protocol.
//!
//! [`NodeClient`] talks to one node. [`WalClient`] knows a set of nodes and
//! routes each proposal to its partition's leader, re-sending the same
//! proposal when the outcome is unknown (always safe: every proposal has an
//! idempotency key), and tails the log from whichever replica answers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{Stream, StreamExt};
use shep_wal_api::proto::wal_admin_client::WalAdminClient;
use shep_wal_api::proto::wal_client::WalClient as WalRpc;
use shep_wal_api::{
    Decided, LogEntry, Member, PartitionId, PartitionStatus, Proposal, Rejection, proto,
};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};

pub use shep_wal_api as api;

/// The default for the largest message either side sends.
const MAX_MESSAGE: usize = 256 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid address {0:?}")]
    InvalidAddress(String),
    #[error(transparent)]
    Rpc(#[from] tonic::Status),
    #[error(transparent)]
    Decode(#[from] shep_wal_api::DecodeError),
}

/// A connection to one WAL node.
#[derive(Clone)]
pub struct NodeClient {
    addr: String,
    wal: WalRpc<Channel>,
    admin: WalAdminClient<Channel>,
}

impl NodeClient {
    /// A client for `addr` (`host:port`). It connects on first use.
    pub fn new(addr: impl Into<String>) -> Result<NodeClient, ClientError> {
        NodeClient::with_max_message(addr, MAX_MESSAGE)
    }

    /// Like `new`, sending and accepting messages up to `max_message` bytes.
    pub fn with_max_message(
        addr: impl Into<String>,
        max_message: usize,
    ) -> Result<NodeClient, ClientError> {
        let addr = addr.into();
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .map_err(|_| ClientError::InvalidAddress(addr.clone()))?
            .connect_timeout(Duration::from_secs(1))
            .tcp_nodelay(true)
            .connect_lazy();
        Ok(NodeClient {
            addr,
            wal: WalRpc::new(channel.clone())
                .max_decoding_message_size(max_message)
                .max_encoding_message_size(max_message),
            admin: WalAdminClient::new(channel),
        })
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Sends one proposal to this node, whatever its role.
    pub async fn propose(
        &self,
        partition: PartitionId,
        proposal: &Proposal,
    ) -> Result<Result<Decided, Rejection>, ClientError> {
        let req = proto::ProposeRequest {
            partition: partition.0,
            proposal: Some(proposal.into()),
        };
        let resp = self.wal.clone().propose(req).await?.into_inner();
        Ok(resp.try_into()?)
    }

    /// Committed entries from `from_index` on, as this node serves them. The
    /// stream ends with an error if the node stops serving it.
    pub async fn tail(
        &self,
        partition: PartitionId,
        from_index: u64,
    ) -> Result<impl Stream<Item = Result<Vec<LogEntry>, ClientError>> + use<>, ClientError> {
        let req = proto::TailRequest {
            partition: partition.0,
            from_index,
        };
        let stream = self.wal.clone().tail(req).await?.into_inner();
        Ok(stream.map(move |resp| {
            let resp = resp?;
            Ok(resp
                .entries
                .into_iter()
                .map(|e| LogEntry::from_proto(partition, e))
                .collect::<Result<Vec<_>, _>>()?)
        }))
    }

    pub async fn status(&self, partition: PartitionId) -> Result<PartitionStatus, ClientError> {
        let req = proto::StatusRequest {
            partition: partition.0,
        };
        Ok(self.wal.clone().status(req).await?.into_inner().into())
    }

    pub async fn create_partition(&self, partition: PartitionId) -> Result<(), ClientError> {
        self.admin
            .clone()
            .create_partition(proto::CreatePartitionRequest {
                partition: partition.0,
            })
            .await?;
        Ok(())
    }

    pub async fn initialize_partition(
        &self,
        partition: PartitionId,
        members: &[Member],
    ) -> Result<(), ClientError> {
        let req = proto::InitializePartitionRequest {
            partition: partition.0,
            members: members.iter().map(Into::into).collect(),
        };
        self.admin.clone().initialize_partition(req).await?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// How long `propose` keeps trying before giving up with `Unavailable`.
    pub propose_deadline: Duration,
    /// How long one RPC may take.
    pub rpc_timeout: Duration,
    /// The pause before trying again after a failure.
    pub retry_backoff: Duration,
    /// How long a tail may go without entries before checking whether
    /// another replica is ahead of the one it's reading.
    pub tail_idle: Duration,
    /// The largest message sent or accepted. It must be at least the
    /// servers' limit, or large batches are refused.
    pub max_message_size: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            propose_deadline: Duration::from_secs(10),
            rpc_timeout: Duration::from_secs(6),
            retry_backoff: Duration::from_millis(25),
            tail_idle: Duration::from_secs(1),
            max_message_size: MAX_MESSAGE,
        }
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
    shared: Arc<Shared>,
}

struct Shared {
    config: ClientConfig,
    nodes: Mutex<Vec<NodeClient>>,
    leaders: Mutex<HashMap<PartitionId, String>>,
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
        let nodes = addrs
            .into_iter()
            .map(|addr| NodeClient::with_max_message(addr, config.max_message_size))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(WalClient {
            shared: Arc::new(Shared {
                config,
                nodes: Mutex::new(nodes),
                leaders: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// The client for one node, added to the set if it's new.
    pub fn node(&self, addr: &str) -> Result<NodeClient, ClientError> {
        let mut nodes = self.shared.nodes.lock().unwrap();
        if let Some(n) = nodes.iter().find(|n| n.addr == addr) {
            return Ok(n.clone());
        }
        let n = NodeClient::with_max_message(addr, self.shared.config.max_message_size)?;
        nodes.push(n.clone());
        Ok(n)
    }

    /// The addresses of every node the client knows.
    pub fn addrs(&self) -> Vec<String> {
        self.nodes().iter().map(|n| n.addr.clone()).collect()
    }

    fn nodes(&self) -> Vec<NodeClient> {
        self.shared.nodes.lock().unwrap().clone()
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
        let mut target = self.shared.leaders.lock().unwrap().get(&partition).cloned();
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
            let rpc = tokio::time::timeout(wait, node.propose(partition, proposal)).await;
            let mut pause = true;
            match rpc {
                Ok(Ok(Ok(decided))) => {
                    self.shared
                        .leaders
                        .lock()
                        .unwrap()
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
                            .lock()
                            .unwrap()
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
                        self.shared.leaders.lock().unwrap().remove(&partition);
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
    /// one fails, or when one goes quiet while another has applied entries
    /// it hasn't sent (say, it was cut off from its leader). The stream never
    /// ends while it's polled.
    pub fn tail(
        &self,
        partition: PartitionId,
        from_index: u64,
    ) -> impl Stream<Item = LogEntry> + use<> {
        let (tx, rx) = mpsc::channel(256);
        let client = self.clone();
        tokio::spawn(async move {
            let mut next = from_index;
            let mut n = 0usize;
            let mut preferred: Option<String> = None;
            loop {
                let node = match preferred.take().map(|addr| client.node(&addr)) {
                    Some(Ok(node)) => node,
                    _ => {
                        let nodes = client.nodes();
                        n += 1;
                        nodes[(n - 1) % nodes.len()].clone()
                    }
                };
                match client.follow(&node, partition, &mut next, &tx).await {
                    Follow::Closed => return,
                    Follow::SwitchTo(addr) => preferred = Some(addr),
                    Follow::Failed => tokio::time::sleep(client.shared.config.retry_backoff).await,
                }
            }
        });
        ReceiverStream::new(rx)
    }

    /// Forwards `node`'s tail to `tx` until the node fails, falls behind
    /// another replica, or the receiver goes away.
    async fn follow(
        &self,
        node: &NodeClient,
        partition: PartitionId,
        next: &mut u64,
        tx: &mpsc::Sender<LogEntry>,
    ) -> Follow {
        let config = &self.shared.config;
        let open = tokio::time::timeout(config.rpc_timeout, node.tail(partition, *next)).await;
        let stream = match open {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                tracing::debug!(addr = node.addr, "tail: {e}");
                return Follow::Failed;
            }
            Err(_) => return Follow::Failed,
        };
        futures::pin_mut!(stream);
        loop {
            tokio::select! {
                batch = stream.next() => {
                    let Some(Ok(batch)) = batch else { return Follow::Failed };
                    for entry in batch {
                        if entry.dot.index < *next {
                            continue;
                        }
                        if entry.dot.index > *next {
                            tracing::warn!(addr = node.addr, next, got = entry.dot.index, "tail: gap");
                            return Follow::Failed;
                        }
                        *next += 1;
                        if tx.send(entry).await.is_err() {
                            return Follow::Closed;
                        }
                    }
                }
                _ = tokio::time::sleep(config.tail_idle) => {
                    // Quiet: either nothing committed, or this replica fell
                    // behind. Ask the others.
                    if let Some(addr) = self.replica_ahead(partition, *next, &node.addr).await {
                        return Follow::SwitchTo(addr);
                    }
                }
                _ = tx.closed() => return Follow::Closed,
            }
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

enum Follow {
    /// The receiver went away.
    Closed,
    Failed,
    SwitchTo(String),
}
