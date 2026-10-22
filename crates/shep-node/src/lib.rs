//! A Shep node: every service the process runs, on one set of cores and one
//! shep-rpc address (design §9.6, §17).
//!
//! Today that's the WAL, with its controller. A single node is a complete,
//! real deployment, just without replication: it founds a cluster of one.
//! Further services (materializers, front ends) join here, each taking its
//! own shep-rpc service IDs.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use shep_rpc::{Request, Response, Server, Status};
use shep_smp::{Smp, SmpConfig};
use shep_wal::{WalConfig, WalNode};
use shep_wal_api::Member;
use tokio::net::TcpListener;
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub node_id: u64,
    /// Where peers and clients reach this node.
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    /// The cluster's founding members. Empty means this node alone.
    pub seeds: Vec<Member>,
    /// Cores to run on; all of the machine's by default.
    pub cores: Option<usize>,
}

impl NodeConfig {
    fn wal(&self) -> WalConfig {
        let mut wal = WalConfig::new(self.node_id, &self.data_dir);
        wal.seeds = if self.seeds.is_empty() {
            vec![Member {
                node_id: self.node_id,
                addr: self.listen.to_string(),
            }]
        } else {
            self.seeds.clone()
        };
        wal
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Wal(#[from] shep_wal::WalError),
    #[error("node {0} isn't in the seed list")]
    NotASeed(u64),
}

/// The services this process runs. Each RPC goes to the service whose ID its
/// method carries.
enum Service {
    Wal,
}

impl Service {
    fn of(method: u32) -> Option<Service> {
        // The WAL node serves Raft (1), the WAL (2) and its controller (3).
        match method >> 16 {
            1..=3 => Some(Service::Wal),
            _ => None,
        }
    }
}

/// A running node.
pub struct Node {
    smp: Smp,
    wal: WalNode,
    shutdown: watch::Sender<bool>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl Node {
    /// Starts the cores, opens storage, and serves on `config.listen`.
    pub async fn start(config: NodeConfig) -> Result<Node, NodeError> {
        let wal_config = config.wal();
        if !wal_config.seeds.iter().any(|s| s.node_id == config.node_id) {
            return Err(NodeError::NotASeed(config.node_id));
        }
        let listener = TcpListener::bind(config.listen).await?;
        let smp_config = match config.cores {
            Some(n) => SmpConfig::with_cores(n),
            None => SmpConfig::new(),
        };
        let smp = Smp::start(smp_config)?;
        let wal = WalNode::start(wal_config.clone(), smp.clone()).await?;
        let server = Server::new(wal_config.raft.server.clone(), {
            let wal = wal.clone();
            move |req: Request| {
                let wal = wal.clone();
                async move {
                    match Service::of(req.method) {
                        Some(Service::Wal) => wal.handle(req).await,
                        None => Response::error(Status::MethodNotFound),
                    }
                }
            }
        });
        let (shutdown, mut stop) = watch::channel(false);
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = server.serve(listener) => {}
                _ = async { let _ = stop.wait_for(|s| *s).await; } => {
                    server.shutdown(Duration::from_millis(500)).await
                }
            }
        });
        tracing::info!(node = config.node_id, listen = %config.listen, cores = smp.cores(), "started");
        Ok(Node {
            smp,
            wal,
            shutdown,
            server: Some(task),
        })
    }

    pub fn wal(&self) -> &WalNode {
        &self.wal
    }

    /// Stops serving, closes every log cleanly, and stops the cores.
    pub async fn shutdown(mut self) {
        self.shutdown.send_replace(true);
        if let Some(server) = self.server.take() {
            let _ = server.await;
        }
        self.wal.shutdown().await;
        self.smp.shutdown().await;
    }
}
