//! Appending, reading, rolling and flushing.

mod util;

use std::time::Duration;

use shep_futures::future::join_all;
use shep_storage::{Error, LogConfig, Record};
use util::*;

// Adapted from storage_e2e_test.cc: test_assigning_offsets_in_single_segment_log.
#[tokio::test(flavor = "local")]
async fn append_and_read_back() {
    let mut env = Env::new().await;
    let log = env.open(LogConfig::default()).await;
    assert_eq!(log.next_index(), 0);
    assert_eq!(log.last_index(), None);
    assert_eq!(read_all(&log).await, vec![]);

    log.append(recs(0..10, 1)).await.unwrap();
    assert_eq!(log.next_index(), 10);
    assert_eq!(log.last_index(), Some(9));
    assert_eq!(log.last_term(), Some(1));
    assert_eq!(read_all(&log).await, recs(0..10, 1));
    assert_eq!(log.read(3..6, usize::MAX).await.unwrap(), recs(3..6, 1));
    assert_eq!(log.read(3..=6, usize::MAX).await.unwrap(), recs(3..7, 1));
    assert_eq!(log.read(8.., usize::MAX).await.unwrap(), recs(8..10, 1));
    assert_eq!(log.read(10.., usize::MAX).await.unwrap(), vec![]);
    assert_eq!(log.read(5..5, usize::MAX).await.unwrap(), vec![]);
    assert_eq!(log.segments().len(), 1);

    let log = env.reopen(log, LogConfig::default()).await;
    assert_eq!(read_all(&log).await, recs(0..10, 1));
}

// Adapted from storage_e2e_test.cc: append_twice_to_same_segment and
// test_reading_range_from_a_log.
#[tokio::test(flavor = "local")]
async fn append_and_read_from_disk_across_segments() {
    let mut env = Env::new().await;
    // No cache, so every read goes to disk.
    let log = env.open(small()).await;
    for i in 0..100 {
        log.append([rec(i, 1)]).await.unwrap();
    }
    assert!(log.segments().len() > 3);
    assert_eq!(read_all(&log).await, recs(0..100, 1));
    for (lo, hi) in [(0, 1), (17, 18), (30, 70), (31, 32), (99, 100), (0, 100)] {
        assert_eq!(log.read(lo..hi, usize::MAX).await.unwrap(), recs(lo..hi, 1));
    }
    let log = env.reopen(log, small()).await;
    assert_eq!(read_all(&log).await, recs(0..100, 1));
}

#[tokio::test(flavor = "local")]
async fn out_of_order_appends_fail() {
    let env = Env::new().await;
    let log = env.open(LogConfig::default()).await;
    log.append(recs(0..3, 2)).await.unwrap();
    assert!(matches!(
        log.append([rec(4, 2)]).await,
        Err(Error::OutOfOrder(_))
    ));
    assert!(matches!(
        log.append([rec(2, 2)]).await,
        Err(Error::OutOfOrder(_))
    ));
    assert!(matches!(
        log.append([rec(3, 1)]).await,
        Err(Error::OutOfOrder(_))
    ));
    // A bad batch appends nothing.
    assert!(matches!(
        log.append([rec(3, 2), rec(5, 2)]).await,
        Err(Error::OutOfOrder(_))
    ));
    assert_eq!(log.next_index(), 3);
}

// Adapted from storage_e2e_test.cc: test_segment_rolling and
// check_max_segment_size.
#[tokio::test(flavor = "local")]
async fn rolls_on_size() {
    let mut env = Env::new().await;
    let config = small();
    let log = env.open(config.clone()).await;
    log.append(recs(0..200, 1)).await.unwrap();
    let segs = log.segments();
    assert!(segs.len() > 5, "{segs:?}");
    for pair in segs.windows(2) {
        assert_eq!(pair[0].end, pair[1].base);
        // A segment rolls once it reaches the limit, so it overshoots by
        // less than one record.
        assert!(pair[0].size >= config.segment_size);
        assert!(pair[0].size < config.segment_size + REC_LEN);
    }
    assert_eq!(segs[0].base, 0);
    assert_eq!(segs.last().unwrap().end, 200);
    assert_eq!(env.segment_files().len(), segs.len());
    let log = env.reopen(log, config).await;
    assert_eq!(log.segments(), segs);
}

