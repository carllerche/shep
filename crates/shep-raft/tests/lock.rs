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
#[tokio::test(flavor = "local")]
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
    let smp = smp2.clone();
    let (done, finished) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        smp.shutdown();
        let _ = done.send(());
    });
    let _ = finished.await;

    first.shutdown().await;
    let smp = smp1.clone();
    let (done, finished) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        smp.shutdown();
        let _ = done.send(());
    });
    let _ = finished.await;
    let (again, smp) = start(dir.path()).await;
    again.unwrap().shutdown().await;
    let smp = smp.clone();
    let (done, finished) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        smp.shutdown();
        let _ = done.send(());
    });
    let _ = finished.await;
}
