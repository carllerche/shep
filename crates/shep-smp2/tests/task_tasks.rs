use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::{Future, pending, poll_fn};
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, ThreadId};
use std::time::Duration;

use shep_futures::future::block_on;
use shep_smp2::task::{JoinError, Schedule, Task, TaskId, new_task};

struct Queue(mpsc::Sender<TaskId>);

impl Schedule for Queue {
    fn schedule(&self, task: TaskId) {
        // Late notifications after executor shutdown are harmless.
        let _ = self.0.send(task);
    }
}

fn queue() -> (Arc<Queue>, mpsc::Receiver<TaskId>) {
    let (send, recv) = mpsc::channel();
    (Arc::new(Queue(send)), recv)
}

fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
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
        assert_eq!(thread::current().id(), self.owner);
        self.drops.set(self.drops.get() + 1);
    }
}

#[test]
fn tasks_are_lazy_and_support_local_outputs() {
    let (queue, ready) = queue();
    let value = Rc::new(23);
    let output = value.clone();
    let polls = Rc::new(Cell::new(0));
    let count = polls.clone();
    let (mut task, mut join) = new_task(
        async move {
            count.set(count.get() + 1);
            output
        },
        queue,
    );
    assert_eq!(polls.get(), 0);
    assert!(!task.is_finished());
    assert!(!join.is_finished());
    assert!(poll(&mut join).is_pending());
    assert!(ready.try_recv().is_err());
    assert!(task.run());
    assert!(task.run());
    assert_eq!(polls.get(), 1);
    assert!(task.is_finished());
    assert!(join.is_finished());
    let result = block_on(join).unwrap();
    assert!(Rc::ptr_eq(&result, &value));
}

#[test]
fn self_wakes_are_coalesced_and_yield_to_another_turn() {
    let (queue, ready) = queue();
    let polls = Rc::new(Cell::new(0));
    let count = polls.clone();
    let (mut task, join) = new_task(
        poll_fn(move |cx| {
            count.set(count.get() + 1);
            if count.get() == 1 {
                for _ in 0..100 {
                    cx.waker().wake_by_ref();
                }
                Poll::Pending
            } else {
                Poll::Ready(42)
            }
        }),
        queue,
    );
    assert!(!task.run());
    assert_eq!(polls.get(), 1);
    assert_eq!(ready.try_recv().unwrap(), task.id());
    assert!(ready.try_recv().is_err());
    assert!(task.run());
    assert_eq!(polls.get(), 2);
    assert_eq!(block_on(join), Ok(42));
}

