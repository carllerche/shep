//! Suffix truncation (Raft conflicts) and prefix truncation (snapshots).

mod util;

use shep_storage::{Error, LogConfig};
use util::*;

/// With `small()`, a segment fills after this many `rec`s.
const PER_SEGMENT: u64 = 32;

// Adapted from log_truncate_test.cc: test_truncate_in_the_middle_of_segment.
#[tokio::test]
async fn truncate_suffix_inside_the_last_segment() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..40, 1)).await.unwrap();
    log.truncate_suffix(37).await.unwrap();
    assert_eq!(log.next_index(), 37);
    assert_eq!(read_all(&log).await, recs(0..37, 1));
    let o = log.offsets();
    assert_eq!((o.written, o.flushed), (37, 37), "durable before returning");
    assert_eq!(log.term_at(37), None);

    // Appending carries on from the cut, in a new term.
    log.append(recs(37..39, 2)).await.unwrap();
    let mut expect = recs(0..37, 1);
    expect.extend(recs(37..39, 2));
    assert_eq!(read_all(&log).await, expect);
    let before = state(&log).await;
    let log = env.reopen(log, small()).await;
    assert_eq!(state(&log).await, before);
}

// Adapted from log_truncate_test.cc: test_truncate_middle_of_old_segment.
#[tokio::test]
async fn truncate_suffix_inside_an_older_segment() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..100, 1)).await.unwrap();
    assert_eq!(log.segments().len(), 4);
    log.truncate_suffix(40).await.unwrap();
    assert_eq!(log.segments().len(), 2);
    assert_eq!(env.segment_files().len(), 2);
    assert_eq!(read_all(&log).await, recs(0..40, 1));
    assert_eq!(file_len(&env.segment_files()[1]), 8 * REC_LEN);
    let before = state(&log).await;
    let log = env.crash(log, small()).await;
    assert_eq!(state(&log).await, before);
}

#[tokio::test]
async fn truncate_suffix_at_a_segment_boundary() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..100, 1)).await.unwrap();
    log.truncate_suffix(2 * PER_SEGMENT).await.unwrap();
    let segs: Vec<_> = log.segments().iter().map(|s| (s.base, s.end)).collect();
    assert_eq!(segs, vec![(0, 32), (32, 64)]);
    assert_eq!(env.segment_files().len(), 2);
    assert_eq!(read_all(&log).await, recs(0..64, 1));

    // A new segment may reuse the deleted one's name.
    log.append(recs(64..66, 1)).await.unwrap();
    let log = env.reopen(log, small()).await;
    assert_eq!(read_all(&log).await, recs(0..66, 1));
}

// Adapted from log_truncate_test.cc: test_truncate_whole and
// truncate_whole_log_and_then_again.
#[tokio::test]
async fn truncate_suffix_to_zero() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..100, 1)).await.unwrap();
    log.truncate_suffix(0).await.unwrap();
    assert_eq!(log.next_index(), 0);
    assert_eq!(log.last_index(), None);
    assert!(log.segments().is_empty());
    assert!(env.segment_files().is_empty());
    assert!(env.index_files().is_empty());
    assert_eq!(read_all(&log).await, vec![]);
    log.truncate_suffix(0).await.unwrap();

    let log = env.crash(log, small()).await;
    assert_eq!(log.next_index(), 0);
    log.append(recs(0..3, 7)).await.unwrap();
    log.truncate_suffix(0).await.unwrap();
    log.append(recs(0..3, 8)).await.unwrap();
    let log = env.reopen(log, small()).await;
    assert_eq!(read_all(&log).await, recs(0..3, 8));
}

// Adapted from log_truncate_test.cc: test_truncate_empty_log and
// test_truncate_last_single_record_batch.
#[tokio::test]
async fn truncate_suffix_edges() {
    let env = Env::new().await;
    let log = env.open(small()).await;
    log.truncate_suffix(0).await.unwrap();
    log.truncate_suffix(5).await.unwrap();
    log.append(recs(0..5, 1)).await.unwrap();
    // At or past the end: nothing to do.
    log.truncate_suffix(5).await.unwrap();
    log.truncate_suffix(50).await.unwrap();
    assert_eq!(log.next_index(), 5);
    log.truncate_suffix(4).await.unwrap();
    assert_eq!(read_all(&log).await, recs(0..4, 1));
}

