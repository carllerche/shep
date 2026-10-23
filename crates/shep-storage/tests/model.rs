//! A randomized comparison of the log against a vector, across appends,
//! truncations, reopens and crashes, on the real disk. Seeds are fixed so a
//! failure reproduces.
//!
//! Adapted from Redpanda's storage opfuzz (src/v/storage/opfuzz), which runs
//! random appends, truncations and restarts against a log.

mod util;

use std::time::Duration;

use bytes::Bytes;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use shep_storage::{Error, LogConfig, Record};
use util::*;

/// What the log should hold: records `start..start + records.len()`.
struct Model {
    start: u64,
    records: Vec<Record>,
    /// One past the last index a flush or truncation made durable.
    flushed: u64,
}

impl Model {
    fn next(&self) -> u64 {
        self.start + self.records.len() as u64
    }

    fn last_term(&self) -> u64 {
        self.records.last().map_or(0, |r| r.term)
    }

    fn range(&self, lo: u64, hi: u64) -> Vec<Record> {
        self.records[(lo - self.start) as usize..(hi - self.start) as usize].to_vec()
    }

    fn truncate(&mut self, from: u64) {
        self.records.truncate((from - self.start) as usize);
        self.flushed = self.flushed.min(from);
    }
}

fn config(rng: &mut StdRng) -> LogConfig {
    LogConfig {
        segment_size: rng.random_range(512..4096),
        prealloc_step: rng.random_range(100..2048),
        chunk_size: rng.random_range(64..1024),
        idle_write: Duration::from_millis(rng.random_range(1..50)),
        cache_bytes: rng.random_range(0..2048),
    }
}

async fn check(log: &shep_storage::Log, model: &Model, rng: &mut StdRng) {
    assert_eq!(log.start_index(), model.start);
    assert_eq!(log.next_index(), model.next());
    assert_eq!(read_all(log).await, model.records);
    for _ in 0..3 {
        if model.records.is_empty() {
            break;
        }
        let lo = rng.random_range(model.start..model.next());
        let hi = rng.random_range(lo..=model.next());
        let max = rng.random_range(0..2000);
        let got = log.read(lo..hi, max).await.unwrap();
        let want = model.range(lo, hi);
        assert!(!got.is_empty() || lo == hi);
        assert_eq!(got, want[..got.len()]);
        let bytes: usize = got.iter().map(|r| r.disk_len()).sum();
        if got.len() < want.len() {
            assert!(bytes >= max);
        }
        let i = rng.random_range(model.start..model.next());
        assert_eq!(log.term_at(i), Some(model.range(i, i + 1)[0].term));
    }
    assert_eq!(log.term_at(model.next()), None);
    if model.start > 0 {
        assert_eq!(log.term_at(model.start - 1), None);
    }
}

async fn run(seed: u64, ops: usize) {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut env = Env::with_pool(16 << 10).await;
    let mut cfg = config(&mut rng);
    let mut log = env.open(cfg.clone()).await;
    let mut model = Model {
        start: 0,
        records: Vec::new(),
        flushed: 0,
    };
    for _ in 0..ops {
        match rng.random_range(0..100) {
            0..45 => {
                let n = rng.random_range(1..8);
                let mut term = model.last_term().max(1);
                let batch: Vec<Record> = (0..n)
                    .map(|k| {
                        if rng.random_bool(0.05) {
                            term += 1;
                        }
                        let index = model.next() + k;
                        let len = if rng.random_bool(0.05) {
                            rng.random_range(1000..5000)
                        } else {
                            rng.random_range(0..200)
                        };
                        let payload: Vec<u8> = (0..len).map(|_| rng.random()).collect();
                        Record::new(index, term, rng.random(), Bytes::from(payload))
                    })
                    .collect();
                log.append(batch.clone()).await.unwrap();
                model.records.extend(batch);
            }
            45..55 => {
                log.flush().await.unwrap();
                model.flushed = model.next();
                assert_eq!(log.offsets().flushed, model.flushed);
            }
            55..65 => {
                let from = rng.random_range(model.start..=model.next());
                log.truncate_suffix(from).await.unwrap();
                model.truncate(from);
            }
            65..72 => {
                let new_start = rng.random_range(model.start..=model.next() + 3);
                log.truncate_prefix(new_start).await.unwrap();
                if new_start >= model.next() {
                    model.records.clear();
                    model.start = new_start;
                    model.flushed = new_start;
                } else if new_start > model.start {
                    model.records.drain(..(new_start - model.start) as usize);
                    model.start = new_start;
                    model.flushed = model.flushed.max(new_start);
                }
            }
            72..80 => {
                if model.start > 0 {
                    let below = rng.random_range(0..model.start);
                    assert!(matches!(
                        log.read(below.., 100).await,
                        Err(Error::BelowStart { .. })
                    ));
                }
                check(&log, &model, &mut rng).await;
            }
            80..88 => {
                cfg = config(&mut rng);
                log = env.reopen(log, cfg.clone()).await;
                model.flushed = model.next();
                check(&log, &model, &mut rng).await;
            }
            _ => {
                // A crash keeps what was flushed and maybe more: some prefix
                // of what was appended.
                cfg = config(&mut rng);
                log = env.crash(log, cfg.clone()).await;
                let next = log.next_index();
                assert!(
                    next >= model.flushed && next <= model.next(),
                    "recovered to {next}, flushed {}, appended {}",
                    model.flushed,
                    model.next()
                );
                model.truncate(next);
                model.flushed = next;
                check(&log, &model, &mut rng).await;
            }
        }
        // Positions are ordered.
        let o = log.offsets();
        assert!(o.start <= o.flushed, "{o:?}");
        assert!(o.flushed <= o.written && o.written <= o.next, "{o:?}");
        assert_eq!(o.next, model.next());
    }
    log = env.reopen(log, cfg).await;
    check(&log, &model, &mut rng).await;
}

#[tokio::test(flavor = "local")]
async fn model_seed_1() {
    run(1, 400).await;
}

#[tokio::test(flavor = "local")]
async fn model_seed_2() {
    run(2, 400).await;
}

#[tokio::test(flavor = "local")]
async fn model_seed_3() {
    run(3, 400).await;
}
