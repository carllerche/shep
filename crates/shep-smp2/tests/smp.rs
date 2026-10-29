use shep_futures::future::{block_on, join_all};
use shep_smp2::{CoreId, Smp, SmpConfig, SmpError, current_core};
use std::{
    cell::RefCell,
    future::{pending, poll_fn},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    task::{Context, Poll, Waker},
};

fn smp(cores: usize) -> Smp {
    Smp::start(SmpConfig {
        pin: false,
        ..SmpConfig::with_cores(cores)
    })
    .unwrap()
}
async fn yield_once() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

#[test]
fn workers_have_native_runtimes_and_local_futures() {
    let smp = smp(3);
    assert_eq!(current_core(), None);
    for core in smp.core_ids() {
        let result = block_on(smp.submit_to(core, || async {
            let state = Rc::new(RefCell::new(1));
            yield_once().await;
            *state.borrow_mut() += 1;
            shep_smp2::sleep(std::time::Duration::from_millis(1)).await;
            let child = shep_smp2::spawn(async { 40 });
            let child_result = child.await.unwrap();
            (
                current_core(),
                *state.borrow() + child_result,
                std::thread::current().name().unwrap().to_owned(),
            )
        }))
        .unwrap();
        assert_eq!(result, (Some(core), 42, format!("shep-core-{}", core.0)));
    }
    smp.shutdown().unwrap();
}

#[test]
fn zero_cores_and_invalid_core() {
    let smp = smp(0);
    assert_eq!(smp.cores(), 1);
    assert_eq!(
        block_on(smp.submit_to(CoreId(1), || async {})),
        Err(SmpError::NoSuchCore(CoreId(1)))
    );
    smp.shutdown().unwrap();
}

#[test]
fn closures_and_future_panics_are_isolated() {
    let smp = smp(1);
    assert_eq!(
        block_on(smp.submit_to(CoreId(0), || -> std::future::Ready<()> {
            panic!("constructor")
        })),
        Err(SmpError::Panicked)
    );
    assert_eq!(
        block_on(smp.submit_to(CoreId(0), || async {
            yield_once().await;
            panic!("poll")
        })),
        Err::<(), _>(SmpError::Panicked)
    );
    assert_eq!(block_on(smp.submit_to(CoreId(0), || async { 42 })), Ok(42));
    smp.shutdown().unwrap();
}

#[test]
fn submissions_are_lazy_ordered_and_detached_when_abandoned() {
    let smp = smp(1);
    let seen = Arc::new(Mutex::new(vec![]));
    let unpolled = smp.submit_to(CoreId(0), || async { panic!("unpolled") });
    drop(unpolled);
    let mut calls = vec![];
    for i in 0..300 {
        let seen = seen.clone();
        let mut call = Box::pin(smp.submit_to(CoreId(0), move || async move {
            seen.lock().unwrap().push(i);
        }));
        match call.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(result) => result.unwrap(),
            Poll::Pending if i % 2 == 0 => calls.push(call),
            Poll::Pending => {}
        }
    }
    block_on(join_all(calls))
        .into_iter()
        .for_each(|r| r.unwrap());
    block_on(smp.submit_to(CoreId(0), || async {})).unwrap();
    assert_eq!(*seen.lock().unwrap(), (0..300).collect::<Vec<_>>());
    smp.shutdown().unwrap();
}

#[test]
fn every_core_can_submit_to_every_core_including_itself() {
    let smp = smp(3);
    let calls = smp.core_ids().map(|from| {
        let nested = smp.clone();
        smp.submit_to(from, move || async move {
            let calls = (0..150).map(|i| {
                let to = CoreId(i % nested.cores());
                nested.submit_to(to, move || async move { (current_core(), i) })
            });
            for (i, result) in join_all(calls).await.into_iter().enumerate() {
                assert_eq!(result, Ok((Some(CoreId(i % nested.cores())), i)));
            }
        })
    });
    block_on(join_all(calls))
        .into_iter()
        .for_each(|r| r.unwrap());
    smp.shutdown().unwrap();
}

