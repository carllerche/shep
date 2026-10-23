use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shep_smp::{CoreId, Sharded, Smp, SmpConfig, SmpError, current_core};

fn smp(cores: usize) -> Smp {
    Smp::start(SmpConfig {
        pin: false,
        ..SmpConfig::with_cores(cores)
    })
    .unwrap()
}

#[tokio::test(flavor = "local")]
async fn work_runs_on_the_core_it_was_sent_to() {
    let smp = smp(4);
    assert_eq!(current_core(), None);
    for core in smp.core_ids() {
        let (on, thread) = smp
            .submit_to(core, || async {
                (
                    current_core(),
                    std::thread::current().name().map(String::from),
                )
            })
            .await
            .unwrap();
        assert_eq!(on, Some(core));
        assert_eq!(
            thread.as_deref(),
            Some(format!("shep-core-{}", core.0).as_str())
        );
    }
    smp.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn a_cores_futures_need_not_be_send() {
    let smp = smp(2);
    let n = smp
        .submit_to(CoreId(1), || async {
            let shared = Rc::new(RefCell::new(0));
            for _ in 0..3 {
                tokio::task::yield_now().await;
                *shared.borrow_mut() += 1;
            }
            *shared.borrow()
        })
        .await
        .unwrap();
    assert_eq!(n, 3);
    smp.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn zero_cores_means_one() {
    let smp = smp(0);
    assert_eq!(smp.cores(), 1);
    assert_eq!(smp.submit_to(CoreId(0), || async { 7 }).await, Ok(7));
    assert_eq!(
        smp.submit_to(CoreId(1), || async {}).await,
        Err(SmpError::NoSuchCore(CoreId(1)))
    );
    smp.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn submit_to_all_answers_in_core_order() {
    let smp = smp(5);
    let cores = smp
        .submit_to_all(|core| async move { (core, current_core()) })
        .await
        .unwrap();
    assert_eq!(cores.len(), 5);
    for (i, (asked, ran)) in cores.into_iter().enumerate() {
        assert_eq!(asked, CoreId(i));
        assert_eq!(ran, Some(CoreId(i)));
    }
    smp.shutdown().await;
}

/// One sender's messages to one core start in the order they were sent.
#[tokio::test(flavor = "local")]
async fn messages_from_one_sender_start_in_order() {
    let smp = smp(2);
    let seen = Arc::new(Mutex::new(Vec::new()));
    for i in 0..500 {
        let seen = seen.clone();
        smp.spawn_on(
            CoreId(1),
            move || async move { seen.lock().unwrap().push(i) },
        )
        .await
        .unwrap();
    }
    // A round trip through the same mailbox runs after everything before it.
    smp.submit_to(CoreId(1), || async {}).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), (0..500).collect::<Vec<_>>());
    smp.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn cores_call_each_other() {
    let smp = smp(3);
    let inner = smp.clone();
    // Core 0 asks core 1, which asks core 2.
    let path = smp
        .submit_to(CoreId(0), move || async move {
            let inner2 = inner.clone();
            let rest = inner
                .submit_to(CoreId(1), move || async move {
                    let last = inner2
                        .submit_to(CoreId(2), || async { current_core().unwrap() })
                        .await
                        .unwrap();
                    (current_core().unwrap(), last)
                })
                .await
                .unwrap();
            (current_core().unwrap(), rest)
        })
        .await
        .unwrap();
    assert_eq!(path, (CoreId(0), (CoreId(1), CoreId(2))));
    smp.shutdown().await;
}

/// Many concurrent calls across all pairs of cores, through small mailboxes,
/// all complete: a core waiting to send never stops draining its own mailbox.
#[tokio::test(flavor = "local")]
async fn concurrent_calls_between_all_cores_complete() {
    let smp = Smp::start(SmpConfig {
        pin: false,
        mailbox_capacity: 4,
        ..SmpConfig::with_cores(4)
    })
    .unwrap();
    let total = Arc::new(AtomicUsize::new(0));
    let inner = smp.clone();
    let t = total.clone();
    smp.submit_to_all(move |from| {
        let smp = inner.clone();
        let total = t.clone();
        async move {
            let mut calls = Vec::new();
            for i in 0..500 {
                let to = CoreId((from.0 + i) % smp.cores());
                let total = total.clone();
                let smp = smp.clone();
                calls.push(tokio::task::spawn_local(async move {
                    let got = smp
                        .submit_to(to, move || async move { (current_core(), i) })
                        .await
                        .unwrap();
                    assert_eq!(got, (Some(to), i));
                    total.fetch_add(1, Ordering::SeqCst);
                }));
            }
            for c in calls {
                c.await.unwrap();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(total.load(Ordering::SeqCst), 4 * 500);
    smp.shutdown().await;
}

/// A full mailbox makes senders wait instead of queueing without bound.
#[tokio::test(flavor = "local")]
async fn a_full_mailbox_pushes_back() {
    let smp = Smp::start(SmpConfig {
        pin: false,
        mailbox_capacity: 1,
        ..SmpConfig::with_cores(2)
    })
    .unwrap();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    // Block core 1's thread, so it stops draining its mailbox.
    let r = release_rx.clone();
    smp.spawn_on(CoreId(1), move || async move {
        r.lock().unwrap().recv().unwrap();
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    // One message fits.
    smp.spawn_on(CoreId(1), || async {}).await.unwrap();
    // The next waits.
    let blocked = tokio::time::timeout(
        Duration::from_millis(100),
        smp.spawn_on(CoreId(1), || async {}),
    )
    .await;
    assert!(blocked.is_err(), "the send should have waited for room");
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), smp.spawn_on(CoreId(1), || async {}))
        .await
        .expect("room after the core resumed")
        .unwrap();
    smp.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn a_panic_fails_that_call_and_the_core_survives() {
    let smp = smp(2);
    let r = smp.submit_to(CoreId(1), || async { panic!("boom") }).await;
    assert_eq!(r, Err::<(), _>(SmpError::Panicked));
    assert_eq!(smp.submit_to(CoreId(1), || async { 1 }).await, Ok(1));
    smp.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn shutdown_fails_pending_and_later_work() {
    let smp = smp(2);
    let started = Arc::new(tokio::sync::Notify::new());
    let s = started.clone();
    let pending = tokio::task::spawn_local({
        let smp = smp.clone();
        async move {
            smp.submit_to(CoreId(1), move || async move {
                s.notify_one();
                std::future::pending::<()>().await
            })
            .await
        }
    });
    started.notified().await;
    smp.shutdown().await;
    assert_eq!(pending.await.unwrap(), Err(SmpError::Shutdown));
    assert_eq!(
        smp.submit_to(CoreId(0), || async {}).await,
        Err(SmpError::Shutdown)
    );
    // Idempotent.
    smp.shutdown().await;
}

/// Each core has its own instance; `local` reaches it without a hop.
#[tokio::test(flavor = "local")]
async fn sharded_services_have_one_instance_per_core() {
    let smp = smp(3);
    let counters = Sharded::start(&smp, |core| async move { RefCell::new(core.0 * 100) })
        .await
        .unwrap();
    for core in smp.core_ids() {
        counters
            .invoke_on(core, |c| async move { *c.borrow_mut() += 1 })
            .await
            .unwrap();
    }
    counters
        .invoke_on(CoreId(2), |c| async move { *c.borrow_mut() += 5 })
        .await
        .unwrap();
    let values = counters
        .invoke_on_all(|c| async move { *c.borrow() })
        .await
        .unwrap();
    assert_eq!(values, vec![1, 101, 206]);
    // `local` on a core is that core's instance.
    let c = counters.clone();
    let v = smp
        .submit_to(CoreId(1), move || async move { *c.local().borrow() })
        .await
        .unwrap();
    assert_eq!(v, 101);
    // Off the cores there's no instance.
    assert!(counters.try_local().is_none());
    smp.shutdown().await;
}

#[tokio::test(flavor = "local")]
async fn independent_services_dont_collide() {
    let smp = smp(2);
    let a = Sharded::start(&smp, |_| async { RefCell::new("a") })
        .await
        .unwrap();
    let b = Sharded::start(&smp, |_| async { RefCell::new(2u32) })
        .await
        .unwrap();
    let got = smp
        .submit_to(CoreId(0), {
            let (a, b) = (a.clone(), b.clone());
            move || async move { (*a.local().borrow(), *b.local().borrow()) }
        })
        .await
        .unwrap();
    assert_eq!(got, ("a", 2));
    smp.shutdown().await;
}

/// Instances are built and dropped on their own cores.
#[tokio::test(flavor = "local")]
async fn sharded_instances_live_and_die_on_their_cores() {
    /// Where each instance was built, and where it was dropped.
    type Seen = Arc<Mutex<Vec<(Option<CoreId>, Option<CoreId>)>>>;
    struct Probe {
        built_on: Option<CoreId>,
        dropped: Seen,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            self.dropped
                .lock()
                .unwrap()
                .push((self.built_on, current_core()));
        }
    }
    let smp = smp(3);
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let d = dropped.clone();
    let probes = Sharded::start(&smp, move |_| {
        let d = d.clone();
        async move {
            Probe {
                built_on: current_core(),
                dropped: d,
            }
        }
    })
    .await
    .unwrap();
    probes.stop().await.unwrap();
    let mut seen = dropped.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        (0..3)
            .map(|i| (Some(CoreId(i)), Some(CoreId(i))))
            .collect::<Vec<_>>()
    );
    // After stop, the service has no instances.
    let p = probes.clone();
    let r = smp
        .submit_to(CoreId(0), move || async move { p.try_local().is_none() })
        .await;
    assert_eq!(r, Ok(true));
    smp.shutdown().await;
}

/// Shutting down drops every service's instances, on their cores.
#[tokio::test(flavor = "local")]
async fn shutdown_drops_services() {
    struct Probe(Arc<AtomicUsize>);
    impl Drop for Probe {
        fn drop(&mut self) {
            assert!(current_core().is_some());
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let smp = smp(4);
    let dropped = Arc::new(AtomicUsize::new(0));
    let d = dropped.clone();
    let _probes = Sharded::start(&smp, move |_| {
        let d = d.clone();
        async move { Probe(d) }
    })
    .await
    .unwrap();
    smp.shutdown().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 4);
}
