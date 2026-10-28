#![cfg(not(miri))]
#![forbid(unsafe_code)]
use shep_smp2::{
    Buffer, Config, Runtime,
    fs::{self, File},
};
use std::{
    future::Future,
    task::{Context, Poll, Waker},
};
fn poll<F: Future>(f: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(Waker::noop()))
}
#[test]
fn storage_file_contract_allocation_trimming_and_publication() {
    use shep_smp2::fs::OpenOptions;
    async fn append_and_trim(file: File) {
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
    let mut runtime = Runtime::new(Config { max_in_flight: 8 }).unwrap();
    runtime
        .block_on(async move {
            let nested = dir.path().join("data");
            fs::create_dir_all(&nested).await.unwrap();
            let tmp = nested.join("manifest.tmp");
            let file = File::open(
                &tmp,
                OpenOptions {
                    create_new: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            append_and_trim(file.clone()).await;
            fs::rename(&tmp, nested.join("manifest")).await.unwrap();
            fs::sync_dir(&nested).await.unwrap();
            assert_eq!(
                fs::read_dir(&nested).await.unwrap(),
                vec![nested.join("manifest")]
            );
            file.shutdown().await;
            assert_eq!(std::fs::read(nested.join("manifest")).unwrap(), b"record");
            fs::remove_file(nested.join("manifest")).await.unwrap();
            fs::sync_dir(&nested).await.unwrap();
        })
        .unwrap();
}

#[test]
fn shutdown_tracks_abandoned_futures_and_retained_results() {
    use shep_smp2::fs::OpenOptions;
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = Runtime::new(Config { max_in_flight: 8 }).unwrap();
    let io = runtime.io();
    runtime
        .block_on(async move {
            let file = File::open(
                dir.path().join("data"),
                OpenOptions {
                    create_new: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let mut abandoned = Box::pin(file.write_at(Buffer::from_slice(b"data", 1).unwrap(), 0));
            assert!(poll(abandoned.as_mut()).is_pending());
            drop(abandoned);
            let mut retained = Box::pin(file.read_at(Buffer::new(0, 1).unwrap(), 0));
            assert!(poll(retained.as_mut()).is_pending());
            let mut shutdown = Box::pin(file.shutdown());
            assert!(poll(shutdown.as_mut()).is_pending());
            let (result, buffer) = file
                .write_at(Buffer::from_slice(b"rejected", 1).unwrap(), 4)
                .await;
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(&*buffer, b"rejected");
            shutdown.await;
            // Completion consumption is not required for shutdown.
            assert!(retained.await.0.is_ok());
            assert_eq!(std::fs::read(dir.path().join("data")).unwrap(), b"data");
            assert_eq!(
                file.len().await.unwrap_err().kind(),
                std::io::ErrorKind::BrokenPipe
            );
            // Shutting down a file leaves the shared driver usable.
            let mut nop = io.push(shep_smp2::io::Operation::Nop).unwrap();
            nop.release(&io);
        })
        .unwrap();
}

#[test]
fn dropping_async_write_and_canceling_shutdown_does_not_reopen_gate() {
    let mut runtime = Runtime::new(Config { max_in_flight: 4 }).unwrap();
    runtime
        .block_on(async move {
            let file = File::from_std(tempfile::tempfile().unwrap()).await.unwrap();
            let mut write = Box::pin(file.write_at(Buffer::new(4096, 1).unwrap(), 0));
            assert!(poll(write.as_mut()).is_pending());
            drop(write);
            let mut shutdown = Box::pin(file.shutdown());
            assert!(poll(shutdown.as_mut()).is_pending());
            drop(shutdown);
            file.shutdown().await;
            assert_eq!(
                file.sync_data().await.unwrap_err().kind(),
                std::io::ErrorKind::BrokenPipe
            );
        })
        .unwrap();
}

#[test]
fn direct_io_discovers_alignment_validates_inputs_and_imports() {
    use shep_smp2::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("direct");
    let mut runtime = Runtime::new(Config { max_in_flight: 8 }).unwrap();
    runtime
        .block_on(async move {
            let file = File::open(
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
            assert_eq!(
                File::from_std(imported).await.unwrap().alignment(),
                alignment
            );
            file.shutdown().await;
        })
        .unwrap();
}

#[test]
fn invalid_open_exclusive_creation_and_saturation_have_no_unaccepted_effects() {
    use shep_smp2::fs::OpenOptions;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    std::fs::write(&path, b"keep").unwrap();
    let mut runtime = Runtime::new(Config { max_in_flight: 1 }).unwrap();
    let io = runtime.io();
    runtime
        .block_on(async move {
            assert_eq!(
                File::open(
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
                File::open(
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

            assert_eq!(std::fs::read(&path).unwrap(), b"keep");
            let mut handle = io.push(shep_smp2::io::Operation::Nop).unwrap();
            let other = dir.path().join("not-created");
            let mut open = Box::pin(File::open(
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
            handle.release(&io);
        })
        .unwrap();
}

#[test]
fn constructors_and_namespace_futures_resolve_context_only_when_polled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lazy");
    // Construct outside any runtime. Dropping does not create anything.
    drop(fs::create_dir_all(path.clone()));
    drop(File::open(
        path.clone(),
        fs::OpenOptions {
            create_new: true,
            ..Default::default()
        },
    ));
    drop(File::from_std(tempfile::tempfile().unwrap()));
    assert!(!path.exists());

    let mkdir = fs::create_dir_all(path.clone());
    let open = File::open(
        path.join("file"),
        fs::OpenOptions {
            create_new: true,
            ..Default::default()
        },
    );
    let import = File::from_std(tempfile::tempfile().unwrap());
    let mut runtime = Runtime::new(Default::default()).unwrap();
    runtime
        .block_on(async move {
            mkdir.await.unwrap();
            let file = open.await.unwrap();
            let imported = import.await.unwrap();
            assert_eq!(imported.alignment(), 1);
            // Unpolled shutdown does not close admission.
            drop(file.shutdown());
            assert_eq!(
                file.write_at(Buffer::new(1, 1).unwrap(), 0)
                    .await
                    .0
                    .unwrap(),
                1
            );
            file.shutdown().await;
            imported.shutdown().await;
        })
        .unwrap();
    assert!(path.join("file").exists());
}

#[test]
#[should_panic(expected = "must be called from within a shep_smp2 runtime")]
fn polling_a_constructor_requires_a_runtime_context() {
    let mut import = Box::pin(File::from_std(tempfile::tempfile().unwrap()));
    let _ = poll(import.as_mut());
}