#[test]
fn shutdown_cancels_work_and_drops_tasks_on_their_worker() {
    struct Probe(mpsc::Sender<Option<CoreId>>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.send(current_core()).unwrap();
        }
    }
    let smp = smp(2);
    let (started, ready) = mpsc::channel();
    let (dropped, drops) = mpsc::channel();
    let mut call = Box::pin(smp.submit_to(CoreId(1), move || async move {
        let _probe = Probe(dropped);
        started.send(()).unwrap();
        pending::<()>().await
    }));
    assert!(
        call.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    ready.recv().unwrap();
    smp.shutdown().unwrap();
    assert_eq!(drops.recv().unwrap(), Some(CoreId(1)));
    assert_eq!(block_on(call), Err(SmpError::Shutdown));
    assert_eq!(
        block_on(smp.submit_to(CoreId(0), || async {})),
        Err(SmpError::Shutdown)
    );
    smp.shutdown().unwrap();
}

#[test]
fn concurrent_shutdown_waits_for_task_destruction() {
    let smp = smp(2);
    let (started, ready) = mpsc::channel();
    let (release, released) = mpsc::channel();
    struct Probe(mpsc::Sender<()>, mpsc::Receiver<()>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.send(()).unwrap();
            self.1.recv().unwrap();
        }
    }
    let (dropping, is_dropping) = mpsc::channel();
    let mut call = Box::pin(smp.submit_to(CoreId(0), move || async move {
        let _probe = Probe(dropping, released);
        started.send(()).unwrap();
        pending::<()>().await;
    }));
    let _ = call.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    ready.recv().unwrap();
    let a = smp.clone();
    let first = std::thread::spawn(move || a.shutdown());
    is_dropping.recv().unwrap();
    let b = smp.clone();
    let second = std::thread::spawn(move || b.shutdown());
    release.send(()).unwrap();
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    assert_eq!(block_on(call), Err(SmpError::Shutdown));
}

#[test]
fn worker_cannot_join_itself() {
    let smp = smp(1);
    let inner = smp.clone();
    assert_eq!(
        block_on(smp.submit_to(CoreId(0), move || async move {
            inner.shutdown().unwrap();
        })),
        Err(SmpError::Panicked)
    );
    smp.shutdown().unwrap();
}

#[test]
fn native_runtime_initialization_failure_is_reported() {
    let config = SmpConfig {
        pin: false,
        runtime: shep_smp2::Config {
            submission_queue_capacity: 0,
        },
        ..SmpConfig::with_cores(3)
    };
    assert!(matches!(Smp::start(config), Err(SmpError::Start(_))));
}

#[test]
fn last_handle_drop_stops_workers() {
    let smp = smp(1);
    let (dropped, drops) = mpsc::channel();
    struct Probe(mpsc::Sender<()>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.send(()).unwrap();
        }
    }
    block_on(smp.submit_to(CoreId(0), move || async move {
        shep_smp2::spawn(async move {
            let _probe = Probe(dropped);
            pending::<()>().await;
        });
        yield_once().await;
    }))
    .unwrap();
    drop(smp);
    drops.recv().unwrap();
}

#[test]
fn shutdown_racing_with_producers_resolves_every_reply() {
    let smp = smp(2);
    let barrier = Arc::new(std::sync::Barrier::new(5));
    let mut producers = vec![];
    for producer in 0..4 {
        let smp = smp.clone();
        let barrier = barrier.clone();
        producers.push(std::thread::spawn(move || {
            barrier.wait();
            let calls = (0..200).map(|i| {
                smp.submit_to(CoreId(producer % 2), move || async move {
                    yield_once().await;
                    i
                })
            });
            for result in block_on(join_all(calls)) {
                assert!(result.is_ok() || result == Err(SmpError::Shutdown));
            }
        }));
    }
    barrier.wait();
    smp.shutdown().unwrap();
    for producer in producers {
        producer.join().unwrap();
    }
}

#[test]
fn foreign_replies_can_be_awaited_by_a_native_runtime() {
    let smp = smp(1);
    let mut runtime = shep_smp2::Runtime::new(Default::default()).unwrap();
    for i in 0..100 {
        let smp = smp.clone();
        assert_eq!(
            runtime
                .block_on(async move { smp.submit_to(CoreId(0), move || async move { i }).await })
                .unwrap(),
            Ok(i)
        );
    }
    runtime.shutdown().unwrap();
    smp.shutdown().unwrap();
}

#[test]
fn busy_inbox_allows_existing_tasks_to_run() {
    let smp = smp(1);
    let (running, ready) = mpsc::channel();
    let local_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = local_count.clone();
    block_on(smp.submit_to(CoreId(0), move || async move {
        shep_smp2::spawn(async move {
            loop {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                yield_once().await;
            }
        });
        running.send(()).unwrap();
    }))
    .unwrap();
    ready.recv().unwrap();
    let initial = local_count.load(std::sync::atomic::Ordering::Relaxed);
    block_on(join_all(
        (0..130).map(|_| smp.submit_to(CoreId(0), || async {})),
    ))
    .into_iter()
    .for_each(|r| r.unwrap());
    assert!(local_count.load(std::sync::atomic::Ordering::Relaxed) > initial);
    smp.shutdown().unwrap();
}

