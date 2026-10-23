//! The contract between a group and its [`shep_raft::Machine`]: admission on
//! the leader, leader-side state rebuilt from the log, idempotency keys, and
//! reading the log back.

mod support;

use std::time::Duration;

use shep_raft::{Fetched, GroupId, ProposeError, RecordKind, Resolution};
use support::*;

const G1: GroupId = GroupId(1);

/// Proposes `payload` with `key` through the leader, retrying while it has
/// no lease yet.
async fn propose(
    cluster: &Cluster,
    key: Option<&'static str>,
    payload: &'static str,
) -> Result<shep_raft::Decided<u64>, ProposeError<String>> {
    for _ in 0..200 {
        let leader = cluster.leader(G1).await;
        let r = cluster
            .node(leader)
            .propose(
                G1,
                key.map(|k| k.as_bytes().to_vec()),
                payload.as_bytes().to_vec(),
            )
            .await;
        match r {
            Err(ProposeError::NotLeader { .. }) => {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            r => return r,
        }
    }
    panic!("never decided");
}

/// A proposal the machine refuses appends nothing; the next one admitted
/// gets the next index.
#[tokio::test(flavor = "local")]
async fn refused_proposals_append_nothing() {
    let cluster = Cluster::start(3, 1).await;
    cluster.create_group(G1).await;
    let a = propose(&cluster, None, "a").await.unwrap();
    assert_eq!(
        propose(&cluster, None, "refuse").await,
        Err(ProposeError::Rejected("refused".into()))
    );
    let b = propose(&cluster, None, "b").await.unwrap();
    assert_eq!(b.index, a.index + 1);
    for n in 0..3 {
        assert_eq!(
            cluster.applied(n, G1, 2).await,
            vec![b"a".to_vec(), b"b".to_vec()]
        );
    }
    cluster.shutdown().await;
}

/// A key is decided once: a retry, whatever it proposes, gets the first
/// decision, before and after the leader changes and after a full restart.
#[tokio::test(flavor = "local")]
async fn keys_are_decided_once() {
    let mut cluster = Cluster::start(3, 1).await;
    cluster.create_group(G1).await;
    let first = propose(&cluster, Some("k"), "one").await.unwrap();
    assert!(!first.replayed);
    let again = propose(&cluster, Some("k"), "two").await.unwrap();
    assert!(again.replayed);
    assert_eq!(
        (again.index, again.commit_time_us, again.response),
        (first.index, first.commit_time_us, first.response)
    );

    // Another leader knows it too.
    let leader = cluster.leader(G1).await;
    let next = (leader + 1) % 3;
    cluster
        .node(leader)
        .transfer_leader(G1, next as u64 + 1)
        .await
        .unwrap();
    while cluster.leader(G1).await != next {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let other = propose(&cluster, Some("k"), "three").await.unwrap();
    assert_eq!((other.index, other.replayed), (first.index, true));

    // So does every node rebuilt from its log.
    for i in 0..3 {
        cluster.stop(i).await;
    }
    for i in 0..3 {
        cluster.restart(i).await;
    }
    let restarted = propose(&cluster, Some("k"), "four").await.unwrap();
    assert_eq!((restarted.index, restarted.replayed), (first.index, true));
    // A new key is a new decision.
    let fresh = propose(&cluster, Some("j"), "five").await.unwrap();
    assert!(!fresh.replayed && fresh.index > first.index);
    for n in 0..3 {
        assert_eq!(
            cluster.applied(n, G1, 2).await,
            vec![b"one".to_vec(), b"five".to_vec()]
        );
    }
    cluster.shutdown().await;
}

/// A replica that starts leading replays its log into its machine before
/// admitting anything; each admission's fate comes back by its sequence
/// number.
#[tokio::test(flavor = "local")]
async fn a_new_leader_replays_then_admits() {
    let cluster = Cluster::start(3, 1).await;
    cluster.create_group(G1).await;
    let a = propose(&cluster, None, "a").await.unwrap();
    let b = propose(&cluster, None, "b").await.unwrap();
    let leader = cluster.leader(G1).await;
    let calls = cluster.calls(leader, G1).await;
    let term = cluster.status(leader, G1).await.term;
    assert!(
        matches!(calls[0], Call::Lead { term: t, .. } if t == term),
        "{calls:?}"
    );
    // A leader refuses appends until its lease holds, so "a" may have been
    // admitted and refused before; those numbers came back NotAppended.
    let s = admitted(&calls, "a");
    assert_eq!(
        calls[calls.len() - 4..],
        [
            Call::Admit(s, "a".into()),
            Call::Resolve(s, Resolution::Appended(a.index)),
            Call::Admit(s + 1, "b".into()),
            Call::Resolve(s + 1, Resolution::Appended(b.index)),
        ]
    );
    assert_refused_before(&calls, s);

    let next = (leader + 1) % 3;
    cluster
        .node(leader)
        .transfer_leader(G1, next as u64 + 1)
        .await
        .unwrap();
    while cluster.leader(G1).await != next {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let c = propose(&cluster, None, "c").await.unwrap();
    let new_term = cluster.status(next, G1).await.term;
    let calls = cluster.calls(next, G1).await;
    let Some(Call::Lead { term, floor }) = calls.first().cloned() else {
        panic!("{calls:?}")
    };
    assert_eq!(term, new_term);
    // Everything since the log's start is in the window, so all of it is
    // replayed, oldest first, before the first admission.
    assert_eq!(floor, 0);
    assert_eq!(
        calls[1..3],
        [
            Call::Replay(a.index, "a".into()),
            Call::Replay(b.index, "b".into()),
        ]
    );
    let s = admitted(&calls, "c");
    assert_eq!(
        calls[calls.len() - 2..],
        [
            Call::Admit(s, "c".into()),
            Call::Resolve(s, Resolution::Appended(c.index)),
        ]
    );
    assert_refused_before(&calls, s);
    cluster.shutdown().await;
}

/// The sequence number `payload` was last admitted under.
fn admitted(calls: &[Call], payload: &str) -> u64 {
    calls
        .iter()
        .rev()
        .find_map(|c| match c {
            Call::Admit(s, p) if p == payload.as_bytes() => Some(*s),
            _ => None,
        })
        .unwrap()
}

/// Every admission numbered below `seq` this term was refused before it
/// was appended.
fn assert_refused_before(calls: &[Call], seq: u64) {
    for s in 0..seq {
        assert!(
            calls.contains(&Call::Resolve(s, Resolution::NotAppended)),
            "{s}: {calls:?}"
        );
    }
}

/// An admitted command too large to append is never appended, and the
/// machine hears so.
#[tokio::test(flavor = "local")]
async fn too_large_commands_are_not_appended() {
    let cluster = Cluster::start_with(1, 1, |c| c.proposals.max_entry_bytes = 64).await;
    cluster.create_group(G1).await;
    let big = "x".repeat(100).leak();
    let r = propose(&cluster, None, big).await;
    assert!(
        matches!(r, Err(ProposeError::TooLarge { limit: 64, .. })),
        "{r:?}"
    );
    let ok = propose(&cluster, None, "small").await.unwrap();
    let calls = cluster.calls(0, G1).await;
    assert!(
        calls.contains(&Call::Resolve(0, Resolution::NotAppended)),
        "{calls:?}"
    );
    assert!(calls.contains(&Call::Resolve(1, Resolution::Appended(ok.index))));
    assert_eq!(cluster.applied(0, G1, 1).await, vec![b"small".to_vec()]);
    cluster.shutdown().await;
}

/// Fetching waits for committed entries and returns them decoded, with
/// Raft's own entries marked as such.
#[tokio::test(flavor = "local")]
async fn fetch_long_polls_for_committed_entries() {
    let cluster = Cluster::start(1, 1).await;
    cluster.create_group(G1).await;
    let a = propose(&cluster, Some("k"), "a").await.unwrap();
    let from = a.index + 1;
    let (waited, fetched) = cluster
        .node(0)
        .with_group(G1, move |g| async move {
            let started = std::time::Instant::now();
            // Nothing past `a` yet: it waits, then answers empty.
            let empty = g.fetch(from, usize::MAX, Duration::from_millis(200)).await;
            let waited = started.elapsed();
            assert_eq!(
                empty,
                Fetched::Records {
                    records: vec![],
                    applied_index: Some(a.index)
                }
            );
            (waited, g.fetch(0, usize::MAX, Duration::ZERO).await)
        })
        .await
        .unwrap();
    assert!(waited >= Duration::from_millis(150), "{waited:?}");
    let Fetched::Records { records, .. } = fetched else {
        panic!("{fetched:?}")
    };
    assert_eq!(records.last().unwrap().index, a.index);
    assert!(
        records[..records.len() - 1]
            .iter()
            .all(|r| r.kind == RecordKind::Internal)
    );
    let RecordKind::Entry(e) = &records.last().unwrap().kind else {
        panic!("{records:?}")
    };
    assert_eq!(
        (e.command.clone(), e.key.clone(), e.commit_time_us),
        (b"a".to_vec(), Some(b"k".to_vec()), a.commit_time_us)
    );

    // A fetch waiting for the next entry returns once it commits.
    let node = cluster.node(0).clone();
    let waiting = tokio::task::spawn_local(async move {
        node.with_group(G1, move |g| async move {
            g.fetch(from, usize::MAX, Duration::from_secs(10)).await
        })
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let b = propose(&cluster, None, "b").await.unwrap();
    let Fetched::Records { records, .. } = waiting.await.unwrap() else {
        panic!()
    };
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].index, b.index);
    cluster.shutdown().await;
}
