//! Runs `shep-node` processes for end-to-end tests: real processes on
//! loopback, stopped with SIGTERM or killed with SIGKILL.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use shep_wal_client::api::{Dvv, KeyRange, Mutation, PartitionId, Proposal, ProposalKind};
use shep_wal_client::{ClientConfig, WalClient};
use tokio::process::{Child, Command};

/// The `shep-node` binary, built once per test run.
pub fn binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "shep-node", "--bin", "shep-node"])
            .current_dir(&root)
            .status()
            .expect("running cargo build");
        assert!(status.success(), "building shep-node failed");
        let target = std::env::var("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| root.join("target"));
        target.join("debug").join("shep-node")
    })
}

/// A free loopback address. Bound and released, so another process could
/// take it first; good enough for tests.
pub fn free_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

/// One node of a test cluster: its address and data directory survive its
/// process, so it can be restarted.
pub struct NodeSpec {
    pub id: u64,
    pub addr: SocketAddr,
    pub dir: tempfile::TempDir,
}

pub struct Process {
    child: Child,
}

impl Process {
    /// Starts `shep-node` for `node`, with `seeds` as `(id, addr)`.
    pub fn start(node: &NodeSpec, seeds: &[(u64, SocketAddr)], cores: usize) -> Process {
        Process::start_in(node, node.dir.path(), seeds, cores)
    }

    /// Like [`Process::start`], on data directory `dir`.
    pub fn start_in(
        node: &NodeSpec,
        dir: &Path,
        seeds: &[(u64, SocketAddr)],
        cores: usize,
    ) -> Process {
        let mut cmd = Command::new(binary());
        cmd.args(["--id", &node.id.to_string()])
            .args(["--listen", &node.addr.to_string()])
            .arg("--data-dir")
            .arg(dir)
            .args(["--cores", &cores.to_string()]);
        for (id, addr) in seeds {
            cmd.args(["--seed", &format!("{id}={addr}")]);
        }
        let quiet = std::env::var("SHEP_TEST_NODE_LOGS").is_err();
        if quiet {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        cmd.kill_on_drop(true);
        Process {
            child: cmd.spawn().expect("spawning shep-node"),
        }
    }

    /// SIGTERM, then wait for a clean exit.
    pub async fn stop(mut self) -> std::process::ExitStatus {
        let pid = self.child.id().expect("running");
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        tokio::time::timeout(Duration::from_secs(20), self.child.wait())
            .await
            .expect("shep-node didn't exit after SIGTERM")
            .unwrap()
    }

    /// SIGKILL: a crash.
    pub async fn kill(mut self) {
        self.child.kill().await.unwrap();
    }

    /// Waits up to `timeout` for the process to exit on its own.
    pub async fn wait(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        tokio::time::timeout(timeout, self.child.wait())
            .await
            .ok()
            .map(Result::unwrap)
    }
}

pub fn client(addrs: impl IntoIterator<Item = SocketAddr>) -> WalClient {
    WalClient::with_config(
        addrs.into_iter().map(|a| a.to_string()),
        ClientConfig {
            propose_deadline: Duration::from_secs(30),
            ..ClientConfig::default()
        },
    )
    .unwrap()
}

/// Waits until some node reports the cluster founded.
pub async fn founded(client: &WalClient) -> Bytes {
    for _ in 0..600 {
        if let Ok(s) = client.controller_state().await
            && let Some(id) = s.cluster_id
        {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never founded");
}

pub fn unique_key() -> Bytes {
    Bytes::from(format!("{:032x}", rand::random::<u128>()))
}

/// A blind write of `key` to `p`, read at `read_version`.
pub fn write(p: PartitionId, read_version: u64, key: &str, payload: &str) -> Proposal {
    let read_version = Dvv::new().with_prefix(p, read_version);
    let k = KeyRange::point(Bytes::copy_from_slice(key.as_bytes()));
    Proposal {
        idempotency_key: unique_key(),
        kind: ProposalKind::Mutation(Mutation {
            context: read_version.clone(),
            read_version,
            read_set: vec![k.clone()],
            write_set: vec![k],
            payload: Bytes::copy_from_slice(payload.as_bytes()),
            reply: Bytes::from_static(b"ok"),
        }),
    }
}

/// The highest index any node reports applied for `p`.
pub async fn applied(client: &WalClient, p: PartitionId) -> u64 {
    let mut best = 0;
    for addr in client.addrs() {
        if let Ok(s) = client.node(&addr).unwrap().status(p).await {
            best = best.max(s.applied_index.unwrap_or(0));
        }
    }
    best
}
