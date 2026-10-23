//! One node per data directory at a time.

mod support;

use shep_raft::{RaftNode, RaftNodeError};
use shep_smp::SmpConfig;
use support::*;

fn start(dir: &std::path::Path) -> Result<RaftNode<Recorder>, RaftNodeError> {
    let smp = SmpConfig {
        pin: false,
        ..SmpConfig::with_cores(1)
    };
    RaftNode::start(test_config(1, dir), smp, |_| Recorder::default(), &[])
}

/// A second node on a directory in use is refused; once the first shuts
/// down, the directory is free.
#[tokio::test(flavor = "local")]
async fn a_data_directory_has_one_node() {
    let dir = tempfile::tempdir().unwrap();
    let first = start(dir.path()).unwrap();

    match start(dir.path()) {
        Err(RaftNodeError::DataDirLocked(path)) => assert_eq!(path, dir.path().join("LOCK")),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("two nodes on one directory"),
    }

    first.shutdown().await;
    start(dir.path()).unwrap().shutdown().await;
}
