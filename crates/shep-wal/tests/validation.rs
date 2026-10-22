//! Leader validation (§10.3): read sets, ranges, extras, write sets entered at
//! append, the window, and malformed proposals.

mod support;

use std::time::Duration;

use shep_wal_client::ProposeError;
use shep_wal_client::api::{Dot, EntryKind, Mutation, ProposalKind, Rejection};
use support::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_commits_and_is_tailed() {
    let cluster = Cluster::start(3, &[P1]).await;
    let mut proposal = write(at(P1, 0), "a");
    let context = at(P1, 0).with_prefix(shep_wal_client::api::PartitionId(9), 4);
    let ProposalKind::Mutation(m) = &mut proposal.kind else {
        unreachable!()
    };
    m.context = context.clone();
    let decided = cluster.client.propose(P1, &proposal).await.unwrap();
    assert!(committed(&decided));
    assert_eq!(decided.dot.partition, P1);
    assert_eq!(&decided.reply[..], b"ok");

    let log = read_log(&cluster.client, P1, decided.dot.index).await;
    // Contiguous from index 0, the initial membership.
    for (i, e) in log.iter().enumerate() {
        assert_eq!(e.dot, Dot::new(P1, i as u64));
    }
    assert_eq!(log[0].kind, EntryKind::Internal);
    let EntryKind::Mutation(entry) = &log.last().unwrap().kind else {
        panic!("{:?}", log.last())
    };
    assert_eq!(&entry.payload[..], b"w a");
    assert_eq!(entry.context, context);
    assert_eq!(entry.idempotency_key, proposal.idempotency_key);
    assert_eq!(entry.commit_time_us, decided.commit_time_us);
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_the_read_version_lacks_conflicts() {
    let cluster = Cluster::start(3, &[P1]).await;
    let c = &cluster.client;
    let s0 = at(
        P1,
        c.propose(P1, &write(at(P1, 0), "x"))
            .await
            .unwrap()
            .dot
            .index,
    );

    let w = c.propose(P1, &write(s0.clone(), "a")).await.unwrap();
    // Read at s0, which doesn't contain w.
    assert_eq!(
        c.propose(P1, &write(s0.clone(), "a")).await,
        Err(ProposeError::Contended)
    );
    // A key nobody wrote since s0.
    c.propose(P1, &write(s0.clone(), "b")).await.unwrap();
    // At a version containing w.
    c.propose(P1, &write(at(P1, w.dot.index), "a"))
        .await
        .unwrap();
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn range_reads_conflict_with_writes_inside_them() {
    let cluster = Cluster::start(3, &[P1]).await;
    let c = &cluster.client;
    let s0 = at(
        P1,
        c.propose(P1, &write(at(P1, 0), "x"))
            .await
            .unwrap()
            .dot
            .index,
    );

    // A point write inside a range someone read: a phantom.
    c.propose(P1, &write(s0.clone(), "user/5")).await.unwrap();
    let scan = |r| mutation(s0.clone(), vec![r], vec![key("count")], "scan");
    assert_eq!(
        c.propose(P1, &scan(prefix("user/"))).await,
        Err(ProposeError::Contended)
    );
    assert_eq!(
        c.propose(P1, &scan(prefix("user"))).await,
        Err(ProposeError::Contended)
    );
    c.propose(P1, &scan(prefix("users/"))).await.unwrap();
    c.propose(P1, &scan(prefix("item/"))).await.unwrap();

    // A range write covering a key someone read.
    let wide = mutation(s0.clone(), vec![], vec![prefix("doc/")], "wipe");
    c.propose(P1, &wide).await.unwrap();
    assert_eq!(
        c.propose(P1, &write(s0.clone(), "doc/1")).await,
        Err(ProposeError::Contended)
    );
    assert_eq!(
        c.propose(P1, &mutation(s0.clone(), vec![prefix("do")], vec![], ""))
            .await,
        Err(ProposeError::Contended),
    );
    c.propose(P1, &write(s0.clone(), "doc")).await.unwrap();
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn extras_count_as_seen() {
    let cluster = Cluster::start(3, &[P1]).await;
    let c = &cluster.client;
    let s0 = at(
        P1,
        c.propose(P1, &write(at(P1, 0), "x"))
            .await
            .unwrap()
            .dot
            .index,
    );
    let wa = c.propose(P1, &write(s0.clone(), "a")).await.unwrap();
    let wb = c.propose(P1, &write(s0.clone(), "b")).await.unwrap();
    assert!(wa.dot.index < wb.dot.index);

    // s0 plus wb as an extra: wb is seen, wa isn't.
    let s = s0.clone().with_dot(wb.dot);
    c.propose(P1, &write(s.clone(), "b")).await.unwrap();
    assert_eq!(
        c.propose(P1, &write(s.clone(), "a")).await,
        Err(ProposeError::Contended)
    );
    cluster.shutdown().await;
}

/// The design's C1 counterexample: an entry appended but not committed is
/// already in the table, so a proposal that didn't see it is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn uncommitted_writes_are_checked() {
    // A long election timeout keeps the cut-off leader's quorum lease, so it
    // keeps accepting.
    let cluster = Cluster::start_with(3, &[P1], |c| {
        c.raft.proposals.commit_timeout = Duration::from_millis(300);
        c.raft.election_timeout = Duration::from_secs(10)..Duration::from_secs(20);
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
    let leader = cluster.leader(P1).await;
    let node = cluster.node_client(leader);

    // Cut both followers off: the leader appends but can't commit.
    for i in (0..3).filter(|&i| i != leader) {
        cluster.node(i).faults().set_partitioned(true);
    }
    let pending = write(s0.clone(), "k");
    let r = node.propose(P1, &pending).await.unwrap();
    assert!(matches!(r, Err(Rejection::UnknownOutcome(_))), "{r:?}");

    // Validated against the pending entry.
    assert_eq!(
        node.propose(P1, &write(s0.clone(), "k")).await.unwrap(),
        Err(Rejection::Contended)
    );
    let read_k = mutation(s0.clone(), vec![prefix("k")], vec![key("other")], "");
    assert_eq!(
        node.propose(P1, &read_k).await.unwrap(),
        Err(Rejection::Contended)
    );
    // A retry of the pending proposal itself waits for it, and isn't
    // appended twice.
    let r = node.propose(P1, &pending).await.unwrap();
    assert!(matches!(r, Err(Rejection::UnknownOutcome(_))), "{r:?}");
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_older_than_the_window_are_too_old() {
    let cluster =
        Cluster::start_with(3, &[P1], |c| c.raft.proposals.admission_window_entries = 4).await;
    let c = &cluster.client;
    let mut last = 0;
    for i in 0..10 {
        last = c
            .propose(P1, &write(at(P1, last), &format!("k{i}")))
            .await
            .unwrap()
            .dot
            .index;
    }
    assert_eq!(
        c.propose(P1, &write(at(P1, 1), "fresh")).await,
        Err(ProposeError::TooOld)
    );
    c.propose(P1, &write(at(P1, last), "fresh")).await.unwrap();
    // Extras don't help: the prefix is what the window is checked against.
    let s = at(P1, 1).with_dot(Dot::new(P1, last));
    assert_eq!(
        c.propose(P1, &write(s, "fresh2")).await,
        Err(ProposeError::TooOld)
    );
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_proposals_are_invalid() {
    let cluster = Cluster::start(3, &[P1]).await;
    let c = &cluster.client;

    let mut p = write(at(P1, 0), "a");
    p.idempotency_key = Default::default();
    assert!(matches!(
        c.propose(P1, &p).await,
        Err(ProposeError::Invalid(_))
    ));

    // The context must include the read version.
    let mut p = write(at(P1, 1), "a");
    let ProposalKind::Mutation(Mutation { context, .. }) = &mut p.kind else {
        unreachable!()
    };
    *context = at(P1, 0);
    assert!(matches!(
        c.propose(P1, &p).await,
        Err(ProposeError::Invalid(_))
    ));

    // A read version naming entries the leader doesn't have.
    assert!(matches!(
        c.propose(P1, &write(at(P1, 1_000_000), "a")).await,
        Err(ProposeError::Invalid(_))
    ));
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn followers_point_to_the_leader() {
    let cluster = Cluster::start(3, &[P1]).await;
    let leader = cluster.leader(P1).await;
    let follower = (leader + 1) % 3;
    let r = cluster
        .node_client(follower)
        .propose(P1, &write(at(P1, 0), "a"))
        .await
        .unwrap();
    let Err(Rejection::NotLeader { leader: Some(m) }) = r else {
        panic!("{r:?}")
    };
    assert_eq!(m.addr, cluster.addr(leader));

    let unknown = shep_wal_client::api::PartitionId(77);
    let r = cluster
        .node_client(follower)
        .propose(unknown, &write(at(P1, 0), "a"))
        .await;
    assert_eq!(r.unwrap(), Err(Rejection::UnknownPartition));
    cluster.shutdown().await;
}

/// Concurrent proposals on disjoint keys all commit, with no retries.
#[tokio::test(flavor = "multi_thread")]
async fn disjoint_proposals_all_commit() {
    let cluster = Cluster::start(3, &[P1]).await;
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
    let tasks: Vec<_> = (0..200)
        .map(|i| {
            let client = cluster.client.clone();
            let s0 = s0.clone();
            tokio::spawn(async move { client.propose(P1, &write(s0, &format!("key{i}"))).await })
        })
        .collect();
    let mut dots = Vec::new();
    for t in tasks {
        dots.push(t.await.unwrap().unwrap().dot);
    }
    dots.sort();
    dots.dedup();
    assert_eq!(dots.len(), 200);
    cluster.shutdown().await;
}

/// Concurrent proposals that all read and write one key at one version:
/// exactly one commits.
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_proposals_one_commits() {
    let cluster = Cluster::start(3, &[P1]).await;
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
    let tasks: Vec<_> = (0..50)
        .map(|_| {
            let client = cluster.client.clone();
            let s0 = s0.clone();
            tokio::spawn(async move { client.propose(P1, &write(s0, "hot")).await })
        })
        .collect();
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(ProposeError::Contended) => {}
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(ok, 1);
    cluster.shutdown().await;
}
