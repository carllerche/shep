//! Backpressure from the per-core write buffer pool.

mod util;

use std::sync::mpsc;
use std::time::Duration;

use shep_storage::LogConfig;
use util::*;

/// Stalls the disk worker until the returned sender is used or dropped.
fn stall(env: &Env) -> mpsc::Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();
    drop(env.disk.run(move || {
        let _ = rx.recv();
        Ok(())
    }));
    tx
}

fn config() -> LogConfig {
    LogConfig {
        chunk_size: 1024,
        ..small()
    }
}

// Adapted from segment_appender_test.cc's chunk-cache exhaustion and
// storage_resources.cc's write-buffer accounting.
#[tokio::test(flavor = "local")]
async fn appends_wait_for_pool_space() {
    let env = Env::with_pool(2048).await;
    let log = env.open(config()).await;
    assert_eq!(env.pool.available(), 2048);
    // The first append creates the segment, which needs the worker.
    log.append([rec(0, 1)]).await.unwrap();
    let unstall = stall(&env);

    // Two chunks' worth fills the pool; the writes can't complete.
    let mut append = std::pin::pin!(log.append(recs(1..30, 1)));
    tokio::select! {
        _ = &mut append => panic!("append should wait for buffer space"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    assert_eq!(env.pool.available(), 0);
    assert!(log.next_index() > 1 && log.next_index() < 30);

    drop(unstall);
    append.await.unwrap();
    assert_eq!(log.next_index(), 30);
    log.flush().await.unwrap();
    assert_eq!(env.pool.available(), 2048, "buffers go back once written");
    assert_eq!(read_all(&log).await, recs(0..30, 1));
}

#[tokio::test(flavor = "local")]
async fn logs_on_a_core_share_the_pool() {
    let env = Env::with_pool(2048).await;
    let a = env.open(config()).await;
    let b = shep_storage::Log::open(
        &env.disk,
        &env.pool,
        &env.kv,
        env.tmp.path().join("b"),
        2,
        config(),
    )
    .await
    .unwrap();
    // Each log holds a chunk; a third can't be had.
    a.append([rec(0, 1)]).await.unwrap();
    b.append([rec(0, 1)]).await.unwrap();
    assert_eq!(env.pool.available(), 0);
    let unstall = stall(&env);
    let mut more = std::pin::pin!(a.append(recs(1..10, 1)));
    tokio::select! {
        _ = &mut more => panic!("append should wait for buffer space"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    drop(unstall);
    // b's partial chunk is only returned once written.
    b.flush().await.unwrap();
    more.await.unwrap();
    a.flush().await.unwrap();
    assert_eq!(env.pool.available(), 2048);
}

#[tokio::test(flavor = "local")]
async fn records_larger_than_the_pool_still_go_through() {
    let env = Env::with_pool(1024).await;
    let log = env.open(config()).await;
    let big = shep_storage::Record::new(0, 1, 0, payload(0, 10_000));
    log.append([big.clone()]).await.unwrap();
    log.flush().await.unwrap();
    assert_eq!(read_all(&log).await, vec![big]);
    assert_eq!(env.pool.available(), 1024);
}
