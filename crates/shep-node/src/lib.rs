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

use shep_rpc_server::{Request, Response, Server, service_fn};
use shep_smp::SmpConfig;
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
    wal: WalNode,
    node_id: u64,
    /// Bound by `start`, accepted on by `serve`.
    listener: Option<std::net::TcpListener>,
    shutdown: watch::Sender<bool>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl Node {
    /// Binds `config.listen`, starts the cores, and opens storage. Blocks
    /// until the node is set up; nothing is accepted until [`Node::serve`].
    /// The caller needn't be async or run a runtime.
    pub fn start(config: NodeConfig) -> Result<Node, NodeError> {
        let wal_config = config.wal();
        if !wal_config.seeds.iter().any(|s| s.node_id == config.node_id) {
            return Err(NodeError::NotASeed(config.node_id));
        }
        // Bound first, so a port in use fails before storage recovery runs.
        // Peers connecting meanwhile wait in the backlog.
        let listener = std::net::TcpListener::bind(config.listen)?;
        listener.set_nonblocking(true)?;
        let smp_config = match config.cores {
            Some(n) => SmpConfig::with_cores(n),
            None => SmpConfig::new(),
        };
        let cores = smp_config.cores;
        let wal = WalNode::start(wal_config, smp_config)?;
        tracing::info!(node = config.node_id, listen = %config.listen, cores, "started");
        Ok(Node {
            wal,
            node_id: config.node_id,
            listener: Some(listener),
            shutdown: watch::channel(false).0,
            server: None,
        })
    }

    /// Accepts on the listener and runs the controller's background work,
    /// until [`Node::shutdown`]. Call it once, on a thread running a
    /// [`LocalRuntime`](tokio::runtime::LocalRuntime).
    pub fn serve(&mut self) -> Result<(), NodeError> {
        let listener = self.listener.take().expect("Node::serve is called once");
        let listener = TcpListener::from_std(listener)?;
        let server = Server::new(self.wal.config().raft.server.clone(), {
            let wal = self.wal.clone();
            service_fn(move |req: Request| {
                let wal = wal.clone();
                async move {
                    match Service::of(req.method) {
                        Some(Service::Wal) => wal.handle(req).await,
                        None => Response::error(shep_wal_api::status::UNKNOWN_METHOD),
                    }
                }
            })
        });
        let mut stop = self.shutdown.subscribe();
        self.server = Some(tokio::task::spawn_local(async move {
            tokio::select! {
                _ = server.serve(listener) => {}
                _ = async { let _ = stop.wait_for(|s| *s).await; } => {
                    server.shutdown(Duration::from_millis(500)).await
                }
            }
        }));
        self.wal.run_controller();
        tracing::info!(node = self.node_id, "serving");
        Ok(())
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
    }
}
