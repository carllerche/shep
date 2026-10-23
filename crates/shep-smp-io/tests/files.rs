use shep_smp_io::{Buffer, Config, FileIo, Io, OpenOptions};

async fn exercise(config: Config) {
    let io = Io::new(config).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let file = io
        .open(
            tmp.path().join("data"),
            OpenOptions {
                create_new: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..32u64 {
        let f = file.clone();
        tasks.push(tokio::task::spawn_local(async move {
            let b = Buffer::from_slice(&vec![i as u8; 4096], 4096).unwrap();
            assert_eq!(f.write_at(b, i * 4096).await.0.unwrap(), 4096);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    file.sync_data().await.unwrap();
    for i in 0..32u64 {
        let (n, b) = file
            .read_at(Buffer::new(4096, 4096).unwrap(), i * 4096)
            .await;
        assert_eq!(n.unwrap(), 4096);
        assert!(b.iter().all(|b| *b == i as u8));
    }
    io.shutdown().await.unwrap();
    assert!(file.sync_data().await.is_err());
}
#[tokio::test(flavor = "local")]
async fn kernel_io() {
    exercise(Config::default()).await;
}

#[tokio::test(flavor = "local")]
async fn initialization_fails_when_ring_cannot_be_created() {
    const CHILD: &str = "SHEP_IO_TEST_NO_DESCRIPTORS";
    if std::env::var_os(CHILD).is_some() {
        // Restrict only the child process, after its runtime has opened its descriptors.
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // Both calls receive a live rlimit; preserve the existing hard limit.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        limit.rlim_cur = 0;
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let error = match Io::new(Config::default()) {
            Err(error) => error,
            Ok(_) => panic!("driver started without being able to create an io_uring"),
        };
        assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "initialization_fails_when_ring_cannot_be_created",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "local")]
async fn abandoning_waiters_does_not_cancel_accepted_kernel_writes() {
    let io = Io::new(Config {
        max_in_flight: 32,
        ..Default::default()
    })
    .unwrap();
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
    let mut tasks = Vec::new();
    for i in 0..32u64 {
        let file = file.clone();
        tasks.push(tokio::task::spawn_local(async move {
            file.write_at(
                Buffer::from_slice(&vec![i as u8; 4096], 4096).unwrap(),
                i * 4096,
            )
            .await
        }));
    }
    while io.stats().submitted < 32 {
        tokio::task::yield_now().await;
    }
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
    io.drain().await.unwrap();
    assert_eq!(io.stats().completed, 32);
    let bytes = std::fs::read(file.path()).unwrap();
    for i in 0..32 {
        assert!(
            bytes[i * 4096..(i + 1) * 4096]
                .iter()
                .all(|v| *v == i as u8)
        );
    }
    io.shutdown().await.unwrap();
}
#[tokio::test(flavor = "local")]
async fn direct_io_rejects_unaligned_requests() {
    let io = Io::new(Default::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = io
        .open(
            dir.path().join("direct"),
            OpenOptions {
                direct: true,
                create_new: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let b = Buffer::new(file.alignment(), file.alignment()).unwrap();
    assert_eq!(
        file.write_at(b, 1).await.0.unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    io.shutdown().await.unwrap();
}

#[tokio::test(flavor = "local")]
async fn close_is_scoped_to_its_file_and_rejects_operations_on_clones() {
    let io = Io::new(Default::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = io
        .open(
            dir.path().join("closed"),
            OpenOptions {
                create_new: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let clone = file.clone();
    file.close().await.unwrap();
    assert_eq!(
        clone.sync_data().await.unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    let other = io
        .open(
            dir.path().join("other"),
            OpenOptions {
                create_new: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        other
            .write_at(Buffer::from_slice(b"works", 1).unwrap(), 0)
            .await
            .0
            .unwrap(),
        5
    );
    other.close().await.unwrap();
    io.shutdown().await.unwrap();
}

#[tokio::test(flavor = "local")]
async fn files_can_be_opened_read_only() {
    use std::os::unix::fs::PermissionsExt;
    let io = Io::new(Default::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source");
    std::fs::write(&path, b"source").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    let file = io
        .open(
            path,
            OpenOptions {
                write: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let (n, b) = file.read_at(Buffer::new(6, 1).unwrap(), 0).await;
    assert_eq!(n.unwrap(), 6);
    assert_eq!(&b[..], b"source");
    assert!(file.write_at(b, 0).await.0.is_err());
    file.close().await.unwrap();
    io.shutdown().await.unwrap();
}
