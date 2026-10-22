//! Crash recovery, on the real disk. A crash is dropping the log without
//! closing it; torn and corrupt writes are made by editing the files before
//! reopening.

mod util;

use std::path::PathBuf;

use shep_storage::{Error, HEADER_LEN, Log};
use util::*;

/// With `small()`, a segment fills after this many `rec`s.
const PER_SEGMENT: u64 = 32;

/// A log of records `0..n` in term 1, synced, then abandoned as by a crash.
/// Returns the segment files.
async fn crashed_log(env: &mut Env, n: u64) -> Vec<PathBuf> {
    let log = env.open(small()).await;
    log.append(recs(0..n, 1)).await.unwrap();
    log.flush().await.unwrap();
    assert_eq!(log.segments()[0].end, PER_SEGMENT.min(n));
    drop(log);
    env.disk.run(|| Ok(())).await.unwrap();
    env.segment_files()
}

/// Byte position of record `i` in its segment.
fn pos(i: u64) -> u64 {
    (i % PER_SEGMENT) * REC_LEN
}

async fn reopen(env: &mut Env) -> Log {
    env.restart_kv().await;
    env.open(small()).await
}

async fn reopen_err(env: &mut Env) -> Error {
    env.restart_kv().await;
    match env.try_open(small()).await {
        Ok(_) => panic!("open should fail"),
        Err(e) => e,
    }
}

// Adapted from storage_e2e_test.cc: committed_offset_updates. Only flushed
// data is promised; whatever else survives is a prefix of what was appended.
#[tokio::test]
async fn crash_keeps_flushed_and_a_prefix_of_the_rest() {
    let mut env = Env::new().await;
    let log = env.open(small()).await;
    log.append(recs(0..50, 1)).await.unwrap();
    log.flush().await.unwrap();
    log.append(recs(50..80, 1)).await.unwrap();
    let log = env.crash(log, small()).await;
    let next = log.next_index();
    assert!((50..=80).contains(&next), "{next}");
    assert_eq!(read_all(&log).await, recs(0..next, 1));
    let o = log.offsets();
    assert_eq!((o.written, o.flushed), (next, next));

    // And the recovered log carries on.
    log.append(recs(next..next + 5, 2)).await.unwrap();
    let log = env.crash(log, small()).await;
    assert!(log.next_index() >= next);
    let mut expect = recs(0..next, 1);
    expect.extend(recs(next..log.next_index(), 2));
    assert_eq!(read_all(&log).await, expect);
}

// Adapted from storage_e2e_test.cc: empty_segment_recovery (a preallocated,
// never-written tail reads as zeros).
#[tokio::test]
async fn zeroed_preallocated_tail_is_trimmed() {
    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 40).await;
    let last = files.last().unwrap();
    let data = 8 * REC_LEN;
    assert!(file_len(last) > data, "preallocated");
    let log = reopen(&mut env).await;
    assert_eq!(read_all(&log).await, recs(0..40, 1));
    assert_eq!(file_len(last), data);
    log.append([rec(40, 1)]).await.unwrap();
    assert_eq!(read_all(&log).await, recs(0..41, 1));
}

// Adapted from log_replayer_test.cc: test_unrecovered_single_batch.
#[tokio::test]
async fn torn_header_is_truncated() {
    // Partly written: the rest of the header still zeros.
    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 40).await;
    poke(files.last().unwrap(), pos(39) + 10, &[0; HEADER_LEN - 10]);
    let log = reopen(&mut env).await;
    assert_eq!(read_all(&log).await, recs(0..39, 1));
    assert_eq!(file_len(files.last().unwrap()), pos(39));

    // Cut short: the file ends inside the header.
    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 40).await;
    set_len(files.last().unwrap(), pos(39) + 10);
    let log = reopen(&mut env).await;
    assert_eq!(read_all(&log).await, recs(0..39, 1));
}

// Adapted from log_replayer_test.cc: test_malformed_segment.
#[tokio::test]
async fn torn_payload_is_truncated() {
    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 40).await;
    poke(files.last().unwrap(), pos(39) + REC_LEN - 5, &[0; 5]);
    let log = reopen(&mut env).await;
    assert_eq!(read_all(&log).await, recs(0..39, 1));

    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 40).await;
    set_len(files.last().unwrap(), pos(39) + HEADER_LEN as u64 + 50);
    let log = reopen(&mut env).await;
    assert_eq!(read_all(&log).await, recs(0..39, 1));
}

// Adapted from log_replayer_test.cc: test_unrecovered_multiple_batches.
#[tokio::test]
async fn corruption_in_the_last_segment_drops_the_rest() {
    for at in [pos(35) + 3, pos(35) + 60] {
        // Header, then payload.
        let mut env = Env::new().await;
        let files = crashed_log(&mut env, 40).await;
        flip(files.last().unwrap(), at);
        let log = reopen(&mut env).await;
        assert_eq!(read_all(&log).await, recs(0..35, 1));
        assert_eq!(log.term_at(34), Some(1));
        assert_eq!(log.term_at(35), None);
        // Recovery is durable and idempotent.
        let log = env.crash(log, small()).await;
        assert_eq!(read_all(&log).await, recs(0..35, 1));
    }
}

