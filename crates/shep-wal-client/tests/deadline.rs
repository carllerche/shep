use std::time::{Duration, Instant};

use bytes::Bytes;
use shep_wal_client::api::{PartitionId, Proposal, ProposalKind};
use shep_wal_client::{ClientConfig, ProposeError, WalClient};

/// A client of one peer that accepts connections and never answers, with a
/// deadline far shorter than its RPC timeout and backoff.
fn silent_peer() -> (std::net::TcpListener, WalClient) {
    // Bound and listening, so connections complete, but never served.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = silent.local_addr().unwrap().to_string();
    let client = WalClient::with_config(
        [addr],
        ClientConfig {
            propose_deadline: Duration::from_millis(30),
            rpc_timeout: Duration::from_millis(400),
            retry_backoff: Duration::from_millis(400),
            ..ClientConfig::default()
        },
    )
    .unwrap();
    (silent, client)
}

/// A silent peer can't hold `propose` past its deadline.
#[tokio::test(flavor = "local")]
async fn propose_stops_at_its_deadline() {
    let (silent, client) = silent_peer();
    let proposal = Proposal {
        idempotency_key: Bytes::from_static(b"k"),
        kind: ProposalKind::Failure {
            reply: Bytes::new(),
        },
    };
    let start = Instant::now();
    let r = client.propose(PartitionId(1), &proposal).await;
    assert!(matches!(r, Err(ProposeError::Unavailable(_))), "{r:?}");
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "took {:?}",
        start.elapsed()
    );
    drop(silent);
}

/// Nor `create_universe`, which has the same deadline.
#[tokio::test(flavor = "local")]
async fn create_universe_stops_at_its_deadline() {
    let (silent, client) = silent_peer();
    let start = Instant::now();
    let r = client.create_universe("prod", 1, 1).await;
    assert!(r.is_err(), "{r:?}");
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "took {:?}",
        start.elapsed()
    );
    drop(silent);
}

/// A client with no nodes fails rather than panicking.
#[tokio::test(flavor = "local")]
async fn create_universe_without_nodes_fails() {
    let client = WalClient::with_config(Vec::<String>::new(), ClientConfig::default()).unwrap();
    assert!(client.create_universe("prod", 1, 1).await.is_err());
}
