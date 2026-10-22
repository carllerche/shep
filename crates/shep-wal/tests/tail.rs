//! Reading the log: batches stay within the message limit, and a tail moves
//! off a replica that stopped receiving entries.

mod support;

use std::time::Duration;

use futures::StreamExt;
use shep_wal_client::api::EntryKind;
use shep_wal_client::{ClientConfig, ProposeError, WalClient};
use support::*;

const LIMIT: usize = 64 << 10;

/// Entries that each fit, but not many to a message, replicate and tail in
/// batches under the limit.
#[tokio::test(flavor = "multi_thread")]
async fn batches_respect_the_message_limit() {
    let mut cluster = Cluster::start_with(3, &[P1], |c| c.max_message_size = LIMIT).await;
    let small = WalClient::with_config(
        cluster.client.addrs(),
        ClientConfig {
            max_message_size: LIMIT,
            ..ClientConfig::default()
        },
    )
    .unwrap();
    // A follower that misses the writes catches up in batches of many.
    let lagging = (cluster.leader(P1).await + 1) % 3;
    cluster.stop(lagging).await;
    let payload = "x".repeat(20 << 10);
    let mut last = 0;
    for i in 0..30 {
        let p = mutation(at(P1, last), vec![], vec![key(&format!("k{i}"))], &payload);
        last = small.propose(P1, &p).await.unwrap().dot.index;
    }
    cluster.restart(lagging).await;
    let log = read_log(&small, P1, last).await;
    let big = log
        .iter()
        .filter(|e| matches!(&e.kind, EntryKind::Mutation(m) if m.payload.len() == payload.len()));
    assert_eq!(big.count(), 30);
    // Every replica got them, through bounded AppendEntries.
    for i in 0..3 {
        read_node_log(&cluster, i, P1, last).await;
    }

    // An entry that couldn't fit in half a message is refused.
    let huge = mutation(
        at(P1, last),
        vec![],
        vec![key("huge")],
        &"x".repeat(40 << 10),
    );
    assert!(matches!(
        small.propose(P1, &huge).await,
        Err(ProposeError::Invalid(_))
    ));
    cluster.shutdown().await;
}

/// A tail reading a follower that's cut off from its leader moves to a
/// replica that has the newer entries.
#[tokio::test(flavor = "multi_thread")]
async fn a_tail_leaves_a_replica_that_fell_behind() {
    let cluster = Cluster::start(3, &[P1]).await;
    let first = cluster
        .client
        .propose(P1, &write(at(P1, 0), "a"))
        .await
        .unwrap();
    let leader = cluster.leader(P1).await;
    let follower = (leader + 1) % 3;
    read_node_log(&cluster, follower, P1, first.dot.index).await;

    // Tail the follower first.
    let mut addrs = vec![cluster.addr(follower)];
    addrs.extend((0..3).filter(|&i| i != follower).map(|i| cluster.addr(i)));
    let client = WalClient::with_config(
        addrs,
        ClientConfig {
            tail_idle: Duration::from_millis(300),
            ..ClientConfig::default()
        },
    )
    .unwrap();
    let stream = client.tail(P1, 0);
    futures::pin_mut!(stream);
    while stream.next().await.unwrap().dot.index < first.dot.index {}

    cluster.node(follower).faults().set_partitioned(true);
    let second = cluster
        .client
        .propose(P1, &write(at(P1, first.dot.index), "b"))
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let e = stream.next().await.unwrap();
            if e.dot.index == second.dot.index {
                return e;
            }
        }
    })
    .await
    .expect("the tail stayed on the cut-off replica");
    assert!(matches!(got.kind, EntryKind::Mutation(_)));
    cluster.shutdown().await;
}