// Adapted from storage_e2e_test.cc: test_truncation_with_write_caching.
#[tokio::test]
async fn truncate_suffix_of_unwritten_appends() {
    let mut env = Env::new().await;
    let config = LogConfig {
        cache_bytes: 1 << 20,
        ..LogConfig::default()
    };
    let log = env.open(config.clone()).await;
    log.append(recs(0..10, 1)).await.unwrap();
    assert_eq!(log.offsets().written, 0);
    log.truncate_suffix(6).await.unwrap();
    assert_eq!(read_all(&log).await, recs(0..6, 1));
    log.append(recs(6..8, 2)).await.unwrap();
    let mut expect = recs(0..6, 1);
    expect.extend(recs(6..8, 2));
    assert_eq!(read_all(&log).await, expect);
    let log = env.reopen(log, config).await;
    assert_eq!(read_all(&log).await, expect);
}

// Adapted from log_truncate_test.cc: truncate_before_read.
#[tokio::test]
async fn reads_after_truncation_see_the_new_suffix() {
    let env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..50, 1)).await.unwrap();
    assert_eq!(log.read(30..50, usize::MAX).await.unwrap(), recs(30..50, 1));
    log.truncate_suffix(35).await.unwrap();
    assert_eq!(log.read(30..50, usize::MAX).await.unwrap(), recs(30..35, 1));
    log.append(recs(35..40, 2)).await.unwrap();
    let mut expect = recs(30..35, 1);
    expect.extend(recs(35..40, 2));
    assert_eq!(log.read(30..50, usize::MAX).await.unwrap(), expect);
}

#[tokio::test]
async fn prefix_truncate_inside_a_segment() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..100, 1)).await.unwrap();
    log.truncate_prefix(40).await.unwrap();
    assert_eq!(log.start_index(), 40);
    // Only whole segments go.
    let segs: Vec<_> = log.segments().iter().map(|s| (s.base, s.end)).collect();
    assert_eq!(segs, vec![(32, 64), (64, 96), (96, 100)]);
    assert_eq!(env.segment_files().len(), 3);
    assert_eq!(read_all(&log).await, recs(40..100, 1));
    assert_eq!(log.read(40..41, 0).await.unwrap(), recs(40..41, 1));
    for lo in [0, 32, 39] {
        assert!(matches!(
            log.read(lo..50, usize::MAX).await,
            Err(Error::BelowStart { start: 40, .. })
        ));
    }
    assert_eq!(log.term_at(39), None);
    assert_eq!(log.term_at(40), Some(1));
    // Moving the start back is a no-op.
    log.truncate_prefix(10).await.unwrap();
    assert_eq!(log.start_index(), 40);

    let before = state(&log).await;
    let log = env.reopen(log, small()).await;
    assert_eq!(state(&log).await, before);
    let log = env.crash(log, small()).await;
    assert_eq!(state(&log).await, before);
    assert!(matches!(
        log.truncate_suffix(39).await,
        Err(Error::BelowStart { .. })
    ));
}

#[tokio::test]
async fn prefix_truncate_at_a_segment_boundary() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..100, 1)).await.unwrap();
    log.truncate_prefix(64).await.unwrap();
    let segs: Vec<_> = log.segments().iter().map(|s| (s.base, s.end)).collect();
    assert_eq!(segs, vec![(64, 96), (96, 100)]);
    let log = env.reopen(log, small()).await;
    assert_eq!(read_all(&log).await, recs(64..100, 1));
}

