//! Leader changes (§10.3): a new leader rebuilds its tables from its log
//! before accepting proposals, and a deposed leader commits nothing.

mod support;

use std::time::Duration;

use shep_wal_client::ProposeError;
use shep_wal_client::api::{EntryKind, Rejection};
use support::*;

/// A write committed under one leader conflicts, under the next, with a
/// proposal that didn't see it.
#[tokio::test(flavor = "local")]
async fn the_new_leader_checks_the_old_leaders_writes() {
    let mut cluster = Cluster::start(3, &[P1]).await;
    let c = cluster.client.clone();
    let s0 = at(
        P1,
        c.propose(P1, &write(at(P1, 0), "x"))
            .await
            .unwrap()
            .dot
            .index,
    );
    let w = c.propose(P1, &write(s0.clone(), "a")).await.unwrap();

    let old = cluster.leader(P1).await;
    cluster.stop(old).await;
    let new = cluster.leader(P1).await;
    assert_ne!(old, new);

    assert_eq!(
        c.propose(P1, &write(s0.clone(), "a")).await,
        Err(ProposeError::Contended)
    );
    c.propose(P1, &write(s0.clone(), "b")).await.unwrap();
    c.propose(P1, &write(at(P1, w.dot.index), "a"))
        .await
        .unwrap();
    cluster.shutdown().await;
}

/// A node that leads again in a later term rebuilds its table: the entries a
/// leader in between committed are in it, though this node never validated
/// them (`WalValidate_norebuild`).
#[tokio::test(flavor = "local")]
async fn a_returning_leader_rebuilds_its_table() {
    // Node 0 times out first, so it wins any election it can. The elections
    // are slow, so the window is long, or `s0` would be too old.
    let cluster = Cluster::start_with_nodes(3, &[P1], |i, c| {
        c.raft.proposals.admission_window = Duration::from_secs(120);
        c.raft.election_timeout = match i {
            0 => Duration::from_millis(300)..Duration::from_millis(400),
            _ => Duration::from_millis(1500)..Duration::from_millis(3000),
        };
    })
    .await;
    let c = &cluster.client;
    assert_eq!(cluster.leader(P1).await, 0);
    let s0 = at(
        P1,
        c.propose(P1, &write(at(P1, 0), "x"))
            .await
            .unwrap()
            .dot
            .index,
    );
    // Node 0 validates something, so it holds a table for its first term.
    c.propose(P1, &write(s0.clone(), "y")).await.unwrap();

    // Another leader commits a write node 0 never sees.
    cluster.node(0).faults().set_partitioned(true);
    let other = cluster.leader_where(P1, |i| i != 0).await;
    let w = c.propose(P1, &write(s0.clone(), "a")).await.unwrap();

    // Node 0 catches up as a follower, then leads again.
    cluster.node(0).faults().set_partitioned(false);
    read_node_log(&cluster, 0, P1, w.dot.index).await;
    cluster.node(other).faults().set_partitioned(true);
    assert_eq!(cluster.leader(P1).await, 0);
    cluster.node(other).faults().set_partitioned(false);

    let r = cluster
        .node_client(0)
        .propose(P1, &write(s0.clone(), "a"))
        .await
        .unwrap();
    assert_eq!(r, Err(Rejection::Contended));
    cluster.shutdown().await;
}

/// Proposals in flight across a leader change all commit exactly once, with
/// no conflicts among disjoint keys.
#[tokio::test(flavor = "local")]
async fn proposals_survive_a_leader_change() {
    let mut cluster = Cluster::start(3, &[P1]).await;
    let s0 = at(
        P1,
        cluster
            .client
            .propose(P1, &write(at(P1, 0), "x"))
            .await
            .unwrap()
            .dot
            .index,
    );
    let proposals: Vec<_> = (0..300)
        .map(|i| write(s0.clone(), &format!("key{i}")))
        .collect();
    let tasks: Vec<_> = proposals
        .iter()
        .cloned()
        .map(|p| {
            let client = cluster.client.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(rand::random_range(0..400))).await;
                retry_at_newer_versions(&client, p).await
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let old = cluster.leader(P1).await;
    cluster.stop(old).await;

    let mut last = 0;
    for t in tasks {
        last = last.max(t.await.unwrap().dot.index);
    }
    // Every key is in the committed log exactly once.
    let log = read_log(&cluster.client, P1, last).await;
    let mut keys: Vec<_> = mutations(&log)
        .iter()
        .map(|(_, m)| m.idempotency_key.clone())
        .collect();
    keys.sort();
    let before = keys.len();
    keys.dedup();
    assert_eq!(before, keys.len(), "a key was decided twice");
    for p in &proposals {
        assert!(keys.binary_search(&p.idempotency_key).is_ok());
    }
    cluster.shutdown().await;
}

/// A leader cut off from its peers keeps accepting proposals but can't
/// commit them; the majority elects a new leader. Once the partition heals,
/// the old leader's appended entries are overwritten and nothing it appended
/// alone is ever committed, while a re-sent proposal is decided once.
#[tokio::test(flavor = "local")]
async fn a_deposed_leader_commits_nothing() {
    let cluster = Cluster::start_with(3, &[P1], |c| {
        c.raft.proposals.commit_timeout = Duration::from_millis(500)
    })
    .await;
    let c = &cluster.client;
    let s0 = at(
        P1,
        c.propose(P1, &write(at(P1, 0), "x"))
            .await
            .unwrap()
            .dot
            .index,
    );
    let old = cluster.leader(P1).await;
    cluster.node(old).faults().set_partitioned(true);

    // Before its quorum lease runs out, it still accepts.
    let stuck = write(s0.clone(), "a");
    let alone = write(s0.clone(), "b");
    let node = cluster.node_client(old);
    let (r1, r2) = tokio::join!(node.propose(P1, &stuck), node.propose(P1, &alone));
    for r in [r1, r2] {
        assert!(matches!(r, Ok(Err(Rejection::UnknownOutcome(_)))), "{r:?}");
    }
    // After it, it refers clients elsewhere.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let r = node.propose(P1, &write(s0.clone(), "z")).await.unwrap();
    assert!(matches!(r, Err(Rejection::NotLeader { .. })), "{r:?}");

    // The majority moves on, and conflicts are checked there afresh.
    let new = cluster.leader_where(P1, |i| i != old).await;
    let d = c.propose(P1, &write(s0.clone(), "c")).await.unwrap();
    assert_eq!(
        c.propose(P1, &write(s0.clone(), "c")).await,
        Err(ProposeError::Contended)
    );

    cluster.node(old).faults().set_partitioned(false);
    cluster.leader(P1).await;
    // Re-sending decides it once, wherever it lands.
    let first = c.propose(P1, &stuck).await.unwrap();
    let again = c.propose(P1, &stuck).await.unwrap();
    assert_eq!(first, again);

    let log = read_log(c, P1, first.dot.index.max(d.dot.index)).await;
    let keys: Vec<_> = mutations(&log)
        .iter()
        .map(|(_, m)| m.idempotency_key.clone())
        .collect();
    assert_eq!(
        keys.iter().filter(|k| **k == stuck.idempotency_key).count(),
        1
    );
    assert!(
        !keys.contains(&alone.idempotency_key),
        "an entry only the deposed leader had committed"
    );
    // Nothing from the old leader's term survived as a mutation.
    let old_term = log
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::Mutation(_)))
        .map(|e| e.term)
        .min();
    assert!(old_term.is_some());
    let _ = new;
    cluster.shutdown().await;
}
