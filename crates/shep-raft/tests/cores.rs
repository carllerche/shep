//! A node restarted with a different number of cores.

mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use shep_raft::{GroupId, ProposeError, RaftNode, RaftNodeError};
use shep_smp::{CoreId, SmpConfig};
use support::*;

const G1: GroupId = GroupId(1);

fn start(dir: &Path, cores: usize) -> Result<RaftNode<Recorder>, RaftNodeError> {
    let smp = SmpConfig {
        pin: false,
        ..SmpConfig::with_cores(cores)
    };
    RaftNode::start(test_config(1, dir), smp, |_| Recorder::default(), &[])
}

async fn stop(node: RaftNode<Recorder>) {
    node.shutdown().await;
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
#[tokio::test(flavor = "local")]
async fn fewer_cores_than_host_groups_is_refused() {
    init_tracing();
    let dir = tempfile::tempdir().unwrap();
    let node = start(dir.path(), 2).unwrap();
    assert_eq!(
        node.create_group_on(G1, CoreId(1)).await.unwrap(),
        CoreId(1)
    );
    let members = BTreeMap::from([(1, "127.0.0.1:1".to_string())]);
    node.initialize_group(G1, members).await.unwrap();
    write(&node, "kept").await;
    stop(node).await;

    match start(dir.path(), 1) {
        Err(RaftNodeError::CoreRemoved {
            core: 1,
            groups: 1,
            cores: 1,
        }) => {}
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("started without core 1's groups"),
    }

    // More cores is fine; the group stays on core 1.
    let node = start(dir.path(), 3).unwrap();
    assert_eq!(node.core_of(G1), Some(CoreId(1)));
    write(&node, "more").await;
    let entries = node
        .with_state_machine(G1, |sm| sm.entries.clone())
        .await
        .unwrap();
    assert_eq!(entries, vec![b"kept".to_vec(), b"more".to_vec()]);
    stop(node).await;

    // Core 2 never hosted a group, so it can go again.
    let node = start(dir.path(), 2).unwrap();
    assert_eq!(node.groups(), vec![G1]);
    stop(node).await;
}
