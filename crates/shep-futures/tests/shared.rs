//! `Shared` under any interleaving of clones, polls, drops, and wakes.
//!
//! Each clone stands for a task: polling one records whether it is waiting,
//! and its waker marks it woken. A driver draws operations on the clones and
//! on the inner future (spurious wakes, finishing with a wake, or finishing
//! silently, to be found by some clone's next poll). Whatever happens:
//!
//! - every clone that resolves gets the inner future's output;
//! - the inner future is never polled after it finishes;
//! - no wakeup is lost: once the inner future finishes, polling only the
//!   clones that were woken resolves every clone still alive, whichever
//!   clone drove the inner future and whether or not it is still around.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use hegel::TestCase;
use hegel::generators as gs;
use shep_futures::FutureExt;
use shep_futures::future::Shared;

/// What the inner future resolves to.
const OUTPUT: u32 = 7;

/// The inner future: finishes once `output` is set, storing the waker it was
/// last polled with.
#[derive(Default)]
struct Gate {
    output: Cell<Option<u32>>,
    waker: RefCell<Option<Waker>>,
    finished: Cell<bool>,
}

struct Inner(Rc<Gate>);

impl Future for Inner {
    type Output = u32;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        let gate = &self.0;
        assert!(!gate.finished.get(), "inner polled after it finished");
        match gate.output.get() {
            Some(v) => {
                gate.finished.set(true);
                gate.waker.borrow_mut().take();
                Poll::Ready(v)
            }
            None => {
                *gate.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[derive(Default)]
struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct Task {
    shared: Shared<Inner>,
    flag: Arc<Flag>,
    /// Returned `Pending` and not woken since.
    waiting: bool,
}

impl Task {
    fn new(shared: Shared<Inner>) -> Task {
        Task {
            shared,
            flag: Arc::default(),
            waiting: false,
        }
    }

    fn woken(&self) -> bool {
        self.flag.0.load(Ordering::SeqCst)
    }

    fn poll(&mut self) -> Option<u32> {
        self.flag.0.store(false, Ordering::SeqCst);
        let waker = Waker::from(self.flag.clone());
        match Pin::new(&mut self.shared).poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(v) => Some(v),
            Poll::Pending => {
                self.waiting = true;
                None
            }
        }
    }
}

#[hegel::test(test_cases = 5000)]
fn no_lost_wakeups(tc: TestCase) {
    let gate = Rc::new(Gate::default());
    let mut tasks = vec![Task::new(Inner(gate.clone()).shared())];
    let mut resolved = 0;

    let steps = tc.draw(gs::integers::<usize>().max_value(40));
    for _ in 0..steps {
        let op = tc.draw(gs::integers::<u8>().max_value(6));
        if tasks.is_empty() {
            break;
        }
        let i = tc.draw(gs::integers::<usize>().max_value(tasks.len() - 1));
        match op {
            0 => {
                let clone = tasks[i].shared.clone();
                tasks.push(Task::new(clone));
            }
            1 | 2 => {
                if let Some(v) = tasks[i].poll() {
                    assert_eq!(v, OUTPUT);
                    tasks.swap_remove(i);
                    resolved += 1;
                }
            }
            3 => {
                tasks.swap_remove(i);
            }
            4 => {
                // A spurious wake of whatever polled the inner future last.
                if let Some(w) = gate.waker.borrow().as_ref() {
                    w.wake_by_ref();
                }
            }
            5 => finish(&gate),
            _ => gate.output.set(Some(OUTPUT)),
        }
    }
    tc.event_value("resolved", resolved as f64);

    // Finish the inner future, then behave like an executor: poll a task
    // only if it is new or was woken.
    finish(&gate);
    while let Some(i) = tasks.iter().position(|t| !t.waiting || t.woken()) {
        if let Some(v) = tasks[i].poll() {
            assert_eq!(v, OUTPUT);
            tasks.swap_remove(i);
        }
    }
    assert!(
        tasks.is_empty(),
        "{} clones wait forever after the inner future finished",
        tasks.len()
    );
}

/// Woken clones keep their place in the waker list, so a clone that starts
/// waiting after a wake isn't given a slot another clone still holds.
#[test]
fn a_woken_clone_keeps_its_slot() {
    let gate = Rc::new(Gate::default());
    let mut a = Task::new(Inner(gate.clone()).shared());
    let mut b = Task::new(a.shared.clone());
    assert_eq!(a.poll(), None);
    assert_eq!(b.poll(), None);
    // A spurious wake: both clones are woken, their wakers taken.
    gate.waker.borrow().as_ref().unwrap().wake_by_ref();
    let mut c = Task::new(a.shared.clone());
    assert_eq!(c.poll(), None);
    assert_eq!(a.poll(), None);
    finish(&gate);
    assert!(c.woken(), "the last clone to wait was never woken");
    for t in [&mut a, &mut b, &mut c] {
        assert_eq!(t.poll(), Some(OUTPUT));
    }
}

/// Makes the inner future ready and wakes it, unless it already finished.
fn finish(gate: &Gate) {
    if gate.finished.get() {
        return;
    }
    gate.output.set(Some(OUTPUT));
    if let Some(w) = gate.waker.borrow_mut().take() {
        w.wake();
    }
}

#[tokio::test(flavor = "local")]
async fn clones_all_get_the_output() {
    let runs = Rc::new(Cell::new(0));
    let shared = {
        let runs = runs.clone();
        async move {
            runs.set(runs.get() + 1);
            tokio::task::yield_now().await;
            String::from("done")
        }
        .shared()
    };
    let a = tokio::task::spawn_local(shared.clone());
    let b = tokio::task::spawn_local(shared.clone());
    assert_eq!(shared.await, "done");
    assert_eq!(a.await.unwrap(), "done");
    assert_eq!(b.await.unwrap(), "done");
    assert_eq!(runs.get(), 1);
}

/// The clone driving the inner future goes away; another takes over.
#[tokio::test(flavor = "local")]
async fn dropping_the_driver_strands_no_one() {
    let (tx, rx) = tokio::sync::oneshot::channel::<u32>();
    let shared = async move { rx.await.unwrap() }.shared();
    let mut driver = Box::pin(shared.clone());
    assert!(shep_futures::poll!(driver.as_mut()).is_pending());
    let waiter = tokio::task::spawn_local(shared);
    tokio::task::yield_now().await;
    drop(driver);
    tx.send(5).unwrap();
    assert_eq!(waiter.await.unwrap(), 5);
}

#[tokio::test(flavor = "local")]
async fn a_panic_poisons_every_clone() {
    let shared = async {
        tokio::task::yield_now().await;
        panic!("boom");
    }
    .shared();
    let other = shared.clone();
    let first = tokio::task::spawn_local(async move {
        let _: () = shared.await;
    });
    let err = first.await.unwrap_err();
    assert!(err.is_panic());
    let second = tokio::task::spawn_local(other);
    let err = second.await.unwrap_err();
    let msg = err.into_panic();
    assert_eq!(
        msg.downcast_ref::<&str>(),
        Some(&"Shared's inner future panicked")
    );
}
