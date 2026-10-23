//! Backpressure and pressure-triggered writeback from the shared buffer pool.
mod util;
use shep_storage::LogConfig;
use util::*;

fn config() -> LogConfig {
    LogConfig {
        chunk_size: 1024,
        ..small()
    }
}

#[tokio::test(flavor = "local")]
async fn large_appends_stay_within_the_pool_budget() {
    let env = Env::with_pool(2048).await;
    let log = env.open(config()).await;
    log.append([rec(0, 1)]).await.unwrap();
    let mut append = std::pin::pin!(log.append(recs(1..30, 1)));
    // Accept the transaction, which then proceeds under the shared budget.
    assert!(shep_futures::poll!(&mut append).is_pending());
    append.await.unwrap();
    log.flush().await.unwrap();
    assert_eq!(env.pool.available(), 2048);
    assert!(env.pool.allocated() <= 2048);
    assert_eq!(read_all(&log).await, recs(0..30, 1));
}

#[tokio::test(flavor = "local")]
async fn logs_on_a_core_share_the_pool() {
    let env = Env::with_pool(2048).await;
    let a = env.open(config()).await;
    let b = shep_storage::Log::open(
        &env.io,
        &env.pool,
        &env.kv,
        env.tmp.path().join("b"),
        2,
        config(),
    )
    .await
    .unwrap();
    a.append([rec(0, 1)]).await.unwrap();
    b.append([rec(0, 1)]).await.unwrap();
    assert_eq!(env.pool.available(), 0);
    // Pool pressure flushes both heads even though their idle timeout is an hour.
    tokio::time::timeout(std::time::Duration::from_secs(2), a.append(recs(1..10, 1)))
        .await
        .unwrap()
        .unwrap();
    a.flush().await.unwrap();
    b.flush().await.unwrap();
    assert_eq!(env.pool.available(), 2048);
    assert_eq!(read_all(&a).await, recs(0..10, 1));
    assert_eq!(read_all(&b).await, recs(0..1, 1));
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
