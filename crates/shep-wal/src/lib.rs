//! The write-ahead log (design §3, §10): one Raft group per partition, run by
//! openraft, with the leader validating every proposal before appending it.
//!
//! A [`WalNode`] hosts replicas of any number of partitions and serves three
//! gRPC services on one address: the client protocol (`shep.wal.v1.Wal`), its
//! admin service, and the peer protocol between replicas
//! (`shep.wal.raft.v1.Raft`). Clients use `shep-wal-client`.
//!
//! What the WAL guarantees (§10.3): committed mutations are serializable in
//! each partition's log order. A proposal commits only if no entry its read
//! version doesn't contain wrote a key in its read set or a range it read,
//! including entries appended but not yet committed, and including entries a
//! previous leader appended. Each idempotency key is decided at most once.

mod log_store;
mod network;
mod partition;
mod proposer;
mod raft_types;
mod service;
mod state_machine;
mod table;

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use openraft::BasicNode;
use shep_wal_api::{Member, NodeId, PartitionId, PartitionStatus};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use network::Faults;

use network::Peers;
use partition::Partition;

#[derive(Clone, Debug)]
pub struct WalConfig {
    pub node_id: NodeId,
    /// Every partition replica's log and state live under
    /// `data_dir/partitions/<id>`.
    pub data_dir: PathBuf,
    /// openraft also uses this as the deadline for each AppendEntries RPC,
    /// and a follower answers only once the entries are synced to disk, so
    /// it must be well above the disk's sync latency.
    pub heartbeat_interval: Duration,
    /// A follower that hears nothing for a random time in this range starts
    /// an election.
    pub election_timeout: Range<Duration>,
    /// How long write sets stay in the leader's validation table. A
    /// proposal whose read version is older than the table gets `TooOld`.
    pub validation_window: Duration,
    /// The most entries the validation table holds, however recent.
    pub validation_window_entries: usize,
    /// How long a decided idempotency key is remembered.
    pub idempotency_window: Duration,
    /// How long a proposal waits to commit before its sender is told the
    /// outcome is unknown.
    pub commit_timeout: Duration,
    /// Log segment files roll over at this size.
    pub segment_size: u64,
    /// The largest gRPC message this node sends or accepts. A proposal's
    /// entry may take at most half of it, so any one entry fits in a
    /// replication or tail batch.
    pub max_message_size: usize,
}

impl WalConfig {
    pub fn new(node_id: NodeId, data_dir: impl Into<PathBuf>) -> WalConfig {
        WalConfig {
            node_id,
            data_dir: data_dir.into(),
            heartbeat_interval: Duration::from_millis(100),
            election_timeout: Duration::from_millis(1000)..Duration::from_millis(2000),
            validation_window: Duration::from_secs(5),
            validation_window_entries: 1 << 20,
            idempotency_window: Duration::from_secs(24 * 60 * 60),
            commit_timeout: Duration::from_secs(5),
            segment_size: 64 << 20,
            max_message_size: 256 << 20,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("raft: {0}")]
    Raft(String),
    #[error("partition {0} isn't hosted here")]
    UnknownPartition(PartitionId),
    #[error("partition {0} is already initialized")]
    AlreadyInitialized(PartitionId),
}

/// A WAL node: replicas of some partitions, and the gRPC services for them.
#[derive(Clone)]
pub struct WalNode {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    config: Arc<WalConfig>,
    partitions: RwLock<HashMap<PartitionId, Arc<Partition>>>,
    creating: tokio::sync::Mutex<()>,
    peers: Arc<Peers>,
    faults: Arc<Faults>,
    shutdown: watch::Sender<bool>,
}

impl WalNode {
    /// Opens the node's data directory and every partition replica in it.
    pub async fn open(config: WalConfig) -> Result<WalNode, WalError> {
        let dir = config.data_dir.join("partitions");
        std::fs::create_dir_all(&dir)?;
        let peers = Arc::new(Peers::new(config.max_message_size));
        let node = WalNode {
            inner: Arc::new(Inner {
                config: Arc::new(config),
                partitions: RwLock::new(HashMap::new()),
                creating: tokio::sync::Mutex::new(()),
                peers,
                faults: Arc::new(Faults::default()),
                shutdown: watch::channel(false).0,
            }),
        };
        for ent in std::fs::read_dir(&dir)? {
            let ent = ent?;
            if let Some(id) = ent.file_name().to_str().and_then(|n| n.parse::<u64>().ok()) {
                node.create_partition(PartitionId(id)).await?;
            }
        }
        Ok(node)
    }

    pub fn node_id(&self) -> NodeId {
        self.inner.config.node_id
    }

    pub fn faults(&self) -> &Faults {
        &self.inner.faults
    }

    /// Creates this node's replica of `partition`, empty, or opens the one
    /// it has.
    pub async fn create_partition(&self, partition: PartitionId) -> Result<(), WalError> {
        self.inner.create_partition(partition).await
    }

    /// Initializes `partition`'s Raft group with its first members. Call it
    /// on one member, after every member has created its replica.
    pub async fn initialize_partition(
        &self,
        partition: PartitionId,
        members: &[Member],
    ) -> Result<(), WalError> {
        self.inner.initialize_partition(partition, members).await
    }

    pub fn partition_status(&self, partition: PartitionId) -> Result<PartitionStatus, WalError> {
        Ok(self.inner.partition(partition)?.status())
    }

    /// Serves every service on `listener` until [`WalNode::shutdown`].
    pub fn serve(&self, listener: TcpListener) -> JoinHandle<Result<(), tonic::transport::Error>> {
        service::serve(self.inner.clone(), listener)
    }

    /// Stops serving and shuts every Raft group down.
    pub async fn shutdown(&self) {
        self.inner.shutdown.send_replace(true);
        let partitions: Vec<_> = self
            .inner
            .partitions
            .write()
            .unwrap()
            .drain()
            .map(|(_, p)| p)
            .collect();
        for p in partitions {
            if let Err(e) = p.raft.shutdown().await {
                tracing::warn!(partition = %p.id, "raft shutdown: {e}");
            }
        }
    }
}

impl Inner {
    pub(crate) fn partition(&self, id: PartitionId) -> Result<Arc<Partition>, WalError> {
        self.partitions
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(WalError::UnknownPartition(id))
    }

    async fn create_partition(&self, id: PartitionId) -> Result<(), WalError> {
        let _guard = self.creating.lock().await;
        if self.partitions.read().unwrap().contains_key(&id) {
            return Ok(());
        }
        let dir = self
            .config
            .data_dir
            .join("partitions")
            .join(id.0.to_string());
        std::fs::create_dir_all(&dir)?;
        let p = Partition::open(
            id,
            &dir,
            self.config.clone(),
            self.peers.clone(),
            self.faults.clone(),
        )
        .await?;
        self.partitions.write().unwrap().insert(id, Arc::new(p));
        Ok(())
    }

    async fn initialize_partition(
        &self,
        id: PartitionId,
        members: &[Member],
    ) -> Result<(), WalError> {
        let p = self.partition(id)?;
        let members: BTreeMap<u64, BasicNode> = members
            .iter()
            .map(|m| (m.node_id, BasicNode::new(m.addr.clone())))
            .collect();
        p.raft.initialize(members).await.map_err(|e| match e {
            openraft::error::RaftError::APIError(openraft::error::InitializeError::NotAllowed(
                _,
            )) => WalError::AlreadyInitialized(id),
            e => WalError::Raft(e.to_string()),
        })
    }
}
