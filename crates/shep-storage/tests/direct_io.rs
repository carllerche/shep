//! Exercise storage framing and recovery over aligned direct-I/O appends.
mod util;
use util::*;

#[tokio::test(flavor = "local")]
async fn direct_segments_recover_truncate_and_keep_appending() {
    let mut env = Env::with_mode(32768, true).await;
    let config = small();
    let log = env.open(config.clone()).await;
    log.append(recs(0..150, 1)).await.unwrap();
    log.flush().await.unwrap();
    assert_eq!(read_all(&log).await, recs(0..150, 1));
    let log = env.crash(log, config.clone()).await;
    assert_eq!(read_all(&log).await, recs(0..150, 1));
    log.truncate_suffix(73).await.unwrap();
    log.append(recs(73..110, 2)).await.unwrap();
    log.truncate_prefix(30).await.unwrap();
    let expected: Vec<_> = recs(30..73, 1)
        .into_iter()
        .chain(recs(73..110, 2))
        .collect();
    let log = env.reopen(log, config).await;
    assert_eq!(read_all(&log).await, expected);
    log.close().await.unwrap();
    env.pool.drain().await;
    env.io.shutdown().await.unwrap();
}
#[tokio::test(flavor = "local")]
async fn a_corrupt_direct_tail_is_cut_at_the_last_complete_record() {
    let mut env = Env::with_mode(32768, true).await;
    let log = env.open(small()).await;
    log.append(recs(0..15, 1)).await.unwrap();
    log.flush().await.unwrap();
    drop(log);
    env.pool.drain().await;
    flip(&env.segment_files()[0], 15 * REC_LEN - 1);
    env.restart_kv().await;
    let log = env.open(small()).await;
    assert_eq!(log.next_index(), 14);
    assert_eq!(read_all(&log).await, recs(0..14, 1));
    log.close().await.unwrap();
    env.io.shutdown().await.unwrap();
}
#[tokio::test(flavor = "local")]
async fn reads_race_segment_closes_without_observing_unwritten_data() {
    let env = Env::with_mode(32768, true).await;
    let log = env.open(small()).await;
    let write = async {
        for index in 0..100 {
            log.append([rec(index, index / 3 + 1)]).await.unwrap();
        }
        log.flush().await.unwrap();
    };
    let read = async {
        for _ in 0..500 {
            let end = log.next_index();
            let records = log.read(..end, usize::MAX).await.unwrap();
            assert_eq!(records.len(), end as usize);
            for (index, record) in records.iter().enumerate() {
                assert_eq!(*record, rec(index as u64, index as u64 / 3 + 1));
            }
            tokio::task::yield_now().await;
        }
    };
    tokio::join!(write, read);
    log.close().await.unwrap();
    env.io.shutdown().await.unwrap();
}
