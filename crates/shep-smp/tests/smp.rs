use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::join_all;
use shep_smp::{CoreId, Smp, SmpConfig, SmpError, current_core};

fn config(cores: usize) -> SmpConfig {
    SmpConfig {
        pin: false,
        ..SmpConfig::with_cores(cores)
    }
}

/// Cores with no state of their own.
async fn smp(cores: usize) -> Smp<()> {
    Smp::start(config(cores), |_| async {}).await.unwrap()
}

/// Stops `smp`'s cores from a thread of its own: `shutdown` blocks, so it
/// must not run on the test's runtime.
async fn stop<S: 'static>(smp: &Smp<S>) {
    let smp = smp.clone();
    let (done, finished) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        smp.shutdown();
        let _ = done.send(());
    });
    let _ = finished.await;
}

#[tokio::test(flavor = "local")]
async fn work_runs_on_the_core_it_was_sent_to() {
    let smp = smp(4).await;
    assert_eq!(current_core(), None);
    for core in smp.core_ids() {
        let (on, thread) = smp
            .submit_to(core, |_| async {
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
    stop(&smp).await;
}

#[tokio::test(flavor = "local")]
async fn a_cores_futures_need_not_be_send() {
    let smp = smp(2).await;
    let n = smp
        .submit_to(CoreId(1), |_| async {
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
    stop(&smp).await;
}

#[tokio::test(flavor = "local")]
async fn zero_cores_means_one() {
    let smp = smp(0).await;
    assert_eq!(smp.cores(), 1);
    assert_eq!(smp.submit_to(CoreId(0), |_| async { 7 }).await, Ok(7));
    assert_eq!(
        smp.submit_to(CoreId(1), |_| async {}).await,
        Err(SmpError::NoSuchCore(CoreId(1)))
    );
    stop(&smp).await;
}

#[tokio::test(flavor = "local")]
async fn calls_to_every_core_answer_in_core_order() {
    let smp = smp(5).await;
    let cores = join_all(
        smp.core_ids()
            .map(|core| smp.submit_to(core, move |_| async move { (core, current_core()) })),
    )
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(cores.len(), 5);
    for (i, (asked, ran)) in cores.into_iter().enumerate() {
        assert_eq!(asked, CoreId(i));
        assert_eq!(ran, Some(CoreId(i)));
    }
    stop(&smp).await;
}

/// One sender's messages to one core start in the order they were sent.
#[tokio::test(flavor = "local")]
async fn messages_from_one_sender_start_in_order() {
    let smp = smp(2).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    // A call is queued on its first poll, so polling each once, in turn,
    // queues them in order.
    let mut calls = Vec::new();
    for i in 0..500 {
        let seen = seen.clone();
        let mut call =
            Box::pin(smp.submit_to(
                CoreId(1),
                move |_| async move { seen.lock().unwrap().push(i) },
            ));
        let _ = futures::poll!(call.as_mut());
        calls.push(call);
    }
    for r in join_all(calls).await {
        r.unwrap();
    }
    // A call queued after all of them starts after them.
    smp.submit_to(CoreId(1), |_| async {}).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), (0..500).collect::<Vec<_>>());
    stop(&smp).await;
}

#[tokio::test(flavor = "local")]
async fn cores_call_each_other() {
    let smp = smp(3).await;
    let inner = smp.clone();
    // Core 0 asks core 1, which asks core 2.
    let path = smp
        .submit_to(CoreId(0), move |_| async move {
            let inner2 = inner.clone();
            let rest = inner
                .submit_to(CoreId(1), move |_| async move {
                    let last = inner2
                        .submit_to(CoreId(2), |_| async { current_core().unwrap() })
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
    stop(&smp).await;
}

/// Many concurrent calls across all pairs of cores, each core both calling
/// and serving, all complete.
#[tokio::test(flavor = "local")]
async fn concurrent_calls_between_all_cores_complete() {
    let smp = smp(4).await;
    let total = Arc::new(AtomicUsize::new(0));
    let inner = smp.clone();
    let t = total.clone();
    let calls = smp.core_ids().map(|from| {
        let smp = inner.clone();
        let total = t.clone();
        inner.submit_to(from, move |_| async move {
            let mut calls = Vec::new();
            for i in 0..500 {
                let to = CoreId((from.0 + i) % smp.cores());
                let total = total.clone();
                let smp = smp.clone();
                calls.push(tokio::task::spawn_local(async move {
                    let got = smp
                        .submit_to(to, move |_| async move { (current_core(), i) })
                        .await
                        .unwrap();
                    assert_eq!(got, (Some(to), i));
                    total.fetch_add(1, Ordering::SeqCst);
                }));
            }
            for c in calls {
                c.await.unwrap();
            }
        })
    });
    for r in join_all(calls).await {
        r.unwrap();
    }
    assert_eq!(total.load(Ordering::SeqCst), 4 * 500);
    stop(&smp).await;
}

#[tokio::test(flavor = "local")]
async fn a_panic_fails_that_call_and_the_core_survives() {
    let smp = smp(2).await;
    let r = smp.submit_to(CoreId(1), |_| async { panic!("boom") }).await;
    assert_eq!(r, Err::<(), _>(SmpError::Panicked));
    assert_eq!(smp.submit_to(CoreId(1), |_| async { 1 }).await, Ok(1));
    stop(&smp).await;
}

#[tokio::test(flavor = "local")]
async fn shutdown_fails_pending_and_later_work() {
    let smp = smp(2).await;
    let started = Arc::new(tokio::sync::Notify::new());
    let s = started.clone();
    let pending = tokio::task::spawn_local({
        let smp = smp.clone();
        async move {
            smp.submit_to(CoreId(1), move |_| async move {
                s.notify_one();
                std::future::pending::<()>().await
            })
            .await
        }
    });
    started.notified().await;
    stop(&smp).await;
    assert_eq!(pending.await.unwrap(), Err(SmpError::Shutdown));
    assert_eq!(
        smp.submit_to(CoreId(0), |_| async {}).await,
        Err(SmpError::Shutdown)
    );
    // Idempotent.
    stop(&smp).await;
}

/// Each core has its own state, built by `make` on that core; calls get the
/// target core's.
#[tokio::test(flavor = "local")]
async fn each_core_has_its_own_state() {
    let smp = Smp::start(config(3), |core| async move { RefCell::new(core.0 * 100) })
        .await
        .unwrap();
    for core in smp.core_ids() {
        smp.submit_to(core, |c| async move { *c.borrow_mut() += 1 })
            .await
            .unwrap();
    }
    smp.submit_to(CoreId(2), |c| async move { *c.borrow_mut() += 5 })
        .await
        .unwrap();
    let values = smp
        .invoke_on_all(|c| async move { *c.borrow() })
        .await
        .unwrap();
    assert_eq!(values, vec![1, 101, 206]);
    stop(&smp).await;
}

/// A core's state is built on that core, and dropped there when the cores
/// shut down, after the core's tasks: work still running can't outlive the
/// state it uses.
#[tokio::test(flavor = "local")]
async fn states_live_and_die_on_their_cores() {
    /// What was dropped, and on which core.
    type Log = Arc<Mutex<Vec<(&'static str, Option<CoreId>)>>>;
    struct Probe {
        what: &'static str,
        built_on: Option<CoreId>,
        log: Log,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            assert_eq!(current_core(), self.built_on);
            self.log.lock().unwrap().push((self.what, current_core()));
        }
    }
    let log: Log = Arc::default();
    let l = log.clone();
    let smp = Smp::start(config(3), move |_| {
        let log = l.clone();
        async move {
            Probe {
                what: "state",
                built_on: current_core(),
                log,
            }
        }
    })
    .await
    .unwrap();
    // A task on every core that never finishes, holding a probe of its own.
    let l = log.clone();
    smp.invoke_on_all(move |_| {
        let log = l.clone();
        async move {
            let task = Probe {
                what: "task",
                built_on: current_core(),
                log,
            };
            tokio::task::spawn_local(async move {
                let _task = task;
                std::future::pending::<()>().await
            });
        }
    })
    .await
    .unwrap();
    stop(&smp).await;
    let log = log.lock().unwrap().clone();
    assert_eq!(log.len(), 6);
    for core in smp.core_ids() {
        let on_core: Vec<_> = log
            .iter()
            .filter(|(_, on)| *on == Some(core))
            .map(|(what, _)| *what)
            .collect();
        assert_eq!(on_core, ["task", "state"], "on {core}");
    }
}

/// `shutdown` blocks until the cores' threads exit, so it refuses to run on
/// a runtime, where it would block every task on that thread.
#[tokio::test(flavor = "local")]
#[should_panic(expected = "Smp::shutdown blocks")]
async fn shutdown_refuses_to_block_a_runtime() {
    let smp = smp(1).await;
    smp.shutdown();
}