#[test]
fn foreign_wakes_during_polling_are_rescheduled() {
    let (queue, ready) = queue();
    let mut first = true;
    let (mut task, join) = new_task(
        poll_fn(move |cx| {
            if std::mem::take(&mut first) {
                let waker = cx.waker().clone();
                thread::spawn(move || {
                    for _ in 0..100 {
                        waker.wake_by_ref();
                    }
                    waker.wake();
                })
                .join()
                .unwrap();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }),
        queue,
    );
    assert!(!task.run());
    assert_eq!(ready.try_recv().unwrap(), task.id());
    assert!(ready.try_recv().is_err());
    assert!(task.run());
    assert_eq!(block_on(join), Ok(()));
}

#[test]
fn pending_tasks_are_not_polled_without_a_notification() {
    let (queue, _) = queue();
    let polls = Rc::new(Cell::new(0));
    let count = polls.clone();
    let (mut task, join) = new_task(
        poll_fn(move |_| {
            count.set(count.get() + 1);
            Poll::<()>::Pending
        }),
        queue,
    );
    for _ in 0..10 {
        assert!(!task.run());
    }
    assert_eq!(polls.get(), 1);
    drop(task);
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
}

#[test]
fn tokio_channels_work_without_a_tokio_runtime() {
    // A tiny executor demonstrates the ownership/notification contract.
    let (queue, ready) = queue();
    let mut tasks = BTreeMap::<TaskId, Task>::new();
    let (send, recv) = tokio::sync::oneshot::channel::<usize>();
    let local = Rc::new(7);
    let (task, join) = new_task(async move { recv.await.unwrap() + *local }, queue.clone());
    let id = task.id();
    tasks.insert(id, task);
    queue.schedule(id);

    let id = ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(!tasks.get_mut(&id).unwrap().run());
    thread::spawn(move || send.send(35).unwrap())
        .join()
        .unwrap();
    let id = ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(tasks.get_mut(&id).unwrap().run());
    tasks.remove(&id);
    assert_eq!(block_on(join), Ok(42));
}

#[test]
fn many_foreign_wakes_coalesce_while_idle() {
    let (queue, ready) = queue();
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    let (mut task, join) = new_task(
        poll_fn(move |cx| {
            *save.borrow_mut() = Some(cx.waker().clone());
            Poll::<()>::Pending
        }),
        queue,
    );
    assert!(!task.run());
    let waker = saved.borrow_mut().take().unwrap();
    thread::scope(|scope| {
        for _ in 0..8 {
            let waker = waker.clone();
            scope.spawn(move || {
                for _ in 0..100 {
                    waker.wake_by_ref();
                }
            });
        }
    });
    assert_eq!(ready.try_recv().unwrap(), task.id());
    assert!(ready.try_recv().is_err());
    drop(task);
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
    thread::spawn(move || waker.wake()).join().unwrap();
    assert!(ready.try_recv().is_err());
}

#[test]
fn cancelling_releases_the_future_before_foreign_wakers() {
    let (queue, ready) = queue();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    let (mut task, mut join) = new_task(
        poll_fn(move |cx| {
            let _ = &probe;
            *save.borrow_mut() = Some(cx.waker().clone());
            Poll::<()>::Pending
        }),
        queue,
    );
    assert!(!task.run());
    assert!(poll(&mut join).is_pending());
    let waker = saved.borrow_mut().take().unwrap();
    drop(task);
    assert_eq!(drops.get(), 1);
    assert!(join.is_finished());
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
    thread::spawn(move || {
        waker.wake_by_ref();
        drop(waker);
    })
    .join()
    .unwrap();
    assert!(ready.try_recv().is_err());
}

#[test]
fn cancellation_before_the_first_poll_drops_captures() {
    let (queue, _) = queue();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    let (task, join) = new_task(
        async move {
            let _probe = probe;
            pending::<()>().await;
        },
        queue,
    );
    drop(task);
    assert_eq!(drops.get(), 1);
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
}

#[test]
fn detaching_before_or_after_completion_drops_the_output_locally() {
    for detach_first in [false, true] {
        let (queue, _) = queue();
        let drops = Rc::new(Cell::new(0));
        let probe = Probe::new(&drops);
        let (mut task, join) = new_task(async move { probe }, queue);
        if detach_first {
            drop(join);
            assert!(task.run());
        } else {
            assert!(task.run());
            assert_eq!(drops.get(), 0);
            // The join handle can outlive the completed executor task.
            drop(task);
            drop(join);
        }
        assert_eq!(drops.get(), 1);
    }
}

#[test]
fn dropping_the_join_handle_does_not_cancel_a_pending_task() {
    let (queue, ready) = queue();
    let (send, recv) = tokio::sync::oneshot::channel();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    let (mut task, mut join) = new_task(
        async move {
            recv.await.unwrap();
            probe
        },
        queue,
    );
    assert!(!task.run());
    let parent = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = Waker::from(parent.clone());
    assert!(
        Pin::new(&mut join)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert_eq!(Arc::strong_count(&parent), 3);
    drop(join);
    assert_eq!(Arc::strong_count(&parent), 2);
    assert_eq!(drops.get(), 0);
    send.send(()).unwrap();
    assert_eq!(ready.try_recv().unwrap(), task.id());
    assert!(task.run());
    assert_eq!(drops.get(), 1);
    assert_eq!(parent.0.load(Ordering::Relaxed), 0);
}

#[test]
fn only_the_latest_join_waker_is_notified_and_released() {
    let (queue, _) = queue();
    let (mut task, mut join) = new_task(async { 1 }, queue);
    let old = Arc::new(CountWake(AtomicUsize::new(0)));
    let latest = Arc::new(CountWake(AtomicUsize::new(0)));
    for parent in [&old, &latest, &latest] {
        let waker = Waker::from(parent.clone());
        assert!(
            Pin::new(&mut join)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }
    assert_eq!(Arc::strong_count(&old), 1);
    assert_eq!(Arc::strong_count(&latest), 2);
    assert!(task.run());
    assert_eq!(old.0.load(Ordering::Relaxed), 0);
    assert_eq!(latest.0.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&latest), 1);
    assert_eq!(block_on(join), Ok(1));
}

#[test]
fn a_panicking_future_fails_its_join_and_releases_captures() {
    let (queue, _) = queue();
    let drops = Rc::new(Cell::new(0));
    let probe = Probe::new(&drops);
    let (mut task, join) = new_task(
        async move {
            let _probe = probe;
            panic!("future panic");
        },
        queue.clone(),
    );
    assert!(task.run());
    assert_eq!(drops.get(), 1);
    assert_eq!(block_on(join), Err(JoinError::Panicked));
    let (mut task, join) = new_task(async { 3 }, queue);
    assert!(task.run());
    assert_eq!(block_on(join), Ok(3));
}

#[test]
fn panicking_future_destructors_are_not_dropped_twice() {
    struct PanicsOnDrop {
        drops: Rc<Cell<usize>>,
        ready: bool,
    }
    impl Future for PanicsOnDrop {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            if self.ready {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }
    impl Drop for PanicsOnDrop {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            panic!("destructor panic");
        }
    }
    for ready in [true, false] {
        let (queue, _) = queue();
        let drops = Rc::new(Cell::new(0));
        let (mut task, join) = new_task(
            PanicsOnDrop {
                drops: drops.clone(),
                ready,
            },
            queue,
        );
        assert_eq!(task.run(), ready);
        drop(task);
        assert_eq!(drops.get(), 1);
        assert_eq!(
            block_on(join),
            Err(if ready {
                JoinError::Panicked
            } else {
                JoinError::Cancelled
            })
        );
    }
}

#[test]
fn pinned_futures_keep_their_address() {
    struct Pinned {
        address: Cell<*const Pinned>,
        _pin: PhantomPinned,
    }
    impl Future for Pinned {
        type Output = ();
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let address = &*self as *const Self;
            if self.address.get().is_null() {
                self.address.set(address);
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                assert_eq!(self.address.get(), address);
                Poll::Ready(())
            }
        }
    }
    impl Drop for Pinned {
        fn drop(&mut self) {
            assert_eq!(self.address.get(), self as *const Self);
        }
    }
    let (queue, _) = queue();
    let (mut task, join) = new_task(
        Pinned {
            address: Cell::new(std::ptr::null()),
            _pin: PhantomPinned,
        },
        queue,
    );
    assert!(!task.run());
    let mut moved_task = task;
    assert!(moved_task.run());
    assert_eq!(block_on(join), Ok(()));
}

#[test]
#[should_panic(expected = "JoinHandle polled after completion")]
fn polling_a_consumed_join_handle_panics() {
    let (queue, _) = queue();
    let (mut task, mut join) = new_task(async {}, queue);
    assert!(task.run());
    assert_eq!(poll(&mut join), Poll::Ready(Ok(())));
    let _ = poll(&mut join);
}

#[test]
fn stale_notifications_cannot_identify_a_new_task() {
    let (queue, ready) = queue();
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    let (mut task, join) = new_task(
        poll_fn(move |cx| {
            *save.borrow_mut() = Some(cx.waker().clone());
            Poll::<()>::Pending
        }),
        queue.clone(),
    );
    let old_id = task.id();
    assert!(!task.run());
    saved.borrow_mut().take().unwrap().wake();
    drop(task);
    drop(join);
    let (new, _) = new_task(async {}, queue);
    assert_ne!(old_id, new.id());
    assert_eq!(ready.try_recv().unwrap(), old_id);
}

#[test]
fn a_future_can_detach_its_own_join_while_polling() {
    let (queue, _) = queue();
    let slot = Rc::new(RefCell::new(None));
    let inner = slot.clone();
    let (mut task, join) = new_task(
        poll_fn(move |_| {
            drop(inner.borrow_mut().take());
            Poll::Ready(7)
        }),
        queue,
    );
    *slot.borrow_mut() = Some(join);
    assert!(task.run());
    assert!(slot.borrow().is_none());
}

#[test]
fn pinned_pending_future_is_dropped_in_place_on_cancellation() {
    struct Pinned {
        address: Cell<*const Pinned>,
        dropped: Rc<Cell<bool>>,
        _pin: PhantomPinned,
    }
    impl Future for Pinned {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            self.address.set(&*self);
            Poll::Pending
        }
    }
    impl Drop for Pinned {
        fn drop(&mut self) {
            assert_eq!(self.address.get(), self as *const Self);
            self.dropped.set(true);
        }
    }
    let (queue, _) = queue();
    let dropped = Rc::new(Cell::new(false));
    let (mut task, join) = new_task(
        Pinned {
            address: Cell::new(std::ptr::null()),
            dropped: dropped.clone(),
            _pin: PhantomPinned,
        },
        queue,
    );
    assert!(!task.run());
    drop(task);
    assert!(dropped.get());
    assert_eq!(block_on(join), Err(JoinError::Cancelled));
}

