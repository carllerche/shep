//! A WAL cluster in this process, on loopback shep-rpc, plus a toy materializer
//! that tails the log so tests can read at versions and check what committed.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use shep_smp::{Smp, SmpConfig};
use shep_wal::{WalConfig, WalNode};
use shep_wal_client::api::{
    Decided, DecidedStatus, Dot, Dvv, EntryKind, KeyRange, LogEntry, Member, Mutation, PartitionId,
    Proposal, ProposalKind,
};
use shep_wal_client::{ClientConfig, NodeClient, ProposeError, WalClient};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::watch;

pub const P1: PartitionId = PartitionId(1);

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

type Configure = dyn Fn(usize, &mut WalConfig) + Send + Sync;

/// Cores per node.
pub const CORES: usize = 2;

pub struct Cluster {
    nodes: Vec<Option<(WalNode, Smp)>>,
    dirs: Vec<TempDir>,
    addrs: Vec<SocketAddr>,
    configure: Arc<Configure>,
    /// Every node is a seed of a controller-founded cluster.
    controlled: bool,
    pub client: WalClient,
}

/// Timeouts short enough for tests, long enough not to flap on a busy machine.
pub fn test_config(config: &mut WalConfig) {
    config.raft.heartbeat_interval = Duration::from_millis(100);
    config.raft.election_timeout = Duration::from_millis(600)..Duration::from_millis(1200);
    config.raft.reconnect = shep_rpc_client::ReconnectConfig {
        initial_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        connect_timeout: Duration::from_millis(500),
        transport: Default::default(),
    };
    config.raft.proposals.commit_timeout = Duration::from_secs(2);
}

impl Cluster {
    /// `n` nodes, each with a replica of every partition in `partitions`.
    pub async fn start(n: usize, partitions: &[PartitionId]) -> Cluster {
        Cluster::start_with(n, partitions, |_| {}).await
    }

    pub async fn start_with(
        n: usize,
        partitions: &[PartitionId],
        configure: impl Fn(&mut WalConfig) + Send + Sync + 'static,
    ) -> Cluster {
        Cluster::start_with_nodes(n, partitions, move |_, c| configure(c)).await
    }

    /// Like `start_with`, configuring each node by its index.
    pub async fn start_with_nodes(
        n: usize,
        partitions: &[PartitionId],
        configure: impl Fn(usize, &mut WalConfig) + Send + Sync + 'static,
    ) -> Cluster {
        init_tracing();
        let mut listeners = Vec::new();
        let mut addrs = Vec::new();
        for _ in 0..n {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(l.local_addr().unwrap());
            listeners.push(l);
        }
        let client = WalClient::with_config(
            addrs.iter().map(|a| a.to_string()),
            ClientConfig {
                propose_deadline: Duration::from_secs(20),
                ..ClientConfig::default()
            },
        )
        .unwrap();
        let mut cluster = Cluster {
            nodes: Vec::new(),
            dirs: (0..n).map(|_| tempfile::tempdir().unwrap()).collect(),
            addrs,
            configure: Arc::new(configure),
            controlled: false,
            client,
        };
        for (i, l) in listeners.into_iter().enumerate() {
            let (node, smp) = cluster.open(i).await;
            node.serve(l);
            cluster.nodes.push(Some((node, smp)));
        }
        for &p in partitions {
            cluster.create_partition(p).await;
        }
        cluster
    }

    async fn open(&self, i: usize) -> (WalNode, Smp) {
        let mut config = WalConfig::new(i as u64 + 1, self.dirs[i].path());
        test_config(&mut config);
        if self.controlled {
            config.seeds = self.members();
        }
        (self.configure)(i, &mut config);
        let smp = Smp::start(SmpConfig {
            pin: false,
            ..SmpConfig::with_cores(CORES)
        })
        .unwrap();
        (WalNode::start(config, smp.clone()).await.unwrap(), smp)
    }

    /// `n` nodes that found a cluster with a WAL controller; partitions come
    /// from the controller.
    pub async fn start_controlled(n: usize) -> Cluster {
        Cluster::start_controlled_some(n, n).await
    }

    /// Like `start_controlled`, starting only the first `running` nodes; the
    /// others start with `restart`.
    pub async fn start_controlled_some(n: usize, running: usize) -> Cluster {
        Cluster::start_controlled_with(n, running, |_, _| {}).await
    }

