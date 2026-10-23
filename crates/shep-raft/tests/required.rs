//! Groups the embedder requires: created on their core while the node
//! starts, so they exist before anything is served.

mod support;

use std::path::Path;

use shep_raft::{GroupId, RaftNode, RaftNodeError};
use shep_smp::{CoreId, SmpConfig, SmpError};
use support::*;

const G1: GroupId = GroupId(1);
const G2: GroupId = GroupId(2);

fn start(
    dir: &Path,
    cores: usize,
    required: &[(GroupId, CoreId)],
) -> Result<RaftNode<Recorder>, RaftNodeError> {
    let smp = SmpConfig {
        pin: false,
        ..SmpConfig::with_cores(cores)
    };
    RaftNode::start(test_config(1, dir), smp, |_| Recorder::default(), required)
}

#[tokio::test(flavor = "local")]
async fn required_groups_exist_once_started() {
    init_tracing();
    let dir = tempfile::tempdir().unwrap();
    let required = [(G1, CoreId(1)), (G2, CoreId(0))];
    let node = start(dir.path(), 2, &required).unwrap();
    assert_eq!(node.core_of(G1), Some(CoreId(1)));
    assert_eq!(node.core_of(G2), Some(CoreId(0)));
    node.shutdown().await;

    // Reopened, not created again; and still there without being required.
    let node = start(dir.path(), 2, &required).unwrap();
    let mut groups = node.groups();
    groups.sort();
    assert_eq!(groups, vec![G1, G2]);
    node.shutdown().await;
    let node = start(dir.path(), 2, &[]).unwrap();
    assert_eq!(node.core_of(G1), Some(CoreId(1)));
    node.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn a_required_group_on_a_missing_core_is_refused() {
    init_tracing();
    let dir = tempfile::tempdir().unwrap();
    match start(dir.path(), 2, &[(G1, CoreId(2))]) {
        Err(RaftNodeError::Smp(SmpError::NoSuchCore(CoreId(2)))) => {}
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("started with a group on core 2 of 2"),
    }
    // Refused before taking the directory.
    start(dir.path(), 2, &[]).unwrap().shutdown().await;
}

/// A group is on one core. Requiring it on another than the one it's on
/// fails the start, and frees the directory.
#[tokio::test(flavor = "local")]
async fn a_required_group_on_another_core_is_refused() {
    init_tracing();
    let dir = tempfile::tempdir().unwrap();
    let node = start(dir.path(), 2, &[]).unwrap();
    node.create_group_on(G1, CoreId(1)).await.unwrap();
    node.shutdown().await;

    match start(dir.path(), 2, &[(G1, CoreId(0))]) {
        Err(RaftNodeError::Raft(e)) => {
            assert!(e.contains("is required on core0 but is on core1"), "{e}")
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("started with group 1 on two cores"),
    }
    let node = start(dir.path(), 2, &[(G1, CoreId(1))]).unwrap();
    assert_eq!(node.core_of(G1), Some(CoreId(1)));
    node.shutdown().await;
}