#[tokio::test]
async fn prefix_truncate_past_the_end_empties_the_log() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..50, 1)).await.unwrap();
    log.truncate_prefix(70).await.unwrap();
    assert!(log.segments().is_empty());
    assert!(env.segment_files().is_empty());
    let o = log.offsets();
    assert_eq!((o.start, o.next, o.flushed), (70, 70, 70));
    assert_eq!(log.last_index(), None);
    assert_eq!(read_all(&log).await, vec![]);
    assert!(log.append([rec(50, 1)]).await.is_err());

    let log = env.crash(log, small()).await;
    assert_eq!(log.next_index(), 70);
    log.append(recs(70..75, 2)).await.unwrap();
    let log = env.reopen(log, small()).await;
    assert_eq!(read_all(&log).await, recs(70..75, 2));
    assert_eq!(log.segments()[0].base, 70);
}

#[tokio::test]
async fn prefix_truncate_an_empty_log_sets_where_it_starts() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.truncate_prefix(1).await.unwrap();
    assert_eq!(log.next_index(), 1);
    log.append(recs(1..4, 1)).await.unwrap();
    let log = env.reopen(log, small()).await;
    assert_eq!(read_all(&log).await, recs(1..4, 1));
}

// Adapted from log_truncate_test.cc: test_prefix_truncate_then_truncate_all.
#[tokio::test]
async fn prefix_truncate_then_truncate_all() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..100, 1)).await.unwrap();
    log.truncate_prefix(50).await.unwrap();
    log.truncate_suffix(50).await.unwrap();
    assert_eq!(log.next_index(), 50);
    assert_eq!(read_all(&log).await, vec![]);
    log.append(recs(50..52, 2)).await.unwrap();
    let log = env.reopen(log, small()).await;
    assert_eq!(read_all(&log).await, recs(50..52, 2));
    assert_eq!(log.start_index(), 50);
}

#[tokio::test]
async fn prefix_truncate_flushes_what_it_keeps() {
    let mut env = Env::new().await;
    let log = env.open(LogConfig::default()).await;
    log.append(recs(0..10, 1)).await.unwrap();
    log.truncate_prefix(5).await.unwrap();
    assert!(log.offsets().flushed >= 5);
    let log = env.crash(log, LogConfig::default()).await;
    let next = log.next_index();
    assert!(next >= 5);
    assert_eq!(read_all(&log).await, recs(5..next, 1));
}

#[tokio::test]
async fn crash_mid_prefix_truncation_finishes_on_open() {
    // The start was persisted but the segments below it survived.
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..100, 1)).await.unwrap();
    log.close().await.unwrap();
    drop(log);
    let saved: Vec<_> = env
        .segment_files()
        .into_iter()
        .map(|p| (p.clone(), std::fs::read(&p).unwrap()))
        .collect();
    let log = env.open(small()).await;
    log.truncate_prefix(70).await.unwrap();
    drop(log);
    for (p, b) in &saved {
        std::fs::write(p, b).unwrap();
    }
    assert_eq!(env.segment_files().len(), 4);
    let log = env.open(small()).await;
    let log = env.crash(log, small()).await;
    assert_eq!(log.start_index(), 70);
    assert_eq!(env.segment_files().len(), 2);
    assert_eq!(read_all(&log).await, recs(70..100, 1));
}

/// A read of retained entries never fails because a suffix truncation is
/// under way: the segment's file shrinks before its size in memory does, so
/// readers wait for the whole truncation.
#[tokio::test]
async fn reads_of_retained_entries_race_suffix_truncation() {
    for _ in 0..20 {
        let env = Env::new().await;
        let log = env.open(small()).await;
        log.append(recs(0..10, 1)).await.unwrap();
        log.flush().await.unwrap();
        let done = std::cell::Cell::new(false);
        let truncate = async {
            log.truncate_suffix(2).await.unwrap();
            done.set(true);
        };
        let reads = async {
            let mut n = 0;
            while !done.get() || n == 0 {
                assert_eq!(log.read(0..1, usize::MAX).await.unwrap(), recs(0..1, 1));
                n += 1;
            }
        };
        tokio::join!(truncate, reads);
        assert_eq!(read_all(&log).await, recs(0..2, 1));
    }
}
