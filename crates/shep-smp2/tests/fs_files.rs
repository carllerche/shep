#![cfg(not(miri))]
#![forbid(unsafe_code)]
use shep_smp2::fs::File;
use shep_smp2::io::{Buffer, Driver};
use std::{
    future::Future,
    io::Read,
    pin::pin,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

fn poll<F: Future>(f: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn futures_are_lazy_and_saturation_returns_owned_buffer_immediately() {
    let mut reactor = Driver::new(1).unwrap();
    let io = reactor.io().clone();
    let temp = tempfile::tempfile().unwrap();
    let file = File::from_std(io.clone(), temp).unwrap();
    let first = file.write_at(Buffer::from_slice(b"first", 1).unwrap(), 0);
    let mut first = pin!(first);
    assert_eq!(io.in_flight(), 0);
    assert!(poll(first.as_mut()).is_pending());
    assert_eq!(io.in_flight(), 1);
    {
        let mut second = pin!(file.write_at(Buffer::from_slice(b"second", 1).unwrap(), 8));
        let Poll::Ready((result, buffer)) = poll(second.as_mut()) else {
            panic!("fail fast at capacity");
        };
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(&*buffer, b"second");
        assert_eq!(io.in_flight(), 1);
    }
    reactor.drain().unwrap();
    let Poll::Ready((result, buffer)) = poll(first.as_mut()) else {
        panic!("completed write");
    };
    assert_eq!(result.unwrap(), 5);
    assert_eq!(&*buffer, b"first");
    assert_eq!(io.in_flight(), 0);
    let mut late = pin!(file.read_at(Buffer::new(4, 1).unwrap(), 0));
    let Poll::Ready((result, buffer)) = poll(late.as_mut()) else {
        panic!("closed admission");
    };
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(buffer.len(), 4);
}

#[test]
fn dropping_a_handle_before_submission_keeps_accepted_work_until_drain() {
    let mut reactor = Driver::new(2).unwrap();
    let io = reactor.io().clone();
    let mut original = tempfile::tempfile().unwrap();
    let file = File::from_std(io.clone(), original.try_clone().unwrap()).unwrap();
    {
        let mut write = pin!(file.write_at(Buffer::from_slice(b"detached", 1).unwrap(), 0));
        assert!(poll(write.as_mut()).is_pending());
    }
    assert_eq!(io.in_flight(), 1);
    reactor.drain().unwrap();
    drop(reactor);
    let mut bytes = Vec::new();
    original.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"detached");
    assert_eq!(io.in_flight(), 0);
}

#[test]
fn submitted_waiter_can_be_dropped_and_remaining_operations_complete() {
    let mut progress = shep_smp2::io::Progress::default();
    let mut reactor = Driver::new(2).unwrap();
    let io = reactor.io().clone();
    let file = File::from_std(io.clone(), tempfile::tempfile().unwrap()).unwrap();
    {
        let mut write = pin!(file.write_at(Buffer::from_slice(b"submitted", 1).unwrap(), 0));
        assert!(poll(write.as_mut()).is_pending());
        reactor.submit().unwrap();
        reactor.reap_into(&mut progress);
        progress.wake();
    }
    let mut sync = pin!(file.sync_all());
    assert!(poll(sync.as_mut()).is_pending());
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        reactor.submit().unwrap();
        reactor.reap_into(&mut progress);
        progress.wake();
        if let Poll::Ready(result) = poll(sync.as_mut()) {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < end);
        reactor.submit_and_wait(Some(end)).unwrap();
    }
    reactor.drain().unwrap();
}

#[test]
fn a_panicking_completion_waker_does_not_strand_other_completions() {
    struct Bad;
    impl std::task::Wake for Bad {
        fn wake(self: std::sync::Arc<Self>) {
            panic!("bad wake");
        }
    }
    let mut reactor = Driver::new(2).unwrap();
    let io = reactor.io().clone();
    let file = File::from_std(io.clone(), tempfile::tempfile().unwrap()).unwrap();
    let mut first = pin!(file.write_at(Buffer::new(1, 1).unwrap(), 0));
    let mut second = pin!(file.write_at(Buffer::new(1, 1).unwrap(), 1));
    let bad = Waker::from(std::sync::Arc::new(Bad));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&bad))
            .is_pending()
    );
    assert!(poll(second.as_mut()).is_pending());
    reactor.drain().unwrap();
    assert!(poll(first.as_mut()).is_ready());
    assert!(poll(second.as_mut()).is_ready());
    assert_eq!(io.in_flight(), 0);
}

