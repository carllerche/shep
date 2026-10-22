//! Durability: the log, votes, and idempotency outcomes survive restarts, and
//! a replica recovers from a torn write and catches up.

mod support;

use std::io::Write;

use shep_wal_client::api::{Decided, LogEntry, Proposal};
use support::*;

async fn commit_n(cluster: &Cluster, n: usize) -> Vec<(Proposal, Decided)> {
    let mut out = Vec::new();
    let mut last = newest_applied(&cluster.client, P1).await;
    for _ in 0..n {
        let p = write(at(P1, last), &format!("k{}", unique_key().escape_ascii()));
        let d = cluster.client.propose(P1, &p).await.unwrap();
        last = d.dot.index;
        out.push((p, d));
    }
    out
}

/// Entries up to `through`, with Raft's own entries dropped: a restart
/// appends new leaders' no-ops after them, but never changes what's before.
fn same_prefix(a: &[LogEntry], b: &[LogEntry], through: u64) {
    let a: Vec<_> = a.iter().filter(|e| e.dot.index <= through).collect();
    let b: Vec<_> = b.iter().filter(|e| e.dot.index <= through).collect();
    assert_eq!(a, b);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_log_survives_a_full_restart() {
    let mut cluster = Cluster::start(3, &[P1]).await;
    let committed = commit_n(&cluster, 40).await;
    let last = committed.last().unwrap().1.dot.index;
    let before = read_log(&cluster.client, P1, last).await;

    for i in 0..3 {
        cluster.stop(i).await;
    }
    for i in 0..3 {
        cluster.restart(i).await;
    }
    cluster.leader(P1).await;

    // Re-sent proposals get their original outcomes.
    for (p, d) in committed.iter().step_by(7) {
        assert_eq!(&cluster.client.propose(P1, p).await.unwrap(), d);
    }
    let next = cluster
        .client
        .propose(P1, &write(at(P1, last), "after"))
        .await
        .unwrap();
    assert!(next.dot.index > last);
    for i in 0..3 {
        same_prefix(&before, &read_node_log(&cluster, i, P1, last).await, last);
    }
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lagging_replica_catches_up() {
    let mut cluster = Cluster::start(3, &[P1]).await;
    commit_n(&cluster, 5).await;
    let leader = cluster.leader(P1).await;
    let follower = (leader + 1) % 3;
    cluster.stop(follower).await;
    let committed = commit_n(&cluster, 60).await;
    let last = committed.last().unwrap().1.dot.index;
    cluster.restart(follower).await;

    let expected = read_log(&cluster.client, P1, last).await;
    same_prefix(
        &expected,
        &read_node_log(&cluster, follower, P1, last).await,
        last,
    );
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_torn_write_is_truncated_on_recovery() {
    let mut cluster = Cluster::start(3, &[P1]).await;
    let committed = commit_n(&cluster, 20).await;
    let last = committed.last().unwrap().1.dot.index;
    let expected = read_log(&cluster.client, P1, last).await;

    let leader = cluster.leader(P1).await;
    let victim = (leader + 1) % 3;
    read_node_log(&cluster, victim, P1, last).await;
    cluster.stop(victim).await;
    // A record whose header promises more than was written.
    let seg = std::fs::read_dir(cluster.replica_dir(victim, P1))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .max()
        .unwrap();
    let len = std::fs::metadata(&seg).unwrap().len();
    let mut f = std::fs::OpenOptions::new().append(true).open(&seg).unwrap();
    f.write_all(&[200, 0, 0, 0, 1, 2, 3, 4, 5, 6]).unwrap();
    drop(f);

    cluster.restart(victim).await;
    assert_eq!(
        std::fs::metadata(&seg).unwrap().len(),
        len,
        "the torn record is gone"
    );
    let more = commit_n(&cluster, 5).await;
    let now = more.last().unwrap().1.dot.index;
    same_prefix(
        &expected,
        &read_node_log(&cluster, victim, P1, now).await,
        last,
    );
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn segments_roll_over() {
    let mut cluster =
        Cluster::start_with(3, &[P1], |c| c.raft.storage.log.segment_size = 512).await;
    let committed = commit_n(&cluster, 60).await;
    let last = committed.last().unwrap().1.dot.index;
    let before = read_log(&cluster.client, P1, last).await;
    let segments = std::fs::read_dir(cluster.replica_dir(0, P1))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "log")
        })
        .count();
    assert!(segments > 5, "{segments} segments");

    for i in 0..3 {
        cluster.stop(i).await;
        cluster.restart(i).await;
    }
    cluster.leader(P1).await;
    for i in 0..3 {
        same_prefix(&before, &read_node_log(&cluster, i, P1, last).await, last);
    }
    cluster.shutdown().await;
}
