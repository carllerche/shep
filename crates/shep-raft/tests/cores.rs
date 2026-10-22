//! A node restarted with a different number of cores.

mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use shep_raft::{GroupId, ProposeError, RaftNode, RaftNodeError};
use shep_smp::{CoreId, Smp, SmpConfig};
use support::*;

const G1: GroupId = GroupId(1);

async fn start(dir: &Path, cores: usize) -> Result<(RaftNode<Recorder>, Smp), RaftNodeError> {
    let smp = Smp::start(SmpConfig {
        pin: false,
        ..SmpConfig::with_cores(cores)
    })
    .unwrap();
    match RaftNode::start(test_config(1, dir), smp.clone(), |_| Recorder::default()).await {
        Ok(node) => Ok((node, smp)),
        Err(e) => {
            smp.shutdown().await;
            Err(e)
        }
    }
}

async fn stop((node, smp): (RaftNode<Recorder>, Smp)) {
    node.shutdown().await;
    smp.shutdown().await;
}

async fn write(node: &RaftNode<Recorder>, payload: &'static str) {
    for _ in 0..500 {
        match node.propose(G1, None, payload.into()).await {
            Ok(_) => return,
            Err(ProposeError::NotLeader { .. }) => {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            Err(e) => panic!("write: {e}"),
        }
    }
    panic!("never committed");
}

/// Groups live in their core's directory, so a node can't drop a core that
/// hosts any: it refuses to start rather than come up without them.
#[tokio::test(flavor = "multi_thread")]
async fn fewer_cores_than_host_groups_is_refused() {
    init_tracing();
    let dir = tempfile::tempdir().unwrap();
    let node = start(dir.path(), 2).await.unwrap();
    assert_eq!(
        node.0.create_group_on(G1, CoreId(1)).await.unwrap(),
        CoreId(1)
    );
    let members = BTreeMap::from([(1, "127.0.0.1:1".to_string())]);
    node.0.initialize_group(G1, members).await.unwrap();
    write(&node.0, "kept").await;
    stop(node).await;

    match start(dir.path(), 1).await {
        Err(RaftNodeError::CoreRemoved {
            core: 1,
            groups: 1,
            cores: 1,
        }) => {}
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("started without core 1's groups"),
    }

    // More cores is fine; the group stays on core 1.
    let node = start(dir.path(), 3).await.unwrap();
    assert_eq!(node.0.core_of(G1), Some(CoreId(1)));
    write(&node.0, "more").await;
    let entries = node
        .0
        .with_state_machine(G1, |sm| sm.entries.clone())
        .await
        .unwrap();
    assert_eq!(entries, vec!["kept", "more"]);
    stop(node).await;

    // Core 2 never hosted a group, so it can go again.
    let node = start(dir.path(), 2).await.unwrap();
    assert_eq!(node.0.groups(), vec![G1]);
    stop(node).await;
}
