//! One node per data directory at a time.

mod support;

use shep_raft::{RaftNode, RaftNodeError};
use shep_smp::{Smp, SmpConfig};
use support::*;

async fn start(dir: &std::path::Path) -> (Result<RaftNode<Recorder>, RaftNodeError>, Smp) {
    let smp = Smp::start(SmpConfig {
        pin: false,
        ..SmpConfig::with_cores(1)
    })
    .unwrap();
    let node = RaftNode::start(test_config(1, dir), smp.clone(), |_| Recorder::default()).await;
    (node, smp)
}

/// A second node on a directory in use is refused; once the first shuts
/// down, the directory is free.
#[tokio::test(flavor = "multi_thread")]
async fn a_data_directory_has_one_node() {
    let dir = tempfile::tempdir().unwrap();
    let (first, smp1) = start(dir.path()).await;
    let first = first.unwrap();

    let (second, smp2) = start(dir.path()).await;
    match second {
        Err(RaftNodeError::DataDirLocked(path)) => assert_eq!(path, dir.path().join("LOCK")),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("two nodes on one directory"),
    }
    smp2.shutdown().await;

    first.shutdown().await;
    smp1.shutdown().await;
    let (again, smp) = start(dir.path()).await;
    again.unwrap().shutdown().await;
    smp.shutdown().await;
}
