//! The per-core key-value store.

use std::path::{Path, PathBuf};
use std::time::Duration;

use shep_futures::future::join_all;
use shep_storage::{Disk, Error, KeySpace, KvConfig, KvStore};
use tempfile::TempDir;

struct Env {
    tmp: TempDir,
    disk: Disk,
}

impl Env {
    fn new() -> Env {
        Env {
            tmp: tempfile::tempdir().unwrap(),
            disk: Disk::new().unwrap(),
        }
    }

    fn dir(&self) -> PathBuf {
        self.tmp.path().join("kv")
    }

    async fn open(&self, config: KvConfig) -> KvStore {
        KvStore::open(&self.disk, self.dir(), config).await.unwrap()
    }

    fn log(&self) -> PathBuf {
        self.dir().join("kvstore.log")
    }

    /// Waits for the disk worker to finish what was queued, such as a
    /// snapshot a dropped store started.
    async fn drain(&self) {
        self.disk.run(|| Ok(())).await.unwrap();
    }
}

fn immediate() -> KvConfig {
    KvConfig {
        commit_interval: Duration::ZERO,
        ..KvConfig::default()
    }
}

fn b(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

fn len(p: &Path) -> u64 {
    std::fs::metadata(p).unwrap().len()
}

// Adapted from kvstore_test.cc: kvstore_empty.
#[tokio::test(flavor = "local")]
async fn empty() {
    let env = Env::new();
    let kv = env.open(KvConfig::default()).await;
    assert_eq!(kv.get(KeySpace::TESTING, b"k"), None);
    assert!(kv.scan(KeySpace::TESTING).is_empty());
    kv.flush().await.unwrap();
    assert_eq!(kv.stats().batches, 0);
    drop(kv);
    let kv = env.open(KvConfig::default()).await;
    assert_eq!(kv.get(KeySpace::TESTING, b"k"), None);
}

// Adapted from kvstore_test.cc: kvstore.
#[tokio::test(flavor = "local")]
async fn put_get_delete_and_reopen() {
    let env = Env::new();
    let kv = env.open(immediate()).await;
    for i in 0..50 {
        kv.put(
            KeySpace::TESTING,
            format!("k{i}").as_bytes(),
            b(&format!("v{i}")),
        )
        .await
        .unwrap();
    }
    kv.put(KeySpace::TESTING, b"k3", b("again")).await.unwrap();
    kv.delete(KeySpace::TESTING, b"k4").await.unwrap();
    kv.delete(KeySpace::TESTING, b"never").await.unwrap();
    let check = |kv: &KvStore| {
        assert_eq!(kv.get(KeySpace::TESTING, b"k0"), Some(b("v0")));
        assert_eq!(kv.get(KeySpace::TESTING, b"k3"), Some(b("again")));
        assert_eq!(kv.get(KeySpace::TESTING, b"k4"), None);
        assert_eq!(kv.get(KeySpace::TESTING, b"k49"), Some(b("v49")));
        assert_eq!(kv.scan(KeySpace::TESTING).len(), 49);
    };
    check(&kv);
    drop(kv);
    let kv = env.open(immediate()).await;
    check(&kv);
}

// Adapted from kvstore_test.cc: key_space.
#[tokio::test(flavor = "local")]
async fn key_spaces_are_separate() {
    let env = Env::new();
    let kv = env.open(immediate()).await;
    let spaces = [
        KeySpace::RAFT,
        KeySpace::STORAGE,
        KeySpace::SHARD_PLACEMENT,
        KeySpace(100),
    ];
    for (i, ks) in spaces.iter().enumerate() {
        kv.put(*ks, b"same", b(&i.to_string())).await.unwrap();
        kv.put(*ks, b"other", b("x")).await.unwrap();
    }
    kv.delete(KeySpace::STORAGE, b"other").await.unwrap();
    drop(kv);
    let kv = env.open(immediate()).await;
    for (i, ks) in spaces.iter().enumerate() {
        assert_eq!(kv.get(*ks, b"same"), Some(b(&i.to_string())));
    }
    assert_eq!(
        kv.scan(KeySpace::STORAGE),
        vec![(b("same"), b("1"))],
        "scan strips the key space"
    );
    assert_eq!(kv.scan(KeySpace::RAFT).len(), 2);
    assert!(kv.scan(KeySpace(101)).is_empty());
}

#[tokio::test(flavor = "local")]
async fn writes_are_visible_only_once_durable() {
    let env = Env::new();
    let kv = env
        .open(KvConfig {
            commit_interval: Duration::from_millis(30),
            ..KvConfig::default()
        })
        .await;
    let mut put = std::pin::pin!(kv.put(KeySpace::TESTING, b"k", b("v")));
    assert!(shep_futures::poll!(put.as_mut()).is_pending());
    assert_eq!(kv.get(KeySpace::TESTING, b"k"), None);
    put.await.unwrap();
    assert_eq!(kv.get(KeySpace::TESTING, b"k"), Some(b("v")));
}

#[tokio::test(flavor = "local")]
async fn group_commit_batches_a_window_into_one_sync() {
    let env = Env::new();
    let kv = env
        .open(KvConfig {
            commit_interval: Duration::from_millis(50),
            ..KvConfig::default()
        })
        .await;
    let before = env.disk.stats().fsyncs;
    let keys: Vec<String> = (0..100).map(|i| format!("k{i}")).collect();
    let results = join_all(
        keys.iter()
            .map(|k| kv.put(KeySpace::TESTING, k.as_bytes(), b("v"))),
    )
    .await;
    assert!(results.iter().all(|r| r.is_ok()));
    assert_eq!(kv.stats().batches, 1);
    assert_eq!(env.disk.stats().fsyncs - before, 1);
    assert_eq!(kv.scan(KeySpace::TESTING).len(), 100);
}

#[tokio::test(flavor = "local")]
async fn commits_while_one_is_in_flight_share_the_next() {
    let env = Env::new();
    let kv = env.open(immediate()).await;
    let mut first = std::pin::pin!(kv.put(KeySpace::TESTING, b"first", b("v")));
    assert!(shep_futures::poll!(first.as_mut()).is_pending());
    let rest =
        join_all((0..20).map(|i| kv.put(KeySpace::TESTING, format!("k{i}").as_bytes(), b("v"))));
    let (a, rest) = tokio::join!(first, rest);
    a.unwrap();
    assert!(rest.iter().all(|r| r.is_ok()));
    assert_eq!(kv.stats().batches, 2);
}

#[tokio::test(flavor = "local")]
async fn dropped_waiters_still_commit() {
    let env = Env::new();
    let kv = env.open(immediate()).await;
    {
        let mut put = std::pin::pin!(kv.put(KeySpace::TESTING, b"a", b("1")));
        // Starts the commit, then goes away.
        assert!(shep_futures::poll!(put.as_mut()).is_pending());
    }
    kv.put(KeySpace::TESTING, b"b", b("2")).await.unwrap();
    assert_eq!(kv.get(KeySpace::TESTING, b"a"), Some(b("1")));
    drop(kv);
    let kv = env.open(immediate()).await;
    assert_eq!(kv.get(KeySpace::TESTING, b"a"), Some(b("1")));
    assert_eq!(kv.get(KeySpace::TESTING, b"b"), Some(b("2")));
}

#[tokio::test(flavor = "local")]
async fn snapshot_rollover_resets_the_log() {
    let env = Env::new();
    let config = KvConfig {
        commit_interval: Duration::ZERO,
        max_log_bytes: 2048,
    };
    let kv = env.open(config.clone()).await;
    for i in 0..200 {
        kv.put(
            KeySpace::TESTING,
            format!("k{}", i % 37).as_bytes(),
            b(&format!("v{i}")),
        )
        .await
        .unwrap();
    }
    kv.delete(KeySpace::TESTING, b"k5").await.unwrap();
    // Wait out a snapshot the last commit may have started.
    kv.put(KeySpace::TESTING, b"last", b("x")).await.unwrap();
    let stats = kv.stats();
    assert!(stats.snapshots >= 3, "{stats:?}");
    assert!(stats.log_bytes <= 2048 + 200, "{stats:?}");
    assert!(len(&env.log()) <= 2048 + 200);
    assert!(env.dir().join("kvstore.snapshot").exists());
    let expect = kv.scan(KeySpace::TESTING);
    assert_eq!(expect.len(), 37);
    drop(kv);
    let kv = env.open(config).await;
    assert_eq!(kv.scan(KeySpace::TESTING), expect);
    assert_eq!(kv.get(KeySpace::TESTING, b"k6"), Some(b("v191")));
}

#[tokio::test(flavor = "local")]
async fn replay_skips_batches_the_snapshot_covers() {
    // A crash between writing the snapshot and emptying the log.
    let env = Env::new();
    let kv = env.open(immediate()).await;
    for i in 0..10 {
        kv.put(KeySpace::TESTING, b"k", b(&format!("v{i}")))
            .await
            .unwrap();
    }
    drop(kv);
    let old_log = std::fs::read(env.log()).unwrap();
    let small = KvConfig {
        commit_interval: Duration::ZERO,
        max_log_bytes: 1,
    };
    let kv = env.open(small.clone()).await;
    kv.put(KeySpace::TESTING, b"k", b("v10")).await.unwrap();
    kv.flush().await.unwrap();
    kv.put(KeySpace::TESTING, b"j", b("j")).await.unwrap();
    assert!(kv.stats().snapshots >= 1);
    drop(kv);
    env.drain().await;
    std::fs::write(env.log(), old_log).unwrap();
    let kv = env.open(small).await;
    assert_eq!(kv.get(KeySpace::TESTING, b"k"), Some(b("v10")));
    assert_eq!(kv.get(KeySpace::TESTING, b"j"), Some(b("j")));
    kv.put(KeySpace::TESTING, b"k", b("v11")).await.unwrap();
    drop(kv);
    let kv = env.open(immediate()).await;
    assert_eq!(kv.get(KeySpace::TESTING, b"k"), Some(b("v11")));
}

// Adapted from kvstore_test.cc: persist_pre_start_survives_crash_before_start
// (a torn final batch was never acknowledged, so it is dropped).
#[tokio::test(flavor = "local")]
async fn torn_last_batch_is_dropped() {
    for tear in [5u64, 40] {
        let env = Env::new();
        let kv = env.open(immediate()).await;
        kv.put(KeySpace::TESTING, b"a", b("1")).await.unwrap();
        let len_a = len(&env.log());
        kv.put(KeySpace::TESTING, b"b", b("2")).await.unwrap();
        drop(kv);
        env.drain().await;
        std::fs::OpenOptions::new()
            .write(true)
            .open(env.log())
            .unwrap()
            .set_len(len_a + tear)
            .unwrap();
        let kv = env.open(immediate()).await;
        assert_eq!(kv.get(KeySpace::TESTING, b"a"), Some(b("1")));
        assert_eq!(kv.get(KeySpace::TESTING, b"b"), None);
        assert_eq!(len(&env.log()), len_a, "the torn tail is cut off");
        kv.put(KeySpace::TESTING, b"c", b("3")).await.unwrap();
        drop(kv);
        let kv = env.open(immediate()).await;
        assert_eq!(kv.get(KeySpace::TESTING, b"c"), Some(b("3")));
    }
}

#[tokio::test(flavor = "local")]
async fn corrupt_snapshot_is_an_error() {
    let env = Env::new();
    let config = KvConfig {
        commit_interval: Duration::ZERO,
        max_log_bytes: 1,
    };
    let kv = env.open(config.clone()).await;
    kv.put(KeySpace::TESTING, b"a", b("1")).await.unwrap();
    kv.put(KeySpace::TESTING, b"b", b("2")).await.unwrap();
    drop(kv);
    env.drain().await;
    let snap = env.dir().join("kvstore.snapshot");
    let mut bytes = std::fs::read(&snap).unwrap();
    bytes[10] ^= 1;
    std::fs::write(&snap, bytes).unwrap();
    let err = KvStore::open(&env.disk, env.dir(), config).await;
    assert!(matches!(err, Err(Error::Corrupt { .. })));
}
