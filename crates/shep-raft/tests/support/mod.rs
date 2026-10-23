//! Raft nodes in this process, each on its own cores, over loopback.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use shep_raft::{
    Admit, Decided, Entry, GroupId, GroupStatus, Machine, NodeId, ProposeError, RaftConfig,
    RaftNode, Resolution,
};
use shep_rpc_client::ReconnectConfig;
use shep_smp::{Smp, SmpConfig};
use tempfile::TempDir;
use tokio::net::TcpListener;

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// Records every committed payload, and every leader-side call; answers each
/// payload with its position. It admits anything but `b"refuse"`.
#[derive(Default)]
pub struct Recorder {
    pub entries: Vec<Vec<u8>>,
    pub calls: Vec<Call>,
}

/// A leader-side call a [`Recorder`] saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Lead { term: u64, floor: u64 },
    Replay(u64, Vec<u8>),
    Admit(u64, Vec<u8>),
    Resolve(u64, Resolution),
}

impl Machine for Recorder {
    type Proposal = Vec<u8>;
    type Command = Vec<u8>;
    type Rejection = String;
    type Response = u64;

    fn admit(&mut self, admit: &Admit, proposal: Vec<u8>) -> Result<Vec<u8>, String> {
        self.calls.push(Call::Admit(admit.seq, proposal.clone()));
        if proposal == b"refuse" {
            return Err("refused".into());
        }
        Ok(proposal)
    }

    fn lead(&mut self, term: u64, floor: u64) {
        self.calls.push(Call::Lead { term, floor });
    }

    fn replay(&mut self, entry: &Entry<Vec<u8>>) {
        self.calls
            .push(Call::Replay(entry.index, entry.command.clone()));
    }

    fn resolve(&mut self, seq: u64, resolution: Resolution) {
        self.calls.push(Call::Resolve(seq, resolution));
    }

    fn apply(&mut self, entry: &Entry<Vec<u8>>) -> u64 {
        self.entries.push(entry.command.clone());
        self.entries.len() as u64
    }

    fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for e in &self.entries {
            out.extend_from_slice(&(e.len() as u32).to_le_bytes());
            out.extend_from_slice(e);
        }
        out
    }

    fn restore(&mut self, snapshot: Vec<u8>) {
        self.entries.clear();
        let mut b = &snapshot[..];
        while b.len() >= 4 {
            let n = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
            self.entries.push(b[4..4 + n].to_vec());
            b = &b[4 + n..];
        }
    }
}

pub struct TestNode {
    pub node: RaftNode<Recorder>,
    pub smp: Smp,
}

pub struct Cluster {
    nodes: Vec<Option<TestNode>>,
    dirs: Vec<TempDir>,
    addrs: Vec<SocketAddr>,
    cores: usize,
    configure: fn(&mut RaftConfig),
}

pub fn test_config(id: NodeId, dir: &std::path::Path) -> RaftConfig {
    let mut c = RaftConfig::new(id, dir);
    c.heartbeat_interval = Duration::from_millis(100);
    c.election_timeout = Duration::from_millis(600)..Duration::from_millis(1200);
    c.reconnect = ReconnectConfig {
        initial_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(200),
        connect_timeout: Duration::from_millis(500),
        ..ReconnectConfig::default()
    };
    c
}

impl Cluster {
    /// `n` nodes with `cores` cores each.
    pub async fn start(n: usize, cores: usize) -> Cluster {
        Cluster::start_with(n, cores, |_| {}).await
    }

    /// Like [`Cluster::start`], with `configure` adjusting each node's config.
    pub async fn start_with(n: usize, cores: usize, configure: fn(&mut RaftConfig)) -> Cluster {
        init_tracing();
        let mut listeners = Vec::new();
        let mut addrs = Vec::new();
        for _ in 0..n {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(l.local_addr().unwrap());
            listeners.push(l);
        }
        let mut cluster = Cluster {
            nodes: Vec::new(),
            dirs: (0..n).map(|_| tempfile::tempdir().unwrap()).collect(),
            addrs,
            cores,
            configure,
        };
        for (i, l) in listeners.into_iter().enumerate() {
            let node = cluster.open(i).await;
            node.node.serve(l);
            cluster.nodes.push(Some(node));
        }
        cluster
    }