#[test]
fn cross_shard_overflow_and_reverse_replies_preserve_every_value() {
    let smp = smp(3);
    block_on(join_all(smp.core_ids().map(|from| {
        let inner = smp.clone();
        smp.submit_to(from, move || async move {
            let destination = CoreId((from.0 + 1) % inner.cores());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let calls = (0..257).map(|i| {
                let seen = seen.clone();
                inner.submit_to(destination, move || async move {
                    seen.lock().unwrap().push(i);
                    yield_once().await;
                    i
                })
            });
            let results = join_all(calls).await;
            assert_eq!(results, (0..257).map(Ok).collect::<Vec<_>>());
            assert_eq!(*seen.lock().unwrap(), (0..257).collect::<Vec<_>>());
        })
    })))
    .into_iter()
    .for_each(|r| r.unwrap());
    smp.shutdown().unwrap();
}

#[test]
fn same_numbered_shards_in_different_groups_use_external_ingress() {
    let a = smp(1);
    let b = smp(1);
    let peer = b.clone();
    let home = a.clone();
    assert_eq!(
        block_on(a.submit_to(CoreId(0), move || async move {
            peer.submit_to(CoreId(0), move || async move {
                home.submit_to(CoreId(0), || async { 42 }).await
            })
            .await
        })),
        Ok(Ok(Ok(42)))
    );
    a.shutdown().unwrap();
    b.shutdown().unwrap();
}

#[test]
fn idle_cross_shard_round_trips_flush_partial_request_and_reply_batches() {
    let smp = smp(2);
    let inner = smp.clone();
    block_on(smp.submit_to(CoreId(0), move || async move {
        for i in 0..40 {
            shep_smp2::sleep(std::time::Duration::from_millis(1)).await;
            assert_eq!(
                inner.submit_to(CoreId(1), move || async move { i }).await,
                Ok(i)
            );
        }
    }))
    .unwrap();
    smp.shutdown().unwrap();
}

#[test]
fn shutdown_resolves_replies_created_on_a_shard_and_moved_to_an_external_waiter() {
    let smp = smp(2);
    let inner = smp.clone();
    let replies = block_on(smp.submit_to(CoreId(0), move || async move {
        (0..257)
            .map(|_| {
                let mut reply =
                    Box::pin(inner.submit_to(CoreId(1), || async { pending::<()>().await }));
                assert!(
                    reply
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                reply
            })
            .collect::<Vec<_>>()
    }))
    .unwrap();
    smp.shutdown().unwrap();
    for reply in block_on(join_all(replies)) {
        assert_eq!(reply, Err(SmpError::Shutdown));
    }
}

#[test]
fn idle_overflow_retries_without_any_completions_or_capacity_notifications() {
    let smp = smp(2);
    // Deliberately stop the consumer so the source must overflow its ring.
    let (blocked, is_blocked) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let mut blocker = Box::pin(smp.submit_to(CoreId(1), move || async move {
        blocked.send(()).unwrap();
        released.recv().unwrap();
    }));
    assert!(
        blocker
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    is_blocked.recv().unwrap();

    let (started, all_started) = mpsc::channel();
    let inner = smp.clone();
    let replies = block_on(smp.submit_to(CoreId(0), move || async move {
        (0..257)
            .map(|i| {
                let started = started.clone();
                let mut reply = Box::pin(inner.submit_to(CoreId(1), move || async move {
                    if i == 256 {
                        started.send(()).unwrap();
                    }
                    // None of these requests can wake the source with a completion.
                    pending::<()>().await;
                }));
                assert!(
                    reply
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                reply
            })
            .collect::<Vec<_>>()
    }))
    .unwrap();
    // Let the producer become idle with no local tasks or application timers.
    std::thread::sleep(std::time::Duration::from_millis(5));
    release.send(()).unwrap();
    all_started.recv().unwrap();
    block_on(blocker).unwrap();
    smp.shutdown().unwrap();
    for result in block_on(join_all(replies)) {
        assert_eq!(result, Err(SmpError::Shutdown));
    }
}
