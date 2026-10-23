mod support;

use std::time::Duration;

use shep_raft::{GroupId, ProposeError, RecordKind};
use support::*;

const G1: GroupId = GroupId(1);

#[tokio::test(flavor = "local")]
async fn a_single_node_commits() {
    let cluster = Cluster::start(1, 2).await;
    cluster.create_group(G1).await;
    for i in 0..10u64 {
        let w = cluster.write(G1, format!("e{i}")).await;
        assert_eq!(w.response, i + 1);
    }
    let applied = cluster.applied(0, G1, 10).await;
    assert_eq!(applied[9], b"e9".to_vec());
    let s = cluster.status(0, G1).await;
    assert!(s.is_leader);
    assert_eq!(s.voters.len(), 1);
    cluster.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn three_nodes_replicate() {
    let cluster = Cluster::start(3, 2).await;
    cluster.create_group(G1).await;
    for i in 0..20 {
        cluster.write(G1, format!("e{i}")).await;
    }
    let expected: Vec<Vec<u8>> = (0..20).map(|i| Vec::<u8>::from(format!("e{i}"))).collect();
    for n in 0..3 {
        assert_eq!(cluster.applied(n, G1, 20).await, expected, "node {n}");
    }
    cluster.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn followers_point_to_the_leader() {
    let cluster = Cluster::start(3, 1).await;
    cluster.create_group(G1).await;
    let leader = cluster.leader(G1).await;
    let follower = (leader + 1) % 3;
    let r = cluster
        .node(follower)
        .propose(G1, None, b"x".to_vec())
        .await;
    let Err(ProposeError::NotLeader {
        leader: Some((id, _)),
    }) = r
    else {
        panic!("{r:?}")
    };
    assert_eq!(id as usize, leader + 1);
    assert_eq!(
        cluster
            .node(0)
            .propose(GroupId(99), None, Vec::<u8>::new())
            .await,
        Err(ProposeError::NoSuchGroup(GroupId(99)))
    );
    cluster.shutdown().await;
}

/// Many concurrent writes go through the pipelined replication streams and
/// commit in one order on every node.
#[tokio::test(flavor = "local")]
async fn concurrent_writes_commit_in_one_order() {
    let cluster = std::sync::Arc::new(Cluster::start(3, 2).await);
    cluster.create_group(G1).await;
    let leader = cluster.leader(G1).await;
    let node = cluster.node(leader).clone();
    let tasks: Vec<_> = (0..500)
        .map(|i| {
            let node = node.clone();
            tokio::task::spawn_local(async move {
                // A leader without a lease yet refuses writes for a moment.
                loop {
                    match node
                        .propose(G1, None, Vec::<u8>::from(format!("w{i}")))
                        .await
                    {
                        Err(ProposeError::NotLeader { leader: None }) => {
                            tokio::time::sleep(Duration::from_millis(10)).await
                        }
                        r => return r,
                    }
                }
            })
        })
        .collect();
    let mut indexes = Vec::new();
    for t in tasks {
        indexes.push(t.await.unwrap().unwrap().index);
    }
    indexes.sort();
    indexes.dedup();
    assert_eq!(indexes.len(), 500);
    let on_leader = cluster.applied(leader, G1, 500).await;
    for n in 0..3 {
        assert_eq!(cluster.applied(n, G1, 500).await, on_leader, "node {n}");
    }
    let cluster = std::sync::Arc::into_inner(cluster).unwrap();
    cluster.shutdown().await;
}

/// Many groups on few cores: they spread over the cores, elect, and stay
/// stable (no spurious elections) while heartbeats share RPCs.
#[tokio::test(flavor = "local")]
async fn many_groups_stay_stable() {
    let cluster = Cluster::start(3, 2).await;
    let groups: Vec<_> = (1..=40).map(GroupId).collect();
    for &g in &groups {
        cluster.create_group(g).await;
    }
    let cores: std::collections::BTreeSet<_> = groups
        .iter()
        .map(|&g| cluster.node(0).core_of(g).unwrap())
        .collect();
    assert_eq!(cores.len(), 2, "groups spread over both cores");
    for &g in &groups {
        cluster.write(g, "x").await;
    }
    let terms: Vec<_> =
        futures::future::join_all(groups.iter().map(|&g| cluster.status(0, g))).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    for (&g, before) in groups.iter().zip(terms) {
        let after = cluster.status(0, g).await;
        assert_eq!(after.term, before.term, "{g} held an election while idle");
    }
    cluster.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn a_new_leader_takes_over() {
    let mut cluster = Cluster::start(3, 2).await;
    cluster.create_group(G1).await;
    for i in 0..5 {
        cluster.write(G1, format!("a{i}")).await;
    }
    let old = cluster.leader(G1).await;
    cluster.stop(old).await;
    let new = cluster.leader(G1).await;
    assert_ne!(old, new);
    for i in 0..5 {
        cluster.write(G1, format!("b{i}")).await;
    }
    let applied = cluster.applied(new, G1, 10).await;
    assert_eq!(
        &applied[..5],
        &(0..5)
            .map(|i| Vec::<u8>::from(format!("a{i}")))
            .collect::<Vec<_>>()[..]
    );
    // The old leader catches up when it returns.
    cluster.restart(old).await;
    assert_eq!(cluster.applied(old, G1, 10).await, applied);
    cluster.shutdown().await;
}

/// A follower that missed many entries catches up through the pipeline.
#[tokio::test(flavor = "local")]
async fn a_lagging_follower_catches_up() {
    let mut cluster = Cluster::start(3, 2).await;
    cluster.create_group(G1).await;
    let leader = cluster.leader(G1).await;
    let follower = (leader + 1) % 3;
    cluster.stop(follower).await;
    for i in 0..300 {
        cluster.write(G1, format!("e{i}")).await;
    }
    cluster.restart(follower).await;
    let expected = cluster.applied(leader, G1, 300).await;
    assert_eq!(cluster.applied(follower, G1, 300).await, expected);
    cluster.shutdown().await;
}

/// Every node restarted: groups reopen on the cores that hosted them, and
/// the log replays into the state machines.
#[tokio::test(flavor = "local")]
async fn a_full_restart_keeps_everything() {
    let mut cluster = Cluster::start(3, 3).await;
    let groups: Vec<_> = (1..=6).map(GroupId).collect();
    for &g in &groups {
        cluster.create_group(g).await;
        for i in 0..5 {
            cluster.write(g, format!("{g}-{i}")).await;
        }
    }
    let cores: Vec<_> = groups
        .iter()
        .map(|&g| cluster.node(1).core_of(g).unwrap())
        .collect();
    for i in 0..3 {
        cluster.stop(i).await;
    }
    for i in 0..3 {
        cluster.restart(i).await;
    }
    for (&g, core) in groups.iter().zip(cores) {
        assert_eq!(
            cluster.node(1).core_of(g),
            Some(core),
            "{g} reopened on its core"
        );
        cluster.leader(g).await;
        cluster.write(g, format!("{g}-after")).await;
        for n in 0..3 {
            let applied = cluster.applied(n, g, 6).await;
            assert_eq!(applied[0], Vec::<u8>::from(format!("{g}-0")));
            assert_eq!(applied[5], Vec::<u8>::from(format!("{g}-after")));
        }
    }
    cluster.shutdown().await;
}

/// Heartbeats from many groups share RPCs, and replication keeps several
/// AppendEntries in flight.
#[tokio::test(flavor = "local")]
async fn heartbeats_batch_and_appends_pipeline() {
    let cluster = Cluster::start(3, 1).await;
    let groups: Vec<_> = (1..=20).map(GroupId).collect();
    for &g in &groups {
        cluster.create_group(g).await;
    }
    // Move every leader to node 0, so its heartbeats all go out from there.
    for &g in &groups {
        let leader = cluster.leader(g).await;
        if leader != 0 {
            cluster.node(leader).transfer_leader(g, 1).await.unwrap();
        }
    }
    for &g in &groups {
        while cluster.leader(g).await != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let before = cluster.node(0).stats();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = cluster.node(0).stats();
    let rpcs = after.heartbeat_rpcs - before.heartbeat_rpcs;
    let beats = after.heartbeats - before.heartbeats;
    assert!(beats >= 20 * 2 * 10, "{beats} heartbeats in 2s");
    // 20 groups × 2 followers per tick, in about 2 RPCs per tick.
    assert!(beats >= 5 * rpcs, "{beats} heartbeats took {rpcs} RPCs");

    // A burst of writes to one group pipelines.
    let node = cluster.node(0).clone();
    let writes: Vec<_> = (0..200)
        .map(|i| {
            let node = node.clone();
            tokio::task::spawn_local(async move {
                node.propose(GroupId(1), None, Vec::<u8>::from(format!("p{i}")))
                    .await
            })
        })
        .collect();
    for w in writes {
        w.await.unwrap().unwrap();
    }
    assert!(
        cluster.node(0).stats().max_append_in_flight > 1,
        "{:?}",
        cluster.node(0).stats()
    );
    cluster.shutdown().await;
}

/// On its core, a group can be proposed to in order, read back, and watched.
#[tokio::test(flavor = "local")]
async fn group_handles_propose_read_and_watch() {
    let cluster = Cluster::start(3, 2).await;
    cluster.create_group(G1).await;
    // A first write, retried, waits out the new leader's lease.
    cluster.write(G1, "first").await;
    let leader = cluster.leader(G1).await;
    let written = cluster
        .node(leader)
        .with_group(G1, |g| async move {
            assert!(g.leadership().is_leader);
            // Proposed together, appended in the order proposed.
            let out: Vec<_> = futures::future::join_all(
                (0..20).map(|i| g.propose(None, Vec::<u8>::from(format!("h{i}")))),
            )
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
            let first = out[0].index;
            let last = out[19].index;
            assert!(out.windows(2).all(|w| w[1].index == w[0].index + 1));
            // The log holds them, readable back.
            let records = g.entries(first..last + 1, usize::MAX).await.unwrap();
            assert_eq!(records.len(), 20);
            let RecordKind::Entry(e) = &records[3].kind else {
                panic!("{:?}", records[3])
            };
            assert_eq!(e.command, b"h3".to_vec());
            assert!(g.applied_index().unwrap() >= last);
            assert!(g.log_range().end > last);
            out.len()
        })
        .await
        .unwrap();
    assert_eq!(written, 20);
    // A follower's handle says who leads.
    let follower = (leader + 1) % 3;
    let seen = cluster
        .node(follower)
        .with_group(G1, |g| async move { g.leadership() })
        .await
        .unwrap();
    assert!(!seen.is_leader);
    assert_eq!(seen.leader.map(|(id, _)| id), Some(leader as u64 + 1));
    let r = cluster
        .node(follower)
        .with_group(
            G1,
            |g| async move { g.propose(None, Vec::<u8>::new()).await },
        )
        .await
        .unwrap();
    assert!(matches!(r, Err(ProposeError::NotLeader { .. })), "{r:?}");
    cluster.shutdown().await;
}

/// A leader cut off from its peers is replaced, and follows once healed.
#[tokio::test(flavor = "local")]
async fn a_partitioned_leader_is_replaced() {
    let cluster = Cluster::start(3, 1).await;
    cluster.create_group(G1).await;
    cluster.write(G1, "before").await;
    let old = cluster.leader(G1).await;
    cluster.node(old).faults().set_partitioned(true);
    let new = loop {
        let mut found = None;
        for i in (0..3).filter(|&i| i != old) {
            if cluster.status(i, G1).await.is_leader {
                found = Some(i);
            }
        }
        if let Some(i) = found {
            break i;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // Retried: the new leader refuses writes until its lease holds.
    let term = loop {
        match cluster
            .node(new)
            .propose(G1, None, b"during".to_vec())
            .await
        {
            Ok(_) => break cluster.status(new, G1).await.term,
            Err(ProposeError::NotLeader { .. }) => {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            Err(e) => panic!("{e}"),
        }
    };
    // The cut-off node can't commit anything.
    let r = tokio::time::timeout(
        Duration::from_millis(500),
        cluster.node(old).propose(G1, None, b"lost".to_vec()),
    )
    .await;
    assert!(!matches!(r, Ok(Ok(_))), "{r:?}");
    cluster.node(old).faults().set_partitioned(false);
    let applied = cluster.applied(old, G1, 2).await;
    assert_eq!(applied, vec![b"before".to_vec(), b"during".to_vec()]);
    assert!(cluster.status(old, G1).await.term >= term);
    cluster.shutdown().await;
}

/// A leader restarted within its election timeout doesn't resume its old
/// term: its replication sessions restart numbering, and followers would
/// take the new ones for stale ones. It wins a new term first.
#[tokio::test(flavor = "local")]
async fn a_restarted_leader_needs_a_new_term() {
    let mut cluster = Cluster::start(3, 1).await;
    cluster.create_group(G1).await;
    for i in 0..20 {
        cluster.write(G1, format!("e{i}")).await;
    }
    let leader = cluster.leader(G1).await;
    let term = cluster.status(leader, G1).await.term;
    cluster.stop(leader).await;
    cluster.restart(leader).await;
    let s = cluster.status(leader, G1).await;
    assert!(!s.is_leader, "restored leadership of term {}", s.term);

    // Whoever leads next does so in a later term, and replicates promptly.
    let started = std::time::Instant::now();
    for i in 0..10 {
        cluster.write(G1, format!("after{i}")).await;
    }
    let now = cluster.leader(G1).await;
    assert!(cluster.status(now, G1).await.term > term);
    for n in 0..3 {
        assert_eq!(cluster.applied(n, G1, 30).await.len(), 30, "node {n}");
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    cluster.shutdown().await;
}
