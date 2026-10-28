use std::cell::{Cell, RefCell};
use std::future::{pending, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Poll, Wake, Waker};
use std::thread::{self, ThreadId};

use shep_futures::future::block_on;
use shep_smp2::task::{self, JoinError, JoinHandle, spawn};
use shep_smp2::{Config, Runtime};

fn runtime() -> Runtime {
    Runtime::new(Config::default()).unwrap()
}

/// A runtime whose foreign-wake driver waker only counts its calls.
fn counting_runtime() -> (Runtime, Arc<CountWake>) {
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let driver = Waker::from(wake.clone());
    let runtime = Runtime::build(Config::default(), None, |_| driver).unwrap();
    (runtime, wake)
}

struct CountWake(AtomicUsize);

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct Probe {
    owner: ThreadId,
    drops: Rc<Cell<usize>>,
}

impl Probe {
    fn new(drops: &Rc<Cell<usize>>) -> Self {
        Self {
            owner: thread::current().id(),
            drops: drops.clone(),
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        assert_eq!(self.owner, thread::current().id());
        self.drops.set(self.drops.get() + 1);
    }
}

#[test]
fn spawn_is_lazy_and_supports_local_futures_and_outputs() {
    let rt = runtime();
    let value = Rc::new(42);
    let output = value.clone();
    let join = rt.spawn(async move { output });
    assert!(!join.is_finished());
    assert!(!rt.is_empty());
    assert!(rt.has_ready());
    assert_eq!(rt.run_ready(), 1);
    assert!(!rt.has_ready());
    assert!(rt.is_empty());
    assert!(Rc::ptr_eq(&block_on(join).unwrap(), &value));
}

#[test]
fn tasks_spawn_children_and_join_them_without_borrow_conflicts() {
    let rt = runtime();
    let value = Rc::new(RefCell::new(1));
    let local = value.clone();
    let join = rt.spawn(async move {
        let child = spawn(async move {
            spawn(async move { *local.borrow_mut() += 6 })
                .await
                .unwrap();
        });
        child.await.unwrap();
        42
    });
    while rt.has_ready() {
        rt.run_ready();
    }
    assert!(rt.is_empty());
    assert_eq!(block_on(join), Ok(42));
    assert_eq!(*value.borrow(), 7);
}

#[test]
fn ready_work_is_fifo_and_turns_defer_children_and_self_wakes() {
    let rt = runtime();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = seen.clone();
    rt.spawn(async move {
        log.borrow_mut().push("parent");
        let child_log = log.clone();
        spawn(async move { child_log.borrow_mut().push("child") });
        let mut yielded = false;
        poll_fn(|cx| {
            if !std::mem::replace(&mut yielded, true) {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        log.borrow_mut().push("parent again");
    });
    let log = seen.clone();
    rt.spawn(async move { log.borrow_mut().push("sibling") });
    assert_eq!(rt.run_ready(), 2);
    assert_eq!(*seen.borrow(), ["parent", "sibling"]);
    assert_eq!(rt.run_ready(), 2);
    assert_eq!(
        *seen.borrow(),
        ["parent", "sibling", "child", "parent again"]
    );
    assert!(rt.is_empty());
}

#[test]
fn a_self_waking_task_runs_once_per_turn() {
    let (rt, wake) = counting_runtime();
    let polls = Rc::new(Cell::new(0));
    let count = polls.clone();
    rt.spawn(poll_fn(move |cx| {
        count.set(count.get() + 1);
        cx.waker().wake_by_ref();
        Poll::<()>::Pending
    }));
    for turn in 1..=7 {
        assert_eq!(rt.run_ready(), 1);
        assert_eq!(polls.get(), turn);
        assert!(rt.has_ready());
    }
    let sibling = rt.spawn(async { 42 });
    assert_eq!(rt.run_ready(), 2);
    assert_eq!(polls.get(), 8);
    assert_eq!(block_on(sibling), Ok(42));
    assert_eq!(wake.0.load(Ordering::Relaxed), 0);
}

#[test]
fn foreign_channel_wakes_reach_the_driver_without_tokio_runtime() {
    let (rt, wake) = counting_runtime();
    let (send, recv) = tokio::sync::oneshot::channel();
    let local = Rc::new(7);
    let join = rt.spawn(async move { recv.await.unwrap() + *local });
    assert_eq!(rt.run_ready(), 1);
    assert!(!rt.has_ready());
    assert!(!rt.is_empty());
    thread::spawn(move || send.send(35).unwrap())
        .join()
        .unwrap();
    assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    assert!(rt.has_ready());
    assert_eq!(rt.run_ready(), 1);
    assert_eq!(block_on(join), Ok(42));
    assert!(rt.is_empty());
}

#[test]
fn foreign_ready_work_is_imported_into_one_turn() {
    let rt = runtime();
    let mut sends = Vec::new();
    let mut joins = Vec::new();
    for _ in 0..12 {
        let (send, recv) = tokio::sync::oneshot::channel();
        sends.push(send);
        joins.push(rt.spawn(async move { recv.await.unwrap() }));
    }
    assert_eq!(rt.run_ready(), 12);
    assert!(!rt.has_ready());
    thread::spawn(move || {
        for send in sends {
            send.send(()).unwrap();
        }
    })
    .join()
    .unwrap();
    assert_eq!(rt.run_ready(), 12);
    assert!(rt.is_empty());
    for join in joins {
        assert_eq!(block_on(join), Ok(()));
    }
}

#[test]
fn owner_thread_wakes_outside_a_turn_bypass_the_driver() {
    let (rt, wake) = counting_runtime();
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    let join = rt.spawn(poll_fn(move |cx| {
        *save.borrow_mut() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    assert_eq!(rt.run_ready(), 1);
    assert!(!rt.has_ready());
    saved.borrow().as_ref().unwrap().wake_by_ref();
    assert_eq!(wake.0.load(Ordering::Relaxed), 0);
    assert!(rt.has_ready());
    assert_eq!(rt.run_ready(), 1);
    drop(join);
}

#[test]
fn a_wake_from_another_runtime_reaches_the_owner() {
    let (owner, wake) = counting_runtime();
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    owner.spawn(poll_fn(move |cx| {
        *save.borrow_mut() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    owner.run_ready();
    let other = runtime();
    other.spawn(async move { saved.borrow_mut().take().unwrap().wake() });
    assert_eq!(other.run_ready(), 1);
    assert!(other.is_empty());
    assert_eq!(wake.0.load(Ordering::Relaxed), 0);
    assert!(owner.has_ready());
    assert_eq!(owner.run_ready(), 1);
}

#[test]
fn pending_tasks_survive_without_wakers_until_shutdown() {
    let mut rt = runtime();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    let join = rt.spawn(async move {
        let _probe = probe;
        pending::<()>().await;
    });
    rt.run_ready();
    assert!(!rt.has_ready());
    assert!(!rt.is_empty());
    assert_eq!(drops.get(), 0);
    rt.shutdown().unwrap();
    assert!(rt.is_empty());
    assert!(!rt.has_ready());
    assert_eq!(drops.get(), 1);
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
    rt.shutdown().unwrap();
    assert_eq!(drops.get(), 1);
}

#[test]
fn dropping_the_runtime_cancels_queued_and_detached_tasks_locally() {
    let rt = runtime();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    drop(rt.spawn(async move {
        let _probe = probe;
        pending::<()>().await;
    }));
    rt.run_ready();
    let probe = Probe::new(&drops);
    let queued = rt.spawn(async move {
        let _probe = probe;
        pending::<()>().await;
    });
    assert_eq!(drops.get(), 0);
    drop(rt);
    assert_eq!(drops.get(), 2);
    assert_eq!(block_on(queued), Err(JoinError::Cancelled));
}

#[test]
fn spawning_after_shutdown_cancels_without_polling() {
    let mut rt = runtime();
    rt.shutdown().unwrap();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    let join = rt.spawn(async move {
        let _probe = probe;
        panic!("a closed runtime must not poll new work");
    });
    assert_eq!(drops.get(), 1);
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
    assert_eq!(rt.run_ready(), 0);
    assert!(rt.is_empty());
    assert!(!rt.has_ready());
}

#[test]
fn work_spawned_by_shutdown_destructors_is_cancelled() {
    struct SpawnOnDrop {
        results: Rc<RefCell<Vec<JoinHandle<()>>>>,
        child: Option<Probe>,
    }
    impl Drop for SpawnOnDrop {
        fn drop(&mut self) {
            let child = self.child.take().unwrap();
            self.results.borrow_mut().push(spawn(async move {
                let _child = child;
                panic!("shutdown must not run spawned children");
            }));
        }
    }
    let mut rt = runtime();
    let drops = Rc::new(Cell::new(0));
    let results = Rc::new(RefCell::new(Vec::new()));
    let probe = SpawnOnDrop {
        results: results.clone(),
        child: Some(Probe::new(&drops)),
    };
    rt.spawn(async move {
        let _probe = probe;
        pending::<()>().await;
    });
    rt.run_ready();
    rt.shutdown().unwrap();
    assert_eq!(drops.get(), 1);
    assert!(rt.is_empty());
    assert_eq!(
        block_on(results.borrow_mut().pop().unwrap()),
        Err(JoinError::Cancelled)
    );
}

#[test]
fn a_task_panic_does_not_stop_its_siblings_or_leak_context() {
    let rt = runtime();
    let failed = rt.spawn(async { panic!("boom") });
    let sibling = rt.spawn(async { spawn(async { 42 }).await.unwrap() });
    while rt.has_ready() {
        rt.run_ready();
    }
    assert_eq!(block_on(failed), Err(JoinError::Panicked));
    assert_eq!(block_on(sibling), Ok(42));
    assert!(catch_unwind(|| spawn(async {})).is_err());
    assert!(task::try_id().is_none());
}

#[test]
fn block_on_inside_a_task_is_rejected_and_the_outer_context_survives() {
    let rt = runtime();
    let mut inner = runtime();
    let nested = rt.spawn(async move { inner.block_on(async {}).unwrap() });
    let sibling = rt.spawn(async { spawn(async { 7 }).await.unwrap() });
    while rt.has_ready() {
        rt.run_ready();
    }
    assert_eq!(block_on(nested), Err(JoinError::Panicked));
    assert_eq!(block_on(sibling), Ok(7));
    assert!(catch_unwind(|| spawn(async {})).is_err());
}

#[test]
fn entering_a_second_runtime_is_rejected() {
    let outer = runtime();
    let inner = runtime();
    let _enter = outer.enter();
    assert!(catch_unwind(AssertUnwindSafe(|| outer.enter())).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| inner.enter())).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| inner.spawn(async {}))).is_err());
    // Operations on the entered runtime reuse its context.
    let join = outer.spawn(async { 1 });
    assert_eq!(outer.run_ready(), 1);
    assert_eq!(block_on(join), Ok(1));
}

#[test]
fn task_ids_name_the_running_task_and_restore_the_parent() {
    let rt = runtime();
    let ids = Rc::new(RefCell::new(Vec::new()));
    let log = ids.clone();
    let join = rt.spawn(async move {
        let parent = task::id();
        let child = spawn(async { task::id() });
        log.borrow_mut().push(parent);
        let child = child.await.unwrap();
        assert_ne!(child, parent);
        assert_eq!(task::id(), parent);
        parent
    });
    while rt.has_ready() {
        rt.run_ready();
    }
    assert_eq!(block_on(join), Ok(ids.borrow()[0]));
    assert!(task::try_id().is_none());
}

#[test]
fn notifications_after_runtime_drop_are_harmless() {
    let (rt, wake) = counting_runtime();
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    let join = rt.spawn(poll_fn(move |cx| {
        *save.borrow_mut() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    rt.run_ready();
    let waker = saved.borrow_mut().take().unwrap();
    // A queued notification must not keep the runtime or its tasks alive.
    waker.wake_by_ref();
    drop(rt);
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
    let before = wake.0.load(Ordering::Relaxed);
    thread::spawn(move || waker.wake()).join().unwrap();
    assert_eq!(wake.0.load(Ordering::Relaxed), before);
}

#[test]
fn an_idle_runtime_parks_until_a_remote_wake() {
    let mut rt = runtime();
    let (send, recv) = tokio::sync::oneshot::channel();
    let barrier = Arc::new(Barrier::new(2));
    let release = barrier.clone();
    let sender = thread::spawn(move || {
        release.wait();
        send.send(42).unwrap();
    });
    let result = rt
        .block_on(async move {
            // Let the wake race with the empty check and parking.
            barrier.wait();
            recv.await.unwrap()
        })
        .unwrap();
    sender.join().unwrap();
    assert_eq!(result, 42);
}

#[test]
fn spawn_outside_a_runtime_is_rejected() {
    assert!(catch_unwind(AssertUnwindSafe(|| spawn(async {}))).is_err());
}

#[test]
fn retained_handle_cannot_keep_execution_alive_after_runtime_drop() {
    let rt = runtime();
    let handle = rt.handle().clone();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    let join = handle.spawn(async move {
        let _probe = probe;
        pending::<()>().await;
    });
    drop(rt);
    assert_eq!(drops.get(), 1);
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
    assert_eq!(
        block_on(handle.spawn(async { 42 })),
        Err(JoinError::Cancelled)
    );
}

#[test]
fn recursive_scheduler_execution_is_rejected_without_losing_the_core() {
    let rt = Rc::new(runtime());
    let nested = rt.clone();
    let join = rt.spawn(async move { nested.run_ready() });
    let sibling = rt.spawn(async { 42 });
    assert_eq!(rt.run_ready(), 2);
    assert_eq!(block_on(join), Err(JoinError::Panicked));
    assert_eq!(block_on(sibling), Ok(42));
    let next = rt.spawn(async { 7 });
    assert_eq!(rt.run_ready(), 1);
    assert_eq!(block_on(next), Ok(7));
}
