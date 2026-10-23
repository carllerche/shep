use shep_append::{AppendFile, BufferPool, Config};
use shep_smp_io::{Io, OpenOptions};
use std::time::Duration;

async fn exercise(direct: bool) {
    let io = Io::new(Default::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = io
        .open(
            dir.path().join("append"),
            OpenOptions {
                create_new: true,
                direct,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let pool = BufferPool::new(32768);
    let append = AppendFile::open(
        file.clone(),
        pool.clone(),
        Config {
            chunk_size: 8192,
            ..Default::default()
        },
        0,
    )
    .await
    .unwrap();
    let mut expected = Vec::new();
    for i in 0..60usize {
        let bytes = vec![i as u8; 333 + i * 11];
        expected.extend_from_slice(&bytes);
        let end = append.append(&bytes).await.unwrap();
        if i % 3 == 0 {
            append.sync_through(end).await.unwrap();
        }
    }
    append.finish().await.unwrap();
    assert_eq!(append.positions().durable, expected.len() as u64);
    assert_eq!(pool.available(), pool.capacity());
    assert!(pool.allocated() <= pool.capacity());
    assert_eq!(std::fs::read(file.path()).unwrap(), expected);
    io.shutdown().await.unwrap();
}
#[tokio::test(flavor = "local")]
async fn buffered_file() {
    exercise(false).await;
}
#[tokio::test(flavor = "local")]
async fn direct_file() {
    exercise(true).await;
}
#[tokio::test(flavor = "local")]
async fn pressure_dispatches_idle_heads() {
    let io = Io::new(Default::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pool = BufferPool::new(1024);
    let mut files = Vec::new();
    for i in 0..3 {
        let file = io
            .open(
                dir.path().join(i.to_string()),
                OpenOptions {
                    create_new: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        files.push(
            AppendFile::open(
                file,
                pool.clone(),
                Config {
                    chunk_size: 512,
                    idle: Duration::from_secs(3600),
                    ..Default::default()
                },
                0,
            )
            .await
            .unwrap(),
        );
    }
    files[0].append(b"one").await.unwrap();
    files[1].append(b"two").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), files[2].append(b"three"))
        .await
        .unwrap()
        .unwrap();
    for file in files {
        file.finish().await.unwrap();
    }
    assert_eq!(pool.available(), pool.capacity());
    io.shutdown().await.unwrap();
}
#[tokio::test(flavor = "local")]
async fn truncate_and_reopen_partial_direct_page() {
    let io = Io::new(Default::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = io
        .open(
            dir.path().join("data"),
            OpenOptions {
                create_new: true,
                direct: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let pool = BufferPool::new(16384);
    let append = AppendFile::open(file.clone(), pool.clone(), Config::default(), 0)
        .await
        .unwrap();
    append.append(&vec![1; 6000]).await.unwrap();
    append.truncate(123).await.unwrap();
    append.append(b"tail").await.unwrap();
    append.finish().await.unwrap();
    drop(append);
    let append = AppendFile::open(file.clone(), pool, Config::default(), 127)
        .await
        .unwrap();
    append.append(b"more").await.unwrap();
    append.finish().await.unwrap();
    let mut expected = vec![1; 123];
    expected.extend_from_slice(b"tailmore");
    assert_eq!(std::fs::read(file.path()).unwrap(), expected);
    io.shutdown().await.unwrap();
}

#[tokio::test(flavor = "local")]
async fn unallocated_files_extend_normally_and_finish_is_idempotent() {
    let io = Io::new(Default::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = io
        .open(
            dir.path().join("data"),
            OpenOptions {
                create_new: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let a = AppendFile::open(
        file.clone(),
        BufferPool::new(4096),
        Config {
            preallocate: 0,
            ..Default::default()
        },
        0,
    )
    .await
    .unwrap();
    a.append(b"no preallocation").await.unwrap();
    a.finish().await.unwrap();
    assert!(a.append(b"closed").await.is_err());
    a.finish().await.unwrap();
    assert_eq!(std::fs::read(file.path()).unwrap(), b"no preallocation");
    io.shutdown().await.unwrap();
}