    /// Like `start_controlled_some`, configuring each node by index after its
    /// seeds are set.
    pub async fn start_controlled_with(
        n: usize,
        running: usize,
        configure: impl Fn(usize, &mut WalConfig) + Send + Sync + 'static,
    ) -> Cluster {
        init_tracing();
        let mut listeners = Vec::new();
        let mut addrs = Vec::new();
        for _ in 0..n {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(l.local_addr().unwrap());
            listeners.push(l);
        }
        let client = WalClient::with_config(
            addrs.iter().map(|a| a.to_string()),
            ClientConfig {
                propose_deadline: Duration::from_secs(20),
                ..ClientConfig::default()
            },
        )
        .unwrap();
        let mut cluster = Cluster {
            nodes: Vec::new(),
            dirs: (0..n).map(|_| tempfile::tempdir().unwrap()).collect(),
            addrs,
            configure: Arc::new(configure),
            controlled: true,
            client,
        };
        for (i, l) in listeners.into_iter().enumerate() {
            if i >= running {
                drop(l);
                cluster.nodes.push(None);
                continue;
            }
            let (node, smp) = cluster.open(i).await;
            node.serve(l);
            cluster.nodes.push(Some((node, smp)));
        }
        cluster
    }

    /// Waits for every running node to see the cluster founded, and returns
    /// its ID.
    pub async fn founded(&self) -> Vec<u8> {
        for _ in 0..1500 {
            let mut ids = Vec::new();
            for (n, _) in self.nodes.iter().flatten() {
                ids.push(n.controller_state().await.and_then(|s| s.cluster_id));
            }
            if let Some(Some(id)) = ids.first()
                && ids.iter().all(|i| i.as_ref() == Some(id))
            {
                return id.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the cluster was never founded");
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn members(&self) -> Vec<Member> {
        (0..self.addrs.len())
            .map(|i| Member {
                node_id: i as u64 + 1,
                addr: self.addr(i),
            })
            .collect()
    }

    /// Creates `p` on every node, initializes it, and waits for a leader.
    pub async fn create_partition(&self, p: PartitionId) {
        for i in 0..self.len() {
            self.node_client(i).create_partition(p).await.unwrap();
        }
        self.node_client(0)
            .initialize_partition(p, &self.members())
            .await
            .unwrap();
        self.leader(p).await;
    }

    /// Node `i`'s log directory for `p`.
    pub fn replica_dir(&self, i: usize, p: PartitionId) -> std::path::PathBuf {
        self.dirs[i].path().join("logs").join(p.0.to_string())
    }

    pub fn addr(&self, i: usize) -> String {
        self.addrs[i].to_string()
    }

    pub fn node(&self, i: usize) -> &WalNode {
        &self.nodes[i].as_ref().expect("node is stopped").0
    }

    pub fn node_client(&self, i: usize) -> NodeClient {
        self.client.node(&self.addr(i)).unwrap()
    }

    pub fn is_running(&self, i: usize) -> bool {
        self.nodes[i].is_some()
    }

    /// The index of `p`'s leader, once a running node leads and every other
    /// running node that isn't cut off agrees.
    pub async fn leader(&self, p: PartitionId) -> usize {
        self.leader_where(p, |_| true).await
    }

    /// Like `leader`, but waits for a leader that satisfies `ok`.
    pub async fn leader_where(&self, p: PartitionId, ok: impl Fn(usize) -> bool) -> usize {
        for _ in 0..1000 {
            let mut views: Vec<Option<u64>> = Vec::new();
            for (n, _) in self
                .nodes
                .iter()
                .flatten()
                .filter(|(n, _)| !n.faults().is_partitioned())
            {
                views.push(n.status(p).await.and_then(|s| s.leader).map(|m| m.node_id));
            }
            if let Some(&Some(id)) = views.first()
                && views.iter().all(|v| *v == Some(id))
            {
                let i = id as usize - 1;
                if self.is_running(i) && !self.node(i).faults().is_partitioned() && ok(i) {
                    return i;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no leader for {p}");
    }

    /// Stops node `i`, as if its process exited.
    pub async fn stop(&mut self, i: usize) {
        let (node, smp) = self.nodes[i].take().expect("already stopped");
        node.shutdown().await;
        let smp = smp.clone();
        let (done, finished) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            smp.shutdown();
            let _ = done.send(());
        });
        let _ = finished.await;
    }

    /// Restarts node `i` from its data directory, at its address.
    pub async fn restart(&mut self, i: usize) {
        assert!(self.nodes[i].is_none(), "node {i} is running");
        let (node, smp) = self.open(i).await;
        let mut tries = 0;
        let listener = loop {
            match TcpListener::bind(self.addrs[i]).await {
                Ok(l) => break l,
                Err(e) if tries < 100 => {
                    tries += 1;
                    tracing::debug!("rebinding {}: {e}", self.addrs[i]);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("rebinding {}: {e}", self.addrs[i]),
            }
        };
        node.serve(listener);
        self.nodes[i] = Some((node, smp));
    }

    pub async fn shutdown(mut self) {
        for i in 0..self.len() {
            if self.is_running(i) {
                self.stop(i).await;
            }
        }
    }
}

// ---- proposals ----

pub fn key(k: &str) -> KeyRange {
    KeyRange::point(k.as_bytes().to_vec())
}

pub fn prefix(k: &str) -> KeyRange {
    KeyRange::prefix(k.as_bytes().to_vec())
}

pub fn at(p: PartitionId, prefix: u64) -> Dvv {
    Dvv::new().with_prefix(p, prefix)
}

pub fn unique_key() -> Vec<u8> {
    use rand::Rng;
    let k: u128 = rand::rng().random();
    Vec::<u8>::from(format!("{k:032x}"))
}

/// A mutation reading `reads` and writing `writes` at `read_version`.
pub fn mutation(
    read_version: Dvv,
    reads: Vec<KeyRange>,
    writes: Vec<KeyRange>,
    payload: &str,
) -> Proposal {
    Proposal {
        idempotency_key: unique_key(),
        kind: ProposalKind::Mutation(Mutation {
            context: read_version.clone(),
            read_version,
            read_set: reads,
            write_set: writes,
            payload: payload.as_bytes().to_vec(),
            reply: b"ok".to_vec(),
        }),
    }
}

/// A blind write of `k` (it reads only `k`).
pub fn write(read_version: Dvv, k: &str) -> Proposal {
    mutation(read_version, vec![key(k)], vec![key(k)], &format!("w {k}"))
}

/// Proposes `p`, and when it's `TooOld` (it can be, if retries outlast the
/// validation window), re-executes it at the newest version a replica has
/// applied, under the same idempotency key, as a front end would. Only for
/// proposals whose writes don't depend on what they read.
pub async fn retry_at_newer_versions(client: &WalClient, mut p: Proposal) -> Decided {
    loop {
        match client.propose(P1, &p).await {
            Ok(d) => return d,
            Err(ProposeError::TooOld) => {
                let applied = newest_applied(client, P1).await;
                let ProposalKind::Mutation(m) = &mut p.kind else {
                    unreachable!()
                };
                m.read_version = at(P1, applied);
                m.context = m.read_version.clone();
            }
            Err(e) => panic!("{e}"),
        }
    }
}

/// The highest index any reachable replica has applied.
pub async fn newest_applied(client: &WalClient, p: PartitionId) -> u64 {
    let mut best = 0;
    for addr in client.addrs() {
        if let Ok(s) = client.node(&addr).unwrap().status(p).await {
            best = best.max(s.applied_index.unwrap_or(0));
        }
    }
    best
}

pub fn committed(d: &Decided) -> bool {
    d.status == DecidedStatus::Committed
}

// ---- reading the log ----

/// Collects `p`'s committed log until it holds `index`.
pub async fn read_log(client: &WalClient, p: PartitionId, through: u64) -> Vec<LogEntry> {
    let stream = client.tail(p, 0);
    futures::pin_mut!(stream);
    let mut out = Vec::new();
    while out.last().is_none_or(|e: &LogEntry| e.dot.index < through) {
        let e = tokio::time::timeout(Duration::from_secs(20), stream.next())
            .await
            .expect("tail stalled")
            .expect("tail ended");
        out.push(e);
    }
    out
}

/// `p`'s committed log as node `i` serves it, through `through`.
pub async fn read_node_log(
    cluster: &Cluster,
    i: usize,
    p: PartitionId,
    through: u64,
) -> Vec<LogEntry> {
    read_node_log_in(cluster, i, p, through, 1 << 20).await
}

/// Like `read_node_log`, fetching about `max_bytes` at a time.
pub async fn read_node_log_in(
    cluster: &Cluster,
    i: usize,
    p: PartitionId,
    through: u64,
    max_bytes: u32,
) -> Vec<LogEntry> {
    let node = cluster.node_client(i);
    let mut out: Vec<LogEntry> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while out.last().is_none_or(|e| e.dot.index < through) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "node {i} never served index {through}"
        );
        let next = out.last().map_or(0, |e| e.dot.index + 1);
        match node
            .fetch(p, next, max_bytes, Duration::from_millis(200))
            .await
        {
            Ok((entries, _)) => out.extend(entries),
            Err(e) => {
                tracing::debug!("fetch from node {i}: {e}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    out.truncate(through as usize + 1);
    out
}

pub fn mutations(log: &[LogEntry]) -> Vec<(Dot, &shep_wal_client::api::MutationEntry)> {
    log.iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Mutation(m) => Some((e.dot, m)),
            _ => None,
        })
        .collect()
}

// ---- a toy materializer ----

/// Applies a partition's committed log to a map of integer registers. A
/// payload is lines `r <key> <value>` (what it read) and `w <key> <value>`
/// (what it writes); the materializer applies the writes and records any read
/// that disagrees with the state the entry was applied to.
#[derive(Clone)]
pub struct Registers {
    state: Arc<Mutex<RegisterState>>,
    applied: watch::Receiver<u64>,
}

#[derive(Default)]
pub struct RegisterState {
    pub applied: u64,
    pub values: BTreeMap<String, i64>,
    /// Mutations whose reads differ from the state at their log position:
    /// a serializability violation.
    pub violations: Vec<String>,
    /// Idempotency keys of every deciding entry, in log order.
    pub keys: Vec<Vec<u8>>,
}

impl Registers {
    pub fn follow(client: &WalClient, p: PartitionId) -> Registers {
        let state = Arc::new(Mutex::new(RegisterState::default()));
        let (tx, applied) = watch::channel(0);
        let stream = client.tail(p, 0);
        let st = state.clone();
        tokio::task::spawn_local(async move {
            futures::pin_mut!(stream);
            while let Some(e) = stream.next().await {
                let mut st = st.lock().unwrap();
                match &e.kind {
                    EntryKind::Mutation(m) => {
                        st.keys.push(m.idempotency_key.clone());
                        let text = std::str::from_utf8(&m.payload).unwrap().to_string();
                        for line in text.lines() {
                            let mut parts = line.split(' ');
                            let (op, k, v) = (parts.next(), parts.next(), parts.next());
                            let (Some(op), Some(k), Some(v)) = (op, k, v) else {
                                continue;
                            };
                            let v: i64 = v.parse().unwrap();
                            match op {
                                "r" => {
                                    let cur = st.values.get(k).copied().unwrap_or(0);
                                    if cur != v {
                                        let msg =
                                            format!("{}: read {k}={v} but it was {cur}", e.dot);
                                        st.violations.push(msg);
                                    }
                                }
                                "w" => {
                                    st.values.insert(k.to_string(), v);
                                }
                                _ => {}
                            }
                        }
                    }
                    EntryKind::Failure {
                        idempotency_key, ..
                    } => st.keys.push(idempotency_key.clone()),
                    EntryKind::Internal | EntryKind::Void => {}
                }
                st.applied = e.dot.index;
                drop(st);
                tx.send_replace(e.dot.index);
            }
        });
        Registers { state, applied }
    }

    /// The applied prefix and the registers at it.
    pub fn snapshot(&self) -> (u64, BTreeMap<String, i64>) {
        let st = self.state.lock().unwrap();
        (st.applied, st.values.clone())
    }

    pub async fn wait_applied(&self, index: u64) {
        let mut rx = self.applied.clone();
        tokio::time::timeout(Duration::from_secs(20), rx.wait_for(|&a| a >= index))
            .await
            .expect("materializer stalled")
            .unwrap();
    }

    pub fn with<T>(&self, f: impl FnOnce(&RegisterState) -> T) -> T {
        f(&self.state.lock().unwrap())
    }
}
