use std::cell::{Cell, RefCell};
use std::future::{pending, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Poll, Wake, Waker};
use std::thread::{self, Thread, ThreadId};
use std::time::{Duration, Instant};

use shep_futures::future::block_on;
use shep_smp_exec::{Executor, JoinError, JoinHandle, spawn};

fn executor() -> Executor {
    Executor::new(Waker::noop().clone())
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
    let mut exec = executor();
    let value = Rc::new(42);
    let output = value.clone();
    let join = exec.spawn(async move { output });
    assert!(!join.is_finished());
    assert!(!exec.is_empty());
    assert!(exec.has_ready());
    assert_eq!(exec.run_ready(1), 1);
    assert!(!exec.has_ready());
    assert!(exec.is_empty());
    assert!(Rc::ptr_eq(&block_on(join).unwrap(), &value));
}

#[test]
fn tasks_spawn_children_and_join_them_without_borrow_conflicts() {
    let mut exec = executor();
    let value = Rc::new(RefCell::new(1));
    let local = value.clone();
    let join = exec.spawn(async move {
        let child = spawn(async move {
            spawn(async move { *local.borrow_mut() += 6 })
                .await
                .unwrap();
        });
        child.await.unwrap();
        42
    });
    while exec.has_ready() {
        exec.run_ready(2);
    }
    assert!(exec.is_empty());
    assert_eq!(block_on(join), Ok(42));
    assert_eq!(*value.borrow(), 7);
}