#[test]
fn foreign_wakes_can_race_with_cancellation_and_deallocation() {
    let (queue, _) = queue();
    let saved = Rc::new(RefCell::new(None));
    let save = saved.clone();
    let (mut task, join) = new_task(
        poll_fn(move |cx| {
            *save.borrow_mut() = Some(cx.waker().clone());
            Poll::<()>::Pending
        }),
        queue,
    );
    assert!(!task.run());
    let waker = saved.borrow_mut().take().unwrap();
    let start = Arc::new(std::sync::Barrier::new(2));
    thread::scope(|scope| {
        let start_remote = start.clone();
        scope.spawn(move || {
            start_remote.wait();
            for _ in 0..1000 {
                waker.wake_by_ref();
            }
            waker.wake();
        });
        start.wait();
        drop(task);
        assert_eq!(block_on(join), Err(JoinError::Cancelled));
    });
}

#[test]
fn panicking_join_waker_drop_keeps_replacement_storage_valid() {
    struct PanicsOnDrop;
    // The Arc destructor is the behavior under test; Waker::noop has none.
    #[allow(clippy::manual_noop_waker)]
    impl Wake for PanicsOnDrop {
        fn wake(self: Arc<Self>) {}
    }
    impl Drop for PanicsOnDrop {
        fn drop(&mut self) {
            panic!("waker drop panic");
        }
    }
    let (queue, _) = queue();
    let (mut task, mut join) = new_task(async { 42 }, queue);
    let waker = Waker::from(Arc::new(PanicsOnDrop));
    assert!(
        Pin::new(&mut join)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);
    // Replacement stores the new waker before dropping the previous one.
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = poll(&mut join);
        }))
        .is_err()
    );
    assert!(poll(&mut join).is_pending());
    assert!(task.run());
    assert_eq!(block_on(join), Ok(42));
}