#[test]
fn completed_result_survives_dropping_the_reactor_owner() {
    let mut reactor = Driver::new(1).unwrap();
    let io = reactor.io().clone();
    let file = File::from_std(io.clone(), tempfile::tempfile().unwrap()).unwrap();
    let mut write = pin!(file.write_at(Buffer::from_slice(b"survives", 1).unwrap(), 0));
    assert!(poll(write.as_mut()).is_pending());
    reactor.drain().unwrap();
    drop(reactor);
    let Poll::Ready((result, buffer)) = poll(write.as_mut()) else {
        panic!("retired result remains available");
    };
    assert_eq!(result.unwrap(), 8);
    assert_eq!(&*buffer, b"survives");
}

// Drive both kernel completions and foreign helper-thread wakes without a
// runtime dependency. The deadline makes lost accounting visible as a failure.
fn run<F: Future>(driver: &mut Driver, future: F) -> F::Output {
    let mut future = pin!(future);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut progress = shep_smp2::io::Progress::default();
    loop {
        if let Poll::Ready(result) = poll(future.as_mut()) {
            return result;
        }
        driver.submit().unwrap();
        driver.reap_into(&mut progress);
        progress.wake();
        assert!(Instant::now() < deadline, "filesystem operation stalled");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn storage_file_contract_allocation_trimming_and_publication() {
    use shep_smp2::fs::{FileIo, Fs, OpenOptions};
    async fn append_and_trim<F: FileIo>(file: F) {
        assert!(file.is_empty().await.unwrap());
        file.allocate(0, 16384).await.unwrap();
        assert_eq!(
            file.len().await.unwrap(),
            0,
            "allocation keeps logical size"
        );
        file.set_len(16384).await.unwrap();
        let (result, _) = file
            .write_at(Buffer::from_slice(b"record", file.alignment()).unwrap(), 0)
            .await;
        assert_eq!(result.unwrap(), 6);
        file.set_len(6).await.unwrap();
        file.sync_data().await.unwrap();
        file.sync_all().await.unwrap();
        let (result, buffer) = file
            .read_at(Buffer::new(32, file.alignment()).unwrap(), 0)
            .await;
        assert_eq!(result.unwrap(), 6);
        assert_eq!(&buffer[..6], b"record");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut driver = Driver::new(8).unwrap();
    let fs = Fs::new(driver.io().clone(), Default::default()).unwrap();
    run(&mut driver, async {
        let nested = dir.path().join("data");
        fs.create_dir_all(&nested).await.unwrap();
        let tmp = nested.join("manifest.tmp");
        let file = fs
            .open(
                &tmp,
                OpenOptions {
                    create_new: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(file.path(), tmp);
        append_and_trim(file.clone()).await;
        fs.rename(&tmp, nested.join("manifest")).await.unwrap();
        fs.sync_dir(&nested).await.unwrap();
        assert_eq!(
            fs.read_dir(&nested).await.unwrap(),
            vec![nested.join("manifest")]
        );
        file.close().await.unwrap();
        assert_eq!(std::fs::read(nested.join("manifest")).unwrap(), b"record");
        fs.remove_file(nested.join("manifest")).await.unwrap();
        fs.sync_dir(&nested).await.unwrap();
        fs.shutdown().await;
        assert_eq!(
            fs.read_dir(&nested).await.unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    });
}

#[test]
fn close_and_domain_drain_track_released_eager_handles_and_retained_results() {
    use shep_smp2::fs::{Fs, OpenOptions};
    let dir = tempfile::tempdir().unwrap();
    let mut driver = Driver::new(8).unwrap();
    let fs = Fs::new(driver.io().clone(), Default::default()).unwrap();
    let file = run(
        &mut driver,
        fs.open(
            dir.path().join("data"),
            OpenOptions {
                create_new: true,
                ..Default::default()
            },
        ),
    )
    .unwrap();
    let mut abandoned = file
        .submit_write_at(Buffer::from_slice(b"data", 1).unwrap(), 0)
        .unwrap();
    abandoned.release(file.io());
    let mut retained = file.submit_read_at(Buffer::new(0, 1).unwrap(), 0).unwrap();
    let mut close = Box::pin(file.clone().close());
    let mut drain = Box::pin(fs.drain());
    assert!(poll(close.as_mut()).is_pending());
    assert!(poll(drain.as_mut()).is_pending());
    let rejected = file
        .submit_write_at(Buffer::from_slice(b"rejected", 1).unwrap(), 4)
        .unwrap_err();
    assert_eq!(rejected.error.kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(&*rejected.operation.into_buffer().unwrap(), b"rejected");
    run(&mut driver, close).unwrap();
    assert!(poll(drain.as_mut()).is_ready());
    // Completion consumption is not required for close/drain.
    assert!(retained.try_complete(file.io()).unwrap().result.is_ok());
    assert_eq!(std::fs::read(dir.path().join("data")).unwrap(), b"data");
    assert_eq!(
        run(&mut driver, file.len()).unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    // A filesystem shutdown must not close a shared networking/other-domain driver.
    run(&mut driver, fs.shutdown());
    let mut nop = driver.io().push(shep_smp2::io::Operation::Nop).unwrap();
    nop.release(driver.io());
}

#[test]
fn dropping_async_write_and_canceling_close_does_not_reopen_gate() {
    let mut driver = Driver::new(4).unwrap();
    let file = File::from_std(driver.io().clone(), tempfile::tempfile().unwrap()).unwrap();
    let mut write = Box::pin(file.write_at(Buffer::new(4096, 1).unwrap(), 0));
    assert!(poll(write.as_mut()).is_pending());
    drop(write);
    let mut close = Box::pin(file.clone().close());
    assert!(poll(close.as_mut()).is_pending());
    drop(close);
    run(&mut driver, file.clone().close()).unwrap();
    assert_eq!(
        run(&mut driver, file.sync_data()).unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
}

#[test]
fn direct_io_discovers_alignment_validates_inputs_and_imports() {
    use shep_smp2::fs::{Fs, OpenOptions};
    use std::os::unix::fs::OpenOptionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("direct");
    let mut driver = Driver::new(8).unwrap();
    let fs = Fs::new(driver.io().clone(), Default::default()).unwrap();
    run(&mut driver, async {
        let file = fs
            .open(
                &path,
                OpenOptions {
                    direct: true,
                    create_new: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let alignment = file.alignment();
        assert!(alignment.is_power_of_two());
        let size = 4096usize.max(alignment);
        let (result, buffer) = file
            .write_at(Buffer::new(size, alignment).unwrap(), 0)
            .await;
        assert_eq!(result.unwrap(), size);
        let (result, buffer) = file.read_at(buffer, 0).await;
        assert_eq!(result.unwrap(), size);
        if alignment > 1 {
            let ptr = buffer.as_ptr();
            let (result, buffer) = file.write_at(buffer, 1).await;
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(ptr, buffer.as_ptr());
        }
        file.set_len(7).await.unwrap();
        file.sync_data().await.unwrap();
        assert_eq!(file.len().await.unwrap(), 7);
        let imported = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&path)
            .unwrap();
        assert_eq!(fs.from_std(imported).await.unwrap().alignment(), alignment);
        file.close().await.unwrap();
        fs.shutdown().await;
    });
}

#[test]
fn invalid_open_exclusive_creation_and_saturation_have_no_unaccepted_effects() {
    use shep_smp2::fs::{Fs, OpenOptions};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    std::fs::write(&path, b"keep").unwrap();
    let mut driver = Driver::new(1).unwrap();
    let fs = Fs::new(driver.io().clone(), Default::default()).unwrap();
    run(&mut driver, async {
        assert_eq!(
            fs.open(
                &path,
                OpenOptions {
                    read: false,
                    write: false,
                    ..Default::default()
                }
            )
            .await
            .err()
            .unwrap()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            fs.open(
                &path,
                OpenOptions {
                    create_new: true,
                    truncate: true,
                    ..Default::default()
                }
            )
            .await
            .err()
            .unwrap()
            .kind(),
            std::io::ErrorKind::AlreadyExists
        );
    });
    assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    let mut handle = driver.io().push(shep_smp2::io::Operation::Nop).unwrap();
    let other = dir.path().join("not-created");
    let mut open = Box::pin(fs.open(
        &other,
        OpenOptions {
            create_new: true,
            ..Default::default()
        },
    ));
    let Poll::Ready(Err(error)) = poll(open.as_mut()) else {
        panic!("full admission must reject")
    };
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(!other.exists());
    handle.release(driver.io());
    run(&mut driver, fs.shutdown());
}