#[test]
fn ready_work_is_fifo_including_children_and_self_wakes() {
    let mut exec = executor();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = seen.clone();
    exec.spawn(async move {
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
    exec.spawn(async move { log.borrow_mut().push("sibling") });
    assert_eq!(exec.run_ready(4), 4);
    assert_eq!(
        *seen.borrow(),
        ["parent", "sibling", "child", "parent again"]
    );
    assert!(exec.is_empty());
}

#[test]
fn a_self_waking_task_cannot_exceed_the_turn_budget() {
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let mut exec = Executor::new(Waker::from(wake.clone()));
    let polls = Rc::new(Cell::new(0));
    let count = polls.clone();
    exec.spawn(poll_fn(move |cx| {
        count.set(count.get() + 1);
        cx.waker().wake_by_ref();
        Poll::<()>::Pending
    }));
    assert_eq!(exec.run_ready(0), 0);
    assert_eq!(polls.get(), 0);
    assert!(exec.has_ready());
    assert_eq!(exec.run_ready(7), 7);
    assert_eq!(polls.get(), 7);
    let sibling = exec.spawn(async { 42 });
    assert_eq!(exec.run_ready(2), 2);
    assert_eq!(polls.get(), 8);
    assert_eq!(block_on(sibling), Ok(42));
    assert_eq!(wake.0.load(Ordering::Relaxed), 0);
}

#[test]
fn foreign_channel_wakes_reach_the_driver_without_tokio_runtime() {
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let mut exec = Executor::new(Waker::from(wake.clone()));
    let (send, recv) = tokio::sync::oneshot::channel();
    let local = Rc::new(7);
    let join = exec.spawn(async move { recv.await.unwrap() + *local });
    assert_eq!(exec.run_ready(4), 1);
    assert!(!exec.has_ready());
    assert!(!exec.is_empty());
    thread::spawn(move || send.send(35).unwrap())
        .join()
        .unwrap();
    assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    assert!(exec.has_ready());
    assert_eq!(exec.run_ready(4), 1);
    assert_eq!(block_on(join), Ok(42));
    assert!(exec.is_empty());
}

#[test]
fn foreign_ready_work_is_processed_in_bounded_turns() {
    let mut exec = executor();
    let mut sends = Vec::new();
    let mut joins = Vec::new();
    for _ in 0..12 {
        let (send, recv) = tokio::sync::oneshot::channel();
        sends.push(send);
        joins.push(exec.spawn(async move { recv.await.unwrap() }));
    }
    assert_eq!(exec.run_ready(12), 12);
    assert!(!exec.has_ready());
    thread::spawn(move || {
        for send in sends {
            send.send(()).unwrap();
        }
    })
    .join()
    .unwrap();
    assert_eq!(exec.run_ready(2), 2);
    assert_eq!(joins.iter().filter(|j| j.is_finished()).count(), 2);
    while exec.has_ready() {
        assert!(exec.run_ready(2) <= 2);
    }
    assert!(exec.is_empty());
    for join in joins {
        assert_eq!(block_on(join), Ok(()));
    }
}

#[test]
fn owner_thread_wakes_outside_a_turn_bypass_the_driver() {
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let mut exec = Executor::new(Waker::from(wake.clone()));
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    let join = exec.spawn(poll_fn(move |cx| {
        *save.borrow_mut() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    assert_eq!(exec.run_ready(1), 1);
    assert!(!exec.has_ready());
    saved.borrow().as_ref().unwrap().wake_by_ref();
    assert_eq!(wake.0.load(Ordering::Relaxed), 0);
    assert!(exec.has_ready());
    assert_eq!(exec.run_ready(1), 1);
    drop(join);
}

#[test]
fn a_wake_from_another_executor_reaches_the_owner() {
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let mut owner = Executor::new(Waker::from(wake.clone()));
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    owner.spawn(poll_fn(move |cx| {
        *save.borrow_mut() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    owner.run_ready(1);
    let mut other = executor();
    other.spawn(async move { saved.borrow_mut().take().unwrap().wake() });
    assert_eq!(other.run_ready(4), 1);
    assert!(other.is_empty());
    assert_eq!(wake.0.load(Ordering::Relaxed), 0);
    assert!(owner.has_ready());
    assert_eq!(owner.run_ready(1), 1);
}

#[test]
fn pending_tasks_survive_without_wakers_until_shutdown() {
    let mut exec = executor();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    let join = exec.spawn(async move {
        let _probe = probe;
        pending::<()>().await;
    });
    exec.run_ready(8);
    assert!(!exec.has_ready());
    assert!(!exec.is_empty());
    assert_eq!(drops.get(), 0);
    exec.shutdown();
    assert!(exec.is_empty());
    assert!(!exec.has_ready());
    assert_eq!(drops.get(), 1);
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
    exec.shutdown();
    assert_eq!(drops.get(), 1);
}

#[test]
fn dropping_the_executor_cancels_queued_and_detached_tasks_locally() {
    let mut exec = executor();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    drop(exec.spawn(async move {
        let _probe = probe;
        pending::<()>().await;
    }));
    exec.run_ready(1);
    let probe = Probe::new(&drops);
    let queued = exec.spawn(async move {
        let _probe = probe;
        pending::<()>().await;
    });
    assert_eq!(drops.get(), 0);
    drop(exec);
    assert_eq!(drops.get(), 2);
    assert_eq!(block_on(queued), Err(JoinError::Cancelled));
}

#[test]
fn spawning_after_shutdown_cancels_without_polling() {
    let mut exec = executor();
    exec.shutdown();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    let join = exec.spawn(async move {
        let _probe = probe;
        panic!("a closed executor must not poll new work");
    });
    assert_eq!(drops.get(), 1);
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
    assert_eq!(exec.run_ready(1), 0);
    assert!(exec.is_empty());
    assert!(!exec.has_ready());
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
    let mut exec = executor();
    let drops = Rc::new(Cell::new(0));
    let results = Rc::new(RefCell::new(Vec::new()));
    let probe = SpawnOnDrop {
        results: results.clone(),
        child: Some(Probe::new(&drops)),
    };
    exec.spawn(async move {
        let _probe = probe;
        pending::<()>().await;
    });
    exec.run_ready(1);
    exec.shutdown();
    assert_eq!(drops.get(), 1);
    assert!(exec.is_empty());
    assert_eq!(
        block_on(results.borrow_mut().pop().unwrap()),
        Err(JoinError::Cancelled)
    );
}

#[test]
fn a_task_panic_does_not_stop_its_siblings_or_leak_context() {
    let mut exec = executor();
    let failed = exec.spawn(async { panic!("boom") });
    let sibling = exec.spawn(async { spawn(async { 42 }).await.unwrap() });
    while exec.has_ready() {
        exec.run_ready(4);
    }
    assert_eq!(block_on(failed), Err(JoinError::Panicked));
    assert_eq!(block_on(sibling), Ok(42));
    assert!(catch_unwind(|| spawn(async {})).is_err());
}

#[test]
fn nested_execution_is_rejected_and_the_outer_context_survives() {
    let mut outer = executor();
    let mut inner = executor();
    let nested = outer.spawn(async move { inner.run_ready(1) });
    let sibling = outer.spawn(async { spawn(async { 7 }).await.unwrap() });
    while outer.has_ready() {
        outer.run_ready(4);
    }
    assert_eq!(block_on(nested), Err(JoinError::Panicked));
    assert_eq!(block_on(sibling), Ok(7));
    assert!(catch_unwind(|| spawn(async {})).is_err());
}

#[test]
fn dropping_another_executor_inside_a_task_restores_the_outer_scope() {
    let mut outer = executor();
    let inner = executor();
    let pending = inner.spawn(pending::<()>());
    let join = outer.spawn(async move {
        drop(inner);
        assert_eq!(pending.await, Err(JoinError::Cancelled));
        spawn(async { 42 }).await.unwrap()
    });
    while outer.has_ready() {
        outer.run_ready(4);
    }
    assert_eq!(block_on(join), Ok(42));
}

#[test]
fn notifications_after_executor_drop_are_harmless() {
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let mut exec = Executor::new(Waker::from(wake.clone()));
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    let join = exec.spawn(poll_fn(move |cx| {
        *save.borrow_mut() = Some(cx.waker().clone());
        Poll::<()>::Pending
    }));
    exec.run_ready(1);
    let waker = saved.borrow_mut().take().unwrap();
    // A queued notification must not keep the executor or its tasks alive.
    waker.wake_by_ref();
    drop(exec);
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
    let before = wake.0.load(Ordering::Relaxed);
    thread::spawn(move || waker.wake()).join().unwrap();
    assert_eq!(wake.0.load(Ordering::Relaxed), before);
}

#[test]
fn an_idle_driver_can_wait_for_a_remote_wake() {
    // Test-only blocking driver. The production io_uring driver will supply
    // the same retained-wake contract through its eventfd.
    struct Unpark(Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut exec = Executor::new(Waker::from(Arc::new(Unpark(thread::current()))));
    let (send, recv) = tokio::sync::oneshot::channel();
    let join = exec.spawn(async move { recv.await.unwrap() });
    exec.run_ready(1);
    assert!(!exec.has_ready());
    let barrier = Arc::new(Barrier::new(2));
    let release = barrier.clone();
    let sender = thread::spawn(move || {
        release.wait();
        send.send(42).unwrap();
    });
    // Let the wake race with the empty check and parking.
    barrier.wait();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !join.is_finished() {
        assert!(Instant::now() < deadline, "lost driver wakeup");
        if exec.has_ready() {
            exec.run_ready(4);
        } else {
            thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }
    sender.join().unwrap();
    assert_eq!(block_on(join), Ok(42));
}

#[test]
fn spawn_outside_an_executor_is_rejected() {
    assert!(catch_unwind(AssertUnwindSafe(|| spawn(async {}))).is_err());
}
