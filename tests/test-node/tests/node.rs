//! `shep-node` as deployed: one server alone, and a three-process cluster,
//! with crashes (SIGKILL) and clean stops (SIGTERM).

use std::time::Duration;

use shep_futures::StreamExt;
use shep_wal_client::api::{EntryKind, PartitionId};
use test_node::*;

fn spec(id: u64) -> NodeSpec {
    NodeSpec {
        id,
        addr: free_addr(),
        dir: tempfile::tempdir().unwrap(),
    }
}

/// Collects `p`'s committed mutations' payloads, through `through`.
async fn payloads(
    client: &shep_wal_client::WalClient,
    p: PartitionId,
    through: u64,
) -> Vec<String> {
    let stream = client.tail(p, 0);
    let mut stream = std::pin::pin!(stream);
    let mut out = Vec::new();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await
            .expect("tail stalled")
            .unwrap();
        if let EntryKind::Mutation(m) = &e.kind {
            out.push(String::from_utf8(m.payload.to_vec()).unwrap());
        }
        if e.dot.index >= through {
            return out;
        }
    }
}

/// One server is a complete deployment: it founds itself, serves a
/// universe, survives a crash, and stops cleanly.
#[tokio::test(flavor = "local")]
async fn a_single_server_is_a_real_deployment() {
    let node = spec(1);
    let process = Process::start(&node, &[], 2);
    let client = client([node.addr]);
    founded(&client).await;
    let (u, _) = client.create_universe("prod", 2, 1).await.unwrap();
    let data = u.data;

    let mut written = Vec::new();
    for (i, &p) in data.iter().enumerate() {
        let proposal = write(p, applied(&client, p).await, "k", &format!("before-{i}"));
        let d = client.propose(p, &proposal).await.unwrap();
        written.push((p, proposal, d));
    }

    // A crash, and a restart from the same directory.
    process.kill().await;
    let process = Process::start(&node, &[], 2);
    founded(&client).await;
    for (i, (p, proposal, d)) in written.iter().enumerate() {
        // The re-sent proposal gets its original outcome.
        assert_eq!(&client.propose(*p, proposal).await.unwrap(), d);
        assert_eq!(
            payloads(&client, *p, d.dot.index).await,
            vec![format!("before-{i}")]
        );
        let next = write(*p, applied(&client, *p).await, "k2", "after");
        client.propose(*p, &next).await.unwrap();
    }

    let status = process.stop().await;
    assert!(status.success(), "{status:?}");
}

/// Three processes found one cluster; a universe replicated on all three
/// keeps committing while one is down, and the returning node catches up.
#[tokio::test(flavor = "local")]
async fn a_three_process_cluster_survives_a_crash() {
    let nodes: Vec<_> = (1..=3).map(spec).collect();
    let seeds: Vec<_> = nodes.iter().map(|n| (n.id, n.addr)).collect();
    let mut processes: Vec<_> = nodes
        .iter()
        .map(|n| Some(Process::start(n, &seeds, 2)))
        .collect();
    let client = client(nodes.iter().map(|n| n.addr));
    founded(&client).await;
    let (u, _) = client.create_universe("prod", 2, 3).await.unwrap();
    let p = u.data[0];

    let mut last = 0;
    for i in 0..10 {
        let w = write(
            p,
            applied(&client, p).await,
            &format!("k{i}"),
            &format!("a{i}"),
        );
        last = client.propose(p, &w).await.unwrap().dot.index;
    }

    // Kill node 1; the others keep committing.
    processes[0].take().unwrap().kill().await;
    for i in 0..10 {
        let w = write(
            p,
            applied(&client, p).await,
            &format!("j{i}"),
            &format!("b{i}"),
        );
        last = client.propose(p, &w).await.unwrap().dot.index;
    }

    processes[0] = Some(Process::start(&nodes[0], &seeds, 2));
    // The restarted node serves the whole log.
    let only_node1 = client_for(&nodes[0]);
    let got = payloads(&only_node1, p, last).await;
    let expected: Vec<String> = (0..10)
        .map(|i| format!("a{i}"))
        .chain((0..10).map(|i| format!("b{i}")))
        .collect();
    assert_eq!(got, expected);

    for p in processes.into_iter().flatten() {
        assert!(p.stop().await.success());
    }
}

/// A process can't start on a data directory another one is using, so a
/// running node's predecessor on the directory is gone; once that one dies,
/// even by SIGKILL, the directory is free.
#[tokio::test(flavor = "local")]
async fn one_process_per_data_directory() {
    let node = spec(1);
    let first = Process::start(&node, &[], 1);
    let client = client([node.addr]);
    founded(&client).await;

    // Same directory, its own address: it exits, refused.
    let twin = NodeSpec {
        id: 1,
        addr: free_addr(),
        dir: tempfile::tempdir().unwrap(),
    };
    let mut second = Process::start_in(&twin, node.dir.path(), &[], 1);
    let status = second
        .wait(Duration::from_secs(20))
        .await
        .expect("a second process on the directory kept running");
    assert!(!status.success(), "{status:?}");
    // The first is unaffected.
    founded(&client).await;

    first.kill().await;
    let restarted = Process::start(&node, &[], 1);
    founded(&client).await;
    assert!(restarted.stop().await.success());
}

fn client_for(node: &NodeSpec) -> shep_wal_client::WalClient {
    client([node.addr])
}