#[tokio::test]
async fn index_discontinuity_ends_the_log() {
    // A well-formed record with the wrong index, as if a stale write
    // survived where a newer one was lost.
    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 40).await;
    let last = files.last().unwrap();
    let r38 = std::fs::read(last).unwrap()[pos(38) as usize..pos(39) as usize].to_vec();
    poke(last, pos(39), &r38);
    let log = reopen(&mut env).await;
    assert_eq!(read_all(&log).await, recs(0..39, 1));
}

// Adapted from segment_set_test.cc: recovery. Only the last segment can be
// torn by a crash; damage anywhere else is reported, not repaired.
#[tokio::test]
async fn corruption_in_a_closed_segment_is_an_error() {
    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 100).await;
    flip(&files[0], pos(10) + 60);
    // Closed segments are trusted through their index, so the damage shows
    // when the record is read.
    let log = reopen(&mut env).await;
    assert_eq!(log.read(0..10, usize::MAX).await.unwrap(), recs(0..10, 1));
    assert!(matches!(
        log.read(10..11, 0).await,
        Err(Error::Corrupt { .. })
    ));
    drop(log);

    // Without its index the segment is scanned, and the scan refuses it.
    std::fs::remove_file(files[0].with_extension("index")).unwrap();
    assert!(matches!(reopen_err(&mut env).await, Error::Corrupt { .. }));
}

#[tokio::test]
async fn bytes_appended_to_a_closed_segment_are_an_error() {
    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 100).await;
    let len = file_len(&files[1]);
    // Its index no longer matches its size, so it's scanned.
    set_len(&files[1], len + 7);
    poke(&files[1], len, &[1; 7]);
    assert!(matches!(reopen_err(&mut env).await, Error::Corrupt { .. }));
}

#[tokio::test]
async fn missing_segment_is_an_error() {
    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 100).await;
    std::fs::remove_file(&files[1]).unwrap();
    assert!(matches!(reopen_err(&mut env).await, Error::Corrupt { .. }));

    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 100).await;
    std::fs::remove_file(&files[0]).unwrap();
    assert!(matches!(reopen_err(&mut env).await, Error::Corrupt { .. }));
}

// Adapted from storage_e2e_test.cc: empty_segment_recovery.
#[tokio::test]
async fn empty_last_segment_is_removed() {
    for contents in [vec![], vec![0; 1024]] {
        // A crash right after rolling, which closes the previous segment
        // first, as closing the log does.
        let mut env = Env::new().await;
        let log = env.open(small()).await;
        log.append(recs(0..40, 1)).await.unwrap();
        log.close().await.unwrap();
        drop(log);
        let files = env.segment_files();
        let stray = env.log_dir().join("00000000000000000040-2.log");
        std::fs::write(&stray, contents).unwrap();
        let log = reopen(&mut env).await;
        assert!(!stray.exists());
        assert_eq!(env.segment_files(), files);
        assert_eq!(read_all(&log).await, recs(0..40, 1));
        log.append(recs(40..42, 2)).await.unwrap();
        assert_eq!(log.term_at(41), Some(2));
    }
}

#[tokio::test]
async fn whole_tail_segment_torn_away() {
    // The crash came right after rolling: the new segment's only record is
    // torn, which leaves it empty.
    let mut env = Env::new().await;
    let files = crashed_log(&mut env, 33).await;
    assert_eq!(files.len(), 2);
    set_len(&files[1], 50);
    let log = reopen(&mut env).await;
    assert_eq!(read_all(&log).await, recs(0..32, 1));
    assert_eq!(env.segment_files(), files[..1]);
}

#[tokio::test]
async fn stray_files_are_cleaned_up() {
    let mut env = Env::new().await;
    crashed_log(&mut env, 10).await;
    let orphan = env.log_dir().join("00000000000000000500-3.index");
    let tmp = env.log_dir().join("whatever.tmp");
    std::fs::write(&orphan, b"x").unwrap();
    std::fs::write(&tmp, b"x").unwrap();
    let log = reopen(&mut env).await;
    assert!(!orphan.exists());
    assert!(!tmp.exists());
    assert_eq!(read_all(&log).await, recs(0..10, 1));
}

// Adapted from log_truncate_test.cc: truncated_segment_recovery.
#[tokio::test]
async fn reopen_after_each_operation_yields_the_same_state() {
    enum Op {
        Append(std::ops::Range<u64>, u64),
        Suffix(u64),
        Prefix(u64),
    }
    let mut env = Env::new().await;
    let mut log = env.open(small()).await;
    let ops = [
        Op::Append(0..70, 1),
        Op::Append(70..75, 3),
        Op::Suffix(72),
        Op::Prefix(40),
        Op::Append(72..120, 4),
        Op::Suffix(64),
        Op::Prefix(70),
        Op::Append(70..72, 5),
    ];
    for op in ops {
        match op {
            Op::Append(r, term) => log.append(recs(r, term)).await.unwrap(),
            Op::Suffix(from) => log.truncate_suffix(from).await.unwrap(),
            Op::Prefix(start) => log.truncate_prefix(start).await.unwrap(),
        }
        let before = state(&log).await;
        log = env.reopen(log, small()).await;
        assert_eq!(state(&log).await, before);
        log.flush().await.unwrap();
        log = env.crash(log, small()).await;
        assert_eq!(state(&log).await, before);
    }
}
