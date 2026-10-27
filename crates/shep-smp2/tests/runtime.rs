#![forbid(unsafe_code)]
use shep_smp2::{Buffer, Config, File, Runtime, sleep, spawn};
use std::{
    cell::Cell,
    future::{Future, pending, poll_fn},
    pin::Pin,
    rc::Rc,
    task::Poll,
    time::{Duration, Instant},
};

#[test]
fn nested_local_tasks_and_file_io_run_without_tokio_runtime() {
    assert!(tokio::runtime::Handle::try_current().is_err());
    let mut runtime = Runtime::new(Config {
        max_in_flight: 2,
        task_budget: 2,
    })
    .unwrap();
    let file = File::from_std(runtime.io(), tempfile::tempfile().unwrap());
    let local = Rc::new(42);
    let expected = local.clone();
    let result = runtime
        .block_on(async move {
            let value = spawn(async move { local }).await.unwrap();
            let (result, buffer) = file
                .write_at(Buffer::from_slice(b"native", 4096).unwrap(), 0)
                .await;
            assert_eq!(result.unwrap(), 6);
            file.sync_data().await.unwrap();
            let (result, buffer) = file.read_at(buffer, 0).await;
            assert_eq!(result.unwrap(), 6);
            assert_eq!(&*buffer, b"native");
            value
        })
        .unwrap();
    assert!(Rc::ptr_eq(&result, &expected));
    runtime.shutdown().unwrap();
}

#[test]
fn timers_make_progress_while_a_task_continually_self_wakes() {
    let mut runtime = Runtime::new(Config {
        task_budget: 1,
        ..Config::default()
    })
    .unwrap();
    let stop = Rc::new(Cell::new(false));
    let stopped = stop.clone();
    let count = Rc::new(Cell::new(0usize));
    let polled = count.clone();
    runtime.spawn(poll_fn(move |cx| {
        polled.set(polled.get() + 1);
        if stopped.get() {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }));
    let start = Instant::now();
    runtime
        .block_on(async move {
            sleep(Duration::from_millis(5)).await;
            stop.set(true);
        })
        .unwrap();
    assert!(start.elapsed() >= Duration::from_millis(5));
    assert!(count.get() > 1);
}

#[test]
fn timer_only_idle_wait_and_repeated_block_on() {
    let mut runtime = Runtime::new(Config::default()).unwrap();
    for _ in 0..5 {
        let start = Instant::now();
        runtime
            .block_on(async { sleep(Duration::from_millis(2)).await })
            .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(2));
    }
}

#[test]
fn foreign_channel_wakes_an_idle_runtime() {
    let mut runtime = Runtime::new(Config::default()).unwrap();
    for value in 0..50 {
        let (send, recv) = tokio::sync::oneshot::channel();
        let (ready, start) = std::sync::mpsc::sync_channel(0);
        let thread = std::thread::spawn(move || {
            start.recv().unwrap();
            send.send(value).unwrap();
        });
        let result = runtime
            .block_on(async move {
                let mut recv = recv;
                let mut first = true;
                poll_fn(|cx| {
                    let result = Pin::new(&mut recv).poll(cx);
                    if std::mem::take(&mut first) {
                        ready.send(()).unwrap();
                    }
                    result
                })
                .await
                .unwrap()
            })
            .unwrap();
        thread.join().unwrap();
        assert_eq!(result, value);
    }
}

