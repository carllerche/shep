//! The WAL controller: founding from seeds, universes, and each node
//! reconciling its replicas with the controller's state.

mod support;

use std::time::Duration;

use shep_wal_client::Universe;
use shep_wal_client::api::{PartitionId, ProposalKind};
use support::*;

/// Proposes to `p` until it commits: the partition may still be electing.
async fn write_to(cluster: &Cluster, p: PartitionId, k: &str) -> u64 {
    let latest = newest_applied(&cluster.client, p).await;
    let mut proposal = write(at(p, latest), k);
    // Partition `p`, not the harness's default.
    if let ProposalKind::Mutation(m) = &mut proposal.kind {
        m.read_version = at(p, latest);
        m.context = m.read_version.clone();
    }
    cluster
        .client
        .propose(p, &proposal)
        .await
        .unwrap()
        .dot
        .index
}

#[tokio::test(flavor = "local")]
async fn a_single_server_founds_itself_and_serves_universes() {
    let cluster = Cluster::start_controlled(1).await;
    cluster.founded().await;
    let (u, state) = cluster.client.create_universe("prod", 2, 1).await.unwrap();
    assert_eq!(u.name, "prod");
    assert_eq!(u.data.len(), 2);
    for p in u.partitions() {
        assert_eq!(state.partitions[&p].replicas, vec![1]);
        write_to(&cluster, p, "x").await;
    }
    // Names are unique.
    assert!(cluster.client.create_universe("prod", 1, 1).await.is_err());
    // More replicas than nodes.
    assert!(cluster.client.create_universe("big", 1, 3).await.is_err());
    // Too many partitions is refused before any are planned.
    let started = std::time::Instant::now();
    assert!(
        cluster
            .client
            .create_universe("huge", u32::MAX, 1)
            .await
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    // Names are domains, without case.
    assert!(
        cluster
            .client
            .create_universe("not_a_domain", 1, 1)
            .await
            .is_err()
    );
    assert!(cluster.client.create_universe("PROD", 1, 1).await.is_err());
    let state = cluster.client.controller_state().await.unwrap();
    assert_eq!(state.log_sets.len(), 1);
    cluster.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn three_seeds_found_one_cluster() {
    let cluster = Cluster::start_controlled(3).await;
    let id = cluster.founded().await;
    assert_eq!(id.len(), 16);
    let state = cluster.client.controller_state().await.unwrap();
    assert_eq!(
        state.members.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    let (universe, state) = cluster.client.create_universe("prod", 3, 3).await.unwrap();
    // Every node reconciles: it hosts a replica of every partition.
    for p in universe.partitions() {
        assert_eq!(state.partitions[&p].replicas, vec![1, 2, 3]);
        let index = write_to(&cluster, p, "x").await;
        for i in 0..3 {
            read_node_log(&cluster, i, p, index).await;
        }
    }
    cluster.shutdown().await;
}

/// Replicas spread over nodes by count.
#[tokio::test(flavor = "local")]
async fn partitions_spread_over_nodes() {
    let cluster = Cluster::start_controlled(3).await;
    cluster.founded().await;
    let (universe, state) = cluster.client.create_universe("prod", 5, 1).await.unwrap();
    let mut per_node = std::collections::BTreeMap::new();
    for p in universe.partitions() {
        *per_node
            .entry(state.partitions[&p].replicas[0])
            .or_insert(0) += 1;
        write_to(&cluster, p, "x").await;
    }
    assert_eq!(
        per_node.values().copied().collect::<Vec<_>>(),
        vec![2, 2, 2]
    );
    cluster.shutdown().await;
}

/// The controller's state and every partition survive a full restart.
#[tokio::test(flavor = "local")]
async fn a_restarted_cluster_keeps_its_universes() {
    let mut cluster = Cluster::start_controlled(3).await;
    let id = cluster.founded().await;
    let (universe, _) = cluster.client.create_universe("prod", 2, 3).await.unwrap();
    for &p in &universe.data {
        write_to(&cluster, p, "before").await;
    }
    for i in 0..3 {
        cluster.stop(i).await;
    }
    for i in 0..3 {
        cluster.restart(i).await;
    }
    assert_eq!(cluster.founded().await, id, "the same cluster");
    let after = cluster.client.controller_state().await.unwrap();
    assert_eq!(Universe::find(&after, universe.id), Some(universe.clone()));
    for &p in &universe.data {
        write_to(&cluster, p, "after").await;
    }
    cluster.shutdown().await;
}

/// A cluster is founded only once every seed is reachable.
#[tokio::test(flavor = "local")]
async fn founding_waits_for_every_seed() {
    let mut cluster = Cluster::start_controlled_some(3, 2).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    for i in 0..2 {
        let state = cluster.node(i).controller_state().await.unwrap();
        assert!(!state.is_founded(), "founded without seed 3");
    }
    cluster.restart(2).await;
    cluster.founded().await;
    let state = cluster.client.controller_state().await.unwrap();
    assert_eq!(state.members.len(), 3);
    cluster.shutdown().await;
}

/// Seeds whose lists differ don't found a cluster together: only nodes that
/// agree on the list do.
#[tokio::test(flavor = "local")]
async fn seeds_with_different_lists_dont_found_together() {
    // Node 1 lists {1, 2, 3}; nodes 2 and 3 list {2, 3}.
    let cluster = Cluster::start_controlled_with(3, 3, |i, c| {
        if i > 0 {
            c.seeds.retain(|m| m.node_id != 1);
        }
    })
    .await;
    // Nodes 2 and 3 agree, so they found a cluster of two.
    for _ in 0..1500 {
        let founded = cluster
            .node(1)
            .controller_state()
            .await
            .is_some_and(|s| s.is_founded());
        if founded {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = cluster.node(1).controller_state().await.unwrap();
    assert_eq!(
        state.members.keys().copied().collect::<Vec<_>>(),
        vec![2, 3]
    );
    // Node 1 never founds anything.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !cluster
            .node(0)
            .controller_state()
            .await
            .unwrap()
            .is_founded()
    );
    cluster.shutdown().await;
}

/// On a node with seeds, partitions come only from the controller: one made
/// by hand could take the id a new universe is given, data and all.
#[tokio::test(flavor = "local")]
async fn controlled_nodes_refuse_partitions_made_by_hand() {
    let cluster = Cluster::start_controlled(1).await;
    cluster.founded().await;
    // The id the controller allocates next.
    let p = PartitionId(1);
    let members = cluster.members();
    let node = cluster.node(0);
    assert!(matches!(
        node.create_partition(p).await,
        Err(shep_wal::WalError::ControllerManaged)
    ));
    assert!(matches!(
        node.initialize_partition(p, &members).await,
        Err(shep_wal::WalError::ControllerManaged)
    ));
    let rpc = cluster.node_client(0);
    assert!(rpc.create_partition(p).await.is_err());
    assert!(rpc.initialize_partition(p, &members).await.is_err());
    assert!(node.status(p).await.is_none());

    // The controller's universe gets the id, with its own replica.
    let (u, _) = cluster.client.create_universe("prod", 1, 1).await.unwrap();
    assert_eq!(u.meta, p);
    write_to(&cluster, p, "x").await;
    cluster.shutdown().await;
}