// Adapted from storage_e2e_test.cc: test_rolling_term and
// test_append_batches_from_multiple_terms.
#[tokio::test(flavor = "local")]
async fn rolls_on_term_and_answers_term_at() {
    let mut env = Env::new().await;
    let log = env.open(LogConfig::default()).await;
    let mut all = recs(0..3, 1);
    all.extend(recs(3..4, 2));
    all.extend(recs(4..9, 5));
    log.append(all.clone()).await.unwrap();
    log.append(recs(9..10, 5)).await.unwrap();
    all.extend(recs(9..10, 5));

    let segs: Vec<_> = log
        .segments()
        .iter()
        .map(|s| (s.base, s.term, s.end))
        .collect();
    assert_eq!(segs, vec![(0, 1, 3), (3, 2, 4), (4, 5, 10)]);
    let terms: Vec<_> = (0..11).map(|i| log.term_at(i)).collect();
    let expect: Vec<_> = [1, 1, 1, 2, 5, 5, 5, 5, 5, 5]
        .into_iter()
        .map(Some)
        .chain([None])
        .collect();
    assert_eq!(terms, expect);
    assert_eq!(log.last_term(), Some(5));

    let names: Vec<String> = env
        .segment_files()
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        [
            "00000000000000000000-1.log",
            "00000000000000000003-2.log",
            "00000000000000000004-5.log"
        ]
    );

    let log = env.reopen(log, LogConfig::default()).await;
    assert_eq!(read_all(&log).await, all);
    assert_eq!((0..11).map(|i| log.term_at(i)).collect::<Vec<_>>(), expect);
}

#[tokio::test(flavor = "local")]
async fn first_append_after_open_starts_a_segment() {
    let mut env = Env::new().await;
    let log = env.open(LogConfig::default()).await;
    log.append(recs(0..5, 1)).await.unwrap();
    let log = env.reopen(log, LogConfig::default()).await;
    log.append(recs(5..8, 1)).await.unwrap();
    let segs: Vec<_> = log.segments().iter().map(|s| (s.base, s.end)).collect();
    assert_eq!(segs, vec![(0, 5), (5, 8)]);
    assert_eq!(read_all(&log).await, recs(0..8, 1));
}

#[tokio::test(flavor = "local")]
async fn read_limits_bytes_but_returns_at_least_one() {
    let env = Env::new().await;
    for config in [LogConfig::default(), small()] {
        let log = env.open(config).await;
        if log.next_index() == 0 {
            log.append(recs(0..50, 1)).await.unwrap();
        }
        assert_eq!(log.read(0.., 1).await.unwrap(), recs(0..1, 1));
        assert_eq!(log.read(0.., 0).await.unwrap(), recs(0..1, 1));
        let n = log.read(10.., 5 * REC_LEN as usize).await.unwrap();
        assert_eq!(n, recs(10..15, 1));
        let n = log.read(10.., 5 * REC_LEN as usize + 1).await.unwrap();
        assert_eq!(n, recs(10..16, 1));
        log.close().await.unwrap();
    }
}

#[tokio::test(flavor = "local")]
async fn large_records_span_chunks_and_reads() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    let big: Vec<Record> = (0..6)
        .map(|i| {
            Record::new(
                i,
                1,
                0,
                payload(i, [0, 1, 511, 5000, 100_000, 3][i as usize]),
            )
        })
        .collect();
    log.append(big.clone()).await.unwrap();
    assert_eq!(read_all(&log).await, big);
    assert_eq!(log.read(4..5, 0).await.unwrap(), big[4..5]);
    let log = env.reopen(log, small()).await;
    assert_eq!(read_all(&log).await, big);
    assert_eq!(log.read(3.., 0).await.unwrap(), big[3..4]);
}

// Adapted from storage_e2e_test.cc: test_reading_range_from_a_log_with_write_caching.
#[tokio::test(flavor = "local")]
async fn appends_are_readable_before_they_are_written() {
    let env = Env::new().await;
    for cache_bytes in [0, 1 << 20] {
        let config = LogConfig {
            cache_bytes,
            ..LogConfig::default()
        };
        let log = env.open(config).await;
        let base = log.next_index();
        log.append(recs(base..base + 5, 1)).await.unwrap();
        // Write-behind: the records sit in a buffer, not yet given to the OS.
        let o = log.offsets();
        assert_eq!(o.next, base + 5);
        assert_eq!(o.written, base);
        assert_eq!(o.flushed, base);
        assert_eq!(
            log.read(base.., usize::MAX).await.unwrap(),
            recs(base..base + 5, 1)
        );
        log.close().await.unwrap();
    }
}

// Adapted from segment_appender_test.cc: TestFlushesAreMerged and
// TestConcurrentFlushes.
#[tokio::test(flavor = "local")]
async fn concurrent_flushes_share_one_sync() {
    let env = Env::new().await;
    let log = env.open(LogConfig::default()).await;
    log.append(recs(0..100, 1)).await.unwrap();
    let before = env.io.stats().fsyncs;
    let results = join_all((0..50).map(|_| log.flush())).await;
    assert!(results.iter().all(|r| r.is_ok()));
    assert_eq!(env.io.stats().fsyncs - before, 1);
    assert_eq!(log.offsets().flushed, 100);
    assert_eq!(log.offsets().written, 100);

    // Nothing new: no sync at all.
    log.flush().await.unwrap();
    assert_eq!(env.io.stats().fsyncs - before, 1);
}

