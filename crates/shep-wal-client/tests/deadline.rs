use std::time::{Duration, Instant};

use bytes::Bytes;
use shep_wal_client::api::{PartitionId, Proposal, ProposalKind};
use shep_wal_client::{ClientConfig, ProposeError, WalClient};

/// A peer that accepts connections and never answers can't hold `propose`
/// past its deadline.
#[tokio::test]
async fn propose_stops_at_its_deadline() {
    // Bound and listening, so connections complete, but never served.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