#[test]
fn shutdown_cancels_tasks_and_drains_detached_kernel_work() {
    struct Dropped(Rc<Cell<bool>>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    let mut runtime = Runtime::new(Config::default()).unwrap();
    let io = runtime.io();
    let original = tempfile::NamedTempFile::new().unwrap();
    let file = File::from_std(io.clone(), original.reopen().unwrap());
    let dropped = Rc::new(Cell::new(false));
    let probe = Dropped(dropped.clone());
    let join = runtime.spawn(async move {
        let _probe = probe;
        pending::<()>().await;
    });
    runtime.spawn(async move {
        file.write_at(Buffer::from_slice(b"drained", 1).unwrap(), 0)
            .await
            .0
            .unwrap();
    });
    runtime.block_on(async {}).unwrap();
    runtime.shutdown().unwrap();
    assert!(dropped.get());
    assert!(join.is_finished());
    assert_eq!(io.in_flight(), 0);
    assert_eq!(std::fs::read(original.path()).unwrap(), b"drained");
    assert!(runtime.block_on(async {}).is_err());
}

#[test]
fn io_and_timers_progress_under_sustained_task_load() {
    let mut runtime = Runtime::new(Config {
        task_budget: 2,
        max_in_flight: 64,
    })
    .unwrap();
    let file = File::from_std(runtime.io(), tempfile::tempfile().unwrap());
    runtime
        .block_on(async move {
            let mut tasks = Vec::new();
            for index in 0..64 {
                let file = file.clone();
                tasks.push(spawn(async move {
                    let (result, _) = file
                        .write_at(
                            Buffer::from_slice(&[index as u8; 64], 64).unwrap(),
                            index * 64,
                        )
                        .await;
                    assert_eq!(result.unwrap(), 64);
                }));
            }
            sleep(Duration::from_millis(2)).await;
            for task in tasks {
                task.await.unwrap();
            }
            file.sync_all().await.unwrap();
            let (result, buffer) = file.read_at(Buffer::new(4096, 4096).unwrap(), 0).await;
            assert_eq!(result.unwrap(), 4096);
            for index in 0..64 {
                assert_eq!(&buffer[index * 64..(index + 1) * 64], &[index as u8; 64]);
            }
        })
        .unwrap();
}

#[test]
fn task_panics_are_contained_and_runtime_can_be_reused() {
    let mut runtime = Runtime::new(Config::default()).unwrap();
    assert!(runtime.block_on(async { panic!("root panic") }).is_err());
    assert_eq!(runtime.block_on(async { 42 }).unwrap(), 42);
}

#[test]
fn invalid_operation_does_not_consume_admission() {
    let mut runtime = Runtime::new(Config {
        task_budget: 8,
        max_in_flight: 1,
    })
    .unwrap();
    let file = File::from_std(runtime.io(), tempfile::tempfile().unwrap());
    runtime
        .block_on(async move {
            let bad = file.clone();
            let bad = spawn(async move {
                assert!(
                    bad.read_at(Buffer::new(1, 1).unwrap(), u64::MAX)
                        .await
                        .0
                        .is_err()
                );
            });
            let good = spawn(async move {
                assert_eq!(
                    file.write_at(Buffer::new(1, 1).unwrap(), 0)
                        .await
                        .0
                        .unwrap(),
                    1
                );
            });
            bad.await.unwrap();
            good.await.unwrap();
        })
        .unwrap();
}

#[test]
fn saturated_runtime_returns_owned_operation_without_an_admission_waiter() {
    let mut runtime = Runtime::new(Config {
        max_in_flight: 1,
        task_budget: 8,
    })
    .unwrap();
    let file = File::from_std(runtime.io(), tempfile::tempfile().unwrap());
    runtime
        .block_on(async move {
            let mut first = file
                .submit_write_at(Buffer::from_slice(b"first", 1).unwrap(), 0)
                .unwrap();
            let (result, buffer) = file
                .write_at(Buffer::from_slice(b"second", 1).unwrap(), 8)
                .await;
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
            assert_eq!(&*buffer, b"second");
            std::future::poll_fn(|cx| first.poll_complete(file.io(), cx))
                .await
                .result
                .unwrap();
            assert_eq!(file.write_at(buffer, 8).await.0.unwrap(), 6);
        })
        .unwrap();
    runtime.shutdown().unwrap();
}

#[test]
fn filesystem_metadata_worker_wakes_native_runtime_and_shutdown_drains_files() {
    use shep_smp_fs2::{Fs, OpenOptions};
    let mut runtime = Runtime::new(Default::default()).unwrap();
    let fs = Fs::new(runtime.io(), Default::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    runtime
        .block_on(async move {
            let work = async {
                let path = dir.path().join("native-fs");
                fs.create_dir_all(&path).await.unwrap();
                let file = fs
                    .open(
                        path.join("data"),
                        OpenOptions {
                            create_new: true,
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                file.allocate(0, 4096).await.unwrap();
                assert_eq!(file.len().await.unwrap(), 0);
                let (result, _) = file
                    .write_at(Buffer::from_slice(b"native", 1).unwrap(), 0)
                    .await;
                assert_eq!(result.unwrap(), 6);
                assert_eq!(file.len().await.unwrap(), 6);
                file.sync_data().await.unwrap();
                fs.sync_dir(&path).await.unwrap();
                assert_eq!(fs.read_dir(&path).await.unwrap().len(), 1);
                file.close().await.unwrap();
                fs.shutdown().await;
            };
            shep_smp2::timeout(Duration::from_secs(5), work)
                .await
                .expect("metadata worker must wake parked runtime");
        })
        .unwrap();
    runtime.shutdown().unwrap();
}