#[tokio::test(flavor = "local")]
async fn flush_after_a_sync_starts_waits_for_another() {
    let env = Env::new().await;
    let log = env.open(LogConfig::default()).await;
    log.append(recs(0..10, 1)).await.unwrap();
    let before = env.io.stats().fsyncs;
    let mut first = std::pin::pin!(log.flush());
    // Start the first sync.
    assert!(shep_futures::poll!(first.as_mut()).is_pending());
    log.append(recs(10..20, 1)).await.unwrap();
    let second = log.flush();
    let third = log.flush();
    let (a, b, c) = tokio::join!(first, second, third);
    a.unwrap();
    b.unwrap();
    c.unwrap();
    // One sync for the first flush, one shared by the other two.
    assert_eq!(env.io.stats().fsyncs - before, 2);
    assert_eq!(log.offsets().flushed, 20);
}

#[tokio::test(flavor = "local")]
async fn flushed_watch_follows_flushes() {
    let env = Env::new().await;
    let log = env.open(LogConfig::default()).await;
    let mut rx = log.subscribe_flushed();
    assert_eq!(*rx.borrow_and_update(), 0);
    log.append(recs(0..4, 1)).await.unwrap();
    assert!(!rx.has_changed().unwrap());
    log.flush().await.unwrap();
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), 4);
    log.truncate_suffix(2).await.unwrap();
    assert_eq!(*rx.borrow_and_update(), 2);
}

// Adapted from segment_appender_test.cc's inactive-timer behaviour.
#[tokio::test(flavor = "local")]
async fn idle_buffers_are_written_without_syncing() {
    let env = Env::new().await;
    let config = LogConfig {
        idle_write: Duration::from_millis(20),
        ..LogConfig::default()
    };
    let log = env.open(config).await;
    log.append(recs(0..3, 1)).await.unwrap();
    let before = env.io.stats();
    assert_eq!(log.offsets().written, 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let o = log.offsets();
    assert_eq!(o.written, 3);
    assert_eq!(o.flushed, 0);
    assert_eq!(env.io.stats().fsyncs, before.fsyncs);
    assert!(env.io.stats().writes > before.writes);
}

#[tokio::test(flavor = "local")]
async fn full_chunks_are_written_without_flushing() {
    let env = Env::new().await;
    let log = env.open(small()).await;
    // Ten records span two 512-byte chunks.
    log.append(recs(0..10, 1)).await.unwrap();
    // Wait for dispatched chunks without dispatching the partial head.
    env.pool.drain().await;
    env.io.drain().await.unwrap();
    let o = log.offsets();
    assert!(o.written > 0 && o.written < 10, "{o:?}");
    assert_eq!(o.flushed, 0);
}

// Adapted from log_segment_appender_test.cc: test_fallocate_size.
#[tokio::test(flavor = "local")]
async fn segments_are_preallocated_and_trimmed_on_close() {
    let env = Env::new().await;
    let config = LogConfig {
        prealloc_step: 1 << 20,
        ..LogConfig::default()
    };
    let log = env.open(config).await;
    log.append(recs(0..10, 1)).await.unwrap();
    log.flush().await.unwrap();
    let path = env.segment_files()[0].clone();
    assert_eq!(file_len(&path), 1 << 20);
    log.close().await.unwrap();
    assert_eq!(file_len(&path), 10 * REC_LEN);
}

#[tokio::test(flavor = "local")]
async fn closed_log_rejects_operations() {
    let env = Env::new().await;
    let log = env.open(LogConfig::default()).await;
    log.append(recs(0..2, 1)).await.unwrap();
    log.close().await.unwrap();
    log.close().await.unwrap();
    assert!(matches!(log.append([rec(2, 1)]).await, Err(Error::Closed)));
    assert!(matches!(log.read(0.., 10).await, Err(Error::Closed)));
    assert!(matches!(log.truncate_suffix(1).await, Err(Error::Closed)));
}

// Adapted from index_state_test.cc (SerdeBasic, SerdeCrc): the index is
// written with a checksum when a segment closes, reloaded on open, and
// rebuilt when it's damaged or stale.
#[tokio::test(flavor = "local")]
async fn index_files_are_written_and_reloaded() {
    let mut env = Env::new().await;
    let config = LogConfig {
        segment_size: 256 << 10,
        ..LogConfig::default()
    };
    let log = env.open(config.clone()).await;
    // Enough for several sparse-index entries per segment.
    log.append(recs(0..6000, 1)).await.unwrap();
    // Only closed segments have one.
    assert_eq!(env.index_files().len(), log.segments().len() - 1);
    let before = state(&log).await;
    let log = env.reopen(log, config.clone()).await;
    assert_eq!(env.index_files().len(), env.segment_files().len());
    assert_eq!(state(&log).await, before);
    for i in [0, 1, 250, 251, 2000, 3999, 5999] {
        assert_eq!(log.read(i..i + 1, 0).await.unwrap(), recs(i..i + 1, 1));
    }
    log.close().await.unwrap();
    drop(log);

    // A torn index is rebuilt from the data.
    let index = env.index_files()[0].clone();
    flip(&index, 20);
    let log = env.open(config.clone()).await;
    assert_eq!(state(&log).await, before);
    log.close().await.unwrap();
    drop(log);

    // So is a missing one.
    std::fs::remove_file(&index).unwrap();
    let log = env.open(config).await;
    assert_eq!(state(&log).await, before);
    assert!(index.exists());
}

// Adapted from storage_e2e_test.cc: the clean_segment_key handling.
#[tokio::test(flavor = "local")]
async fn clean_close_skips_rescanning_the_last_segment() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..30, 1)).await.unwrap();
    let last = log.segments().last().unwrap().base;
    let log = env.reopen(log, small()).await;

    // Damage the last segment's last record. A rescan would cut it off;
    // a trusted index doesn't look.
    log.close().await.unwrap();
    drop(log);
    let path = env.segment_files().last().unwrap().clone();
    let len = file_len(&path);
    flip(&path, len - 1);
    let before = env.io.stats().fsyncs;
    let log = env.open(small()).await;
    assert_eq!(env.io.stats().fsyncs, before, "no rescan, no repair");
    assert_eq!(log.next_index(), 30);
    assert_eq!(log.segments().last().unwrap().base, last);
    assert!(matches!(
        log.read(29.., 0).await,
        Err(Error::Corrupt { .. })
    ));
    assert_eq!(log.read(28..29, 0).await.unwrap(), recs(28..29, 1));

    // A truncation clears the marker first, so after a crash the same
    // segment is rescanned.
    log.truncate_suffix(29).await.unwrap();
    let before = env.io.stats().fsyncs;
    let log = env.crash(log, small()).await;
    assert!(env.io.stats().fsyncs > before);
    assert_eq!(read_all(&log).await, recs(0..29, 1));
}

