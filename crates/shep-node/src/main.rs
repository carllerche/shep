//! `shep-node`: runs a Shep node until interrupted.
//!
//! ```text
//! shep-node --id 1 --listen 127.0.0.1:7001 --data-dir ./data/1 \
//!     [--seed 1=127.0.0.1:7001 --seed 2=127.0.0.1:7002 ...] [--cores N]
//! ```
//!
//! Without `--seed`, the node founds a cluster of its own.

use std::process::ExitCode;

use shep_node::{Node, NodeConfig};
use shep_wal_api::Member;

const USAGE: &str = "usage: shep-node --id <u64> --listen <addr:port> --data-dir <path> [--seed <id>=<addr:port>]... [--cores <n>]";

fn parse(args: impl Iterator<Item = String>) -> Result<NodeConfig, String> {
    let (mut id, mut listen, mut data_dir, mut seeds, mut cores) =
        (None, None, None, Vec::new(), None);
    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--id" => id = Some(value()?.parse::<u64>().map_err(|e| format!("--id: {e}"))?),
            "--listen" => listen = Some(value()?.parse().map_err(|e| format!("--listen: {e}"))?),
            "--data-dir" => data_dir = Some(value()?.into()),
            "--cores" => {
                cores = Some(
                    value()?
                        .parse::<usize>()
                        .map_err(|e| format!("--cores: {e}"))?,
                )
            }
            "--seed" => {
                let v = value()?;
                let (id, addr) = v
                    .split_once('=')
                    .ok_or(format!("--seed {v}: expected <id>=<addr:port>"))?;
                let node_id = id.parse::<u64>().map_err(|e| format!("--seed {v}: {e}"))?;
                seeds.push(Member {
                    node_id,
                    addr: addr.to_string(),
                });
            }
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    Ok(NodeConfig {
        node_id: id.ok_or(format!("--id is required\n{USAGE}"))?,
        listen: listen.ok_or(format!("--listen is required\n{USAGE}"))?,
        data_dir: data_dir.ok_or(format!("--data-dir is required\n{USAGE}"))?,
        seeds,
        cores,
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let config = match parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let node = match Node::start(config).await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("starting: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    node.shutdown().await;
    ExitCode::SUCCESS
}
