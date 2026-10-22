//! Read-modify-write workers under leader kills, restarts, and network
//! partitions, checked against `WalValidate.tla`'s invariants:
//!
//! - **Serializable**: replaying the committed log in order, every mutation's
//!   reads equal the state at its position.
//! - **IdemOnce**: no idempotency key is decided twice.
//! - **RepliesDurable**: every outcome a client was given is in the
//!   committed log, at its dot.

mod support;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use shep_wal_client::ProposeError;
use shep_wal_client::api::{Decided, Dot, EntryKind, Mutation, Proposal, ProposalKind};
use support::*;

const COUNTERS: usize = 4;
const WORKERS: usize = 6;
const INCREMENTS: usize = 20;

#[tokio::test(flavor = "multi_thread")]
async fn chaos_keeps_the_log_serializable() {
    let mut cluster = Cluster::start_with(3, &[P1], |c| {
        c.raft.proposals.commit_timeout = Duration::from_millis(800)
    })
    .await;
    let regs = Registers::follow(&cluster.client, P1);
    let done = Arc::new(AtomicBool::new(false));

    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let client = cluster.client.clone();
            let regs = regs.clone();
            tokio::spawn(async move {
                let mut acked: Vec<(Bytes, Decided)> = Vec::new();
                for _ in 0..INCREMENTS {
                    let key = unique_key();
                    let counter = format!("c{}", rand::random_range(0..COUNTERS));
                    acked.push((key.clone(), increment(&client, &regs, key, &counter).await));
                }
                acked
            })
        })
        .collect();

    let chaos = {
        let done = done.clone();
        async move {
            let mut rounds = 0;
            while !done.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(rand::random_range(200..600))).await;
                let leader = cluster.leader(P1).await;
                match rounds % 3 {
                    // Kill the leader, and bring it back later.
                    0 => {
                        cluster.stop(leader).await;
                        tokio::time::sleep(Duration::from_millis(rand::random_range(300..900)))
                            .await;
                        cluster.restart(leader).await;
                    }
                    // Cut the leader off from its peers.
                    1 => {
                        cluster.node(leader).faults().set_partitioned(true);
                        tokio::time::sleep(Duration::from_millis(rand::random_range(300..1500)))
                            .await;
                        cluster.node(leader).faults().set_partitioned(false);
                    }
                    // Kill a follower.
                    _ => {
                        let f = (leader + 1 + rounds % 2) % 3;
                        cluster.stop(f).await;
                        tokio::time::sleep(Duration::from_millis(rand::random_range(100..600)))
                            .await;
                        cluster.restart(f).await;
                    }
                }
                rounds += 1;
            }
            (cluster, rounds)
        }
    };
    let chaos = tokio::spawn(chaos);

    let mut acked = Vec::new();
    for w in workers {
        acked.extend(w.await.unwrap());
    }
    done.store(true, Ordering::SeqCst);
    let (cluster, rounds) = chaos.await.unwrap();
    assert!(rounds >= 3, "only {rounds} rounds of chaos");

    let last = acked.iter().map(|(_, d)| d.dot.index).max().unwrap();
    regs.wait_applied(last).await;
    let log = read_log(&cluster.client, P1, last).await;

    // Serializable.
    regs.with(|st| assert!(st.violations.is_empty(), "{:#?}", st.violations));

    // IdemOnce.
    let mut seen = HashSet::new();
    regs.with(|st| {
        for k in &st.keys {
            assert!(seen.insert(k.clone()), "key {k:?} decided twice");
        }
    });

    // RepliesDurable.
    let by_dot: BTreeMap<Dot, Bytes> = log
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Mutation(m) => Some((e.dot, m.idempotency_key.clone())),
            _ => None,
        })
        .collect();
    for (key, d) in &acked {
        assert!(committed(d));
        assert_eq!(
            by_dot.get(&d.dot),
            Some(key),
            "acknowledged at {} but not in the log",
            d.dot
        );
    }

    // Every acknowledged increment is counted once, and nothing else is.
    let total: i64 = regs.snapshot().1.values().sum();
    assert_eq!(total as usize, WORKERS * INCREMENTS);
    cluster.shutdown().await;
}

/// Reads `counter` at the newest applied version and proposes `counter + 1`
/// under `key`, re-executing at a newer version until the key is decided.
async fn increment(
    client: &shep_wal_client::WalClient,
    regs: &Registers,
    key: Bytes,
    counter: &str,
) -> Decided {
    loop {
        let (applied, values) = regs.snapshot();
        let v = values.get(counter).copied().unwrap_or(0);
        let p = Proposal {
            idempotency_key: key.clone(),
            kind: ProposalKind::Mutation(Mutation {
                read_version: at(P1, applied),
                context: at(P1, applied),
                read_set: vec![support::key(counter)],
                write_set: vec![support::key(counter)],
                payload: Bytes::from(format!("r {counter} {v}\nw {counter} {}", v + 1)),
                reply: Bytes::from_static(b"ok"),
            }),
        };
        match client.propose(P1, &p).await {
            Ok(d) => return d,
            Err(ProposeError::Contended | ProposeError::TooOld) => {
                // Let the materializer catch up a little before re-reading.
                tokio::time::sleep(Duration::from_millis(rand::random_range(1..20))).await;
            }
            // The outcome is unknown; re-executing under the same key learns
            // it, or commits it if nothing did.
            Err(ProposeError::Unavailable(_)) => {}
            Err(e) => panic!("{e}"),
        }
    }
}