    async fn open(&self, i: usize) -> TestNode {
        let smp = Smp::start(SmpConfig {
            pin: false,
            ..SmpConfig::with_cores(self.cores)
        })
        .unwrap();
        let mut config = test_config(i as u64 + 1, self.dirs[i].path());
        (self.configure)(&mut config);
        let node = RaftNode::start(config, smp.clone(), |_| Recorder::default())
            .await
            .unwrap();
        TestNode { node, smp }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn node(&self, i: usize) -> &RaftNode<Recorder> {
        &self.nodes[i].as_ref().expect("node is stopped").node
    }

    pub fn is_running(&self, i: usize) -> bool {
        self.nodes[i].is_some()
    }

    pub fn members(&self) -> BTreeMap<NodeId, String> {
        (0..self.len())
            .map(|i| (i as u64 + 1, self.addrs[i].to_string()))
            .collect()
    }

    /// Creates `group` on every running node and initializes it from the
    /// first, then waits for a leader.
    pub async fn create_group(&self, group: GroupId) {
        for i in (0..self.len()).filter(|&i| self.is_running(i)) {
            self.node(i).create_group(group).await.unwrap();
        }
        let first = (0..self.len()).find(|&i| self.is_running(i)).unwrap();
        self.node(first)
            .initialize_group(group, self.members())
            .await
            .unwrap();
        self.leader(group).await;
    }

    /// The index of `group`'s leader, once every running node agrees on it.
    pub async fn leader(&self, group: GroupId) -> usize {
        for _ in 0..1500 {
            let mut views = Vec::new();
            for i in (0..self.len()).filter(|&i| self.is_running(i)) {
                views.push(self.node(i).status(group).await.ok().and_then(|s| s.leader));
            }
            if let Some(&Some(id)) = views.first()
                && views.iter().all(|v| *v == Some(id))
            {
                let i = id as usize - 1;
                if self.is_running(i) && self.node(i).status(group).await.is_ok_and(|s| s.is_leader)
                {
                    return i;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no leader for {group}");
    }

    pub async fn status(&self, i: usize, group: GroupId) -> GroupStatus {
        self.node(i).status(group).await.unwrap()
    }

    /// Writes through the current leader, following redirects.
    pub async fn write(&self, group: GroupId, payload: impl Into<Vec<u8>>) -> Decided<u64> {
        let payload = payload.into();
        for _ in 0..200 {
            let leader = self.leader(group).await;
            match self
                .node(leader)
                .propose(group, None, payload.clone())
                .await
            {
                Ok(w) => return w,
                Err(ProposeError::NotLeader { .. }) => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(e) => panic!("write: {e}"),
            }
        }
        panic!("write never committed");
    }

    /// Node `i`'s machine's leader-side calls for `group`.
    pub async fn calls(&self, i: usize, group: GroupId) -> Vec<Call> {
        self.node(i)
            .with_state_machine(group, |sm| sm.calls.clone())
            .await
            .unwrap()
    }

    /// Node `i`'s applied entries of `group`, once it has applied `n`.
    pub async fn applied(&self, i: usize, group: GroupId, n: usize) -> Vec<Vec<u8>> {
        for _ in 0..1500 {
            let entries = self
                .node(i)
                .with_state_machine(group, |sm| sm.entries.clone())
                .await
                .unwrap();
            if entries.len() >= n {
                return entries;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("node {i} never applied {n} entries of {group}");
    }

    pub async fn stop(&mut self, i: usize) {
        let n = self.nodes[i].take().expect("already stopped");
        n.node.shutdown().await;
        let smp = n.smp.clone();
        let (done, finished) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            smp.shutdown();
            let _ = done.send(());
        });
        let _ = finished.await;
    }

    pub async fn restart(&mut self, i: usize) {
        assert!(self.nodes[i].is_none());
        let node = self.open(i).await;
        let mut tries = 0;
        let listener = loop {
            match TcpListener::bind(self.addrs[i]).await {
                Ok(l) => break l,
                Err(e) if tries < 100 => {
                    tries += 1;
                    tracing::debug!("rebinding: {e}");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("rebinding {}: {e}", self.addrs[i]),
            }
        };
        node.node.serve(listener);
        self.nodes[i] = Some(node);
    }

    pub async fn shutdown(mut self) {
        for i in 0..self.len() {
            if self.is_running(i) {
                self.stop(i).await;
            }
        }
    }
}