#[tokio::test(flavor = "local")]
async fn crash_without_close_rescans() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..30, 1)).await.unwrap();
    log.flush().await.unwrap();
    let before = env.io.stats().fsyncs;
    let log = env.crash(log, small()).await;
    // The rescan syncs what it recovered and writes its index.
    assert!(env.io.stats().fsyncs > before);
    assert_eq!(read_all(&log).await, recs(0..30, 1));
}

#[tokio::test(flavor = "local")]
async fn per_core_storage_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let storage = shep_storage::Storage::open(tmp.path(), Default::default())
        .await
        .unwrap();
    let a = storage.open_log(1).await.unwrap();
    let b = storage.open_log(2).await.unwrap();
    a.append(recs(0..3, 1)).await.unwrap();
    b.append(recs(0..5, 7)).await.unwrap();
    a.truncate_prefix(1).await.unwrap();
    a.close().await.unwrap();
    b.close().await.unwrap();
    drop((a, b, storage));

    let storage = shep_storage::Storage::open(tmp.path(), Default::default())
        .await
        .unwrap();
    let a = storage.open_log(1).await.unwrap();
    let b = storage.open_log(2).await.unwrap();
    assert_eq!(read_all(&a).await, recs(1..3, 1));
    assert_eq!(read_all(&b).await, recs(0..5, 7));
    assert!(storage.log_dir(1).exists());
}

#[tokio::test(flavor = "local")]
async fn canceled_append_waiters_do_not_leave_partial_records() {
    let env = Env::with_pool(1024).await;
    let log = env.open(small()).await;
    let record = shep_storage::Record::new(0, 1, 0, vec![7; 10000]);
    {
        let mut append = std::pin::pin!(log.append([record.clone()]));
        assert!(shep_futures::poll!(&mut append).is_pending());
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while log.next_index() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    log.append([rec(1, 1)]).await.unwrap();
    log.flush().await.unwrap();
    assert_eq!(read_all(&log).await, vec![record, rec(1, 1)]);
}

#[tokio::test(flavor = "local")]
async fn canceled_truncation_waiters_finish_the_metadata_transaction() {
    let env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..100, 1)).await.unwrap();
    log.flush().await.unwrap();
    {
        let mut truncate = std::pin::pin!(log.truncate_suffix(10));
        assert!(shep_futures::poll!(&mut truncate).is_pending());
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while log.next_index() != 10 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    log.append(recs(10..20, 2)).await.unwrap();
    log.flush().await.unwrap();
    assert_eq!(
        read_all(&log).await,
        recs(0..10, 1)
            .into_iter()
            .chain(recs(10..20, 2))
            .collect::<Vec<_>>()
    );
}
