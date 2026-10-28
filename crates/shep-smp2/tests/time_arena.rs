use std::cell::{Cell, RefCell};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Wake, Waker};

use shep_smp2::time::{Duration, Instant, Sleep, sleep_until};

#[path = "support/timer.rs"]
mod support;
use support::ManualTimer;

fn poll(sleep: Pin<&mut Sleep>) -> Poll<()> {
    sleep.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn default_reserves_capacity_but_unpolled_sleeps_do_not_occupy_slots() {
    let epoch = Instant::now();
    assert!(ManualTimer::new(epoch).capacity() >= 1024);
    let timer = ManualTimer::with_capacity(epoch, 0);
    let unpolled: Vec<_> = timer.with(|| {
        (0..32)
            .map(|_| sleep_until(epoch + Duration::from_millis(1)))
            .collect()
    });
    assert_eq!(timer.capacity(), 0);
    assert_eq!(timer.next_wake(), None);
    drop(unpolled);
    assert_eq!(timer.capacity(), 0);
    let mut reset = Box::pin(timer.with(|| sleep_until(epoch + Duration::from_millis(10))));
    reset.as_mut().reset(epoch + Duration::from_millis(1));
    assert!(timer.capacity() > 0, "reset registers before first poll");
    timer.advance(epoch + Duration::from_millis(1));
    assert!(poll(reset.as_mut()).is_ready());
}

#[test]
fn capacity_boundaries_preserve_entries_and_freed_slots_are_reused() {
    let epoch = Instant::now();
    // Miri's repeated mutable-slice retagging makes large filled slabs very
    // expensive. Exercise identical growth/reuse transitions with small pools;
    // the normal suite checks the full production-sized capacity boundaries.
    let capacities: &[usize] = if cfg!(miri) {
        &[0, 1, 7]
    } else {
        &[0, 1, 256, 1024, 4096]
    };
    for &requested in capacities {
        let timer = ManualTimer::with_capacity(epoch, requested);
        let initial = timer.capacity();
        assert!(initial >= requested);
        let deadline = epoch + Duration::from_millis(10);
        let mut sleeps = Vec::new();
        for _ in 0..initial {
            let mut sleep = Box::pin(timer.with(|| sleep_until(deadline)));
            assert!(poll(sleep.as_mut()).is_pending());
            sleeps.push(sleep);
            assert_eq!(timer.capacity(), initial);
        }
        let mut overflow = Box::pin(timer.with(|| sleep_until(deadline)));
        assert!(poll(overflow.as_mut()).is_pending());
        assert!(timer.capacity() > initial);
        timer.advance(deadline);
        for sleep in &mut sleeps {
            assert!(poll(sleep.as_mut()).is_ready());
        }
        assert!(poll(overflow.as_mut()).is_ready());
        drop(overflow);
        drop(sleeps);
        let retained = timer.capacity();
        let mut again = Vec::new();
        for _ in 0..retained {
            let mut sleep =
                Box::pin(timer.with(|| sleep_until(deadline + Duration::from_millis(1))));
            assert!(poll(sleep.as_mut()).is_pending());
            again.push(sleep);
            assert_eq!(timer.capacity(), retained);
        }
        drop(again);
        assert_eq!(timer.next_wake(), None);
        assert_eq!(timer.capacity(), retained);
    }
}

#[test]
fn elapsed_sleeps_retain_their_slots_until_drop_and_rearm_in_place() {
    let epoch = Instant::now();
    let timer = ManualTimer::with_capacity(epoch, 1);
    let capacity = timer.capacity();
    let mut elapsed = Box::pin(timer.with(|| sleep_until(epoch)));
    assert!(poll(elapsed.as_mut()).is_ready());
    for tick in 1..100 {
        let deadline = epoch + Duration::from_millis(tick);
        elapsed.as_mut().reset(deadline);
        assert!(poll(elapsed.as_mut()).is_pending());
        timer.advance(deadline);
        assert!(poll(elapsed.as_mut()).is_ready());
        assert_eq!(timer.capacity(), capacity);
    }
    let mut held = Vec::new();
    for _ in 1..capacity {
        let mut sleep = Box::pin(timer.with(|| sleep_until(epoch)));
        assert!(poll(sleep.as_mut()).is_ready());
        held.push(sleep);
    }
    assert_eq!(timer.capacity(), capacity);
    let mut extra = Box::pin(timer.with(|| sleep_until(epoch)));
    assert!(poll(extra.as_mut()).is_ready());
    assert!(
        timer.capacity() > capacity,
        "elapsed owners must still occupy their slots"
    );
}

thread_local! {
    static ON_CLONE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}

fn clone_callback_waker() -> Waker {
    fn clone(_: *const ()) -> RawWaker {
        if let Some(callback) = ON_CLONE.with(|slot| slot.take()) {
            callback();
        }
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    fn noop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    // SAFETY: Every operation ignores the null data pointer and owns no data.
    // Clones are independent, drops do nothing, and the callback is accessed
    // only through the current thread's TLS, so operations are Send + Sync.
    // A custom raw waker is necessary to make Clone itself call user code.
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

#[test]
fn repeated_pending_polls_retain_the_first_waker_without_cloning_replacements() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let epoch = Instant::now();
    let timer = ManualTimer::new(epoch);
    let deadline = epoch + Duration::from_millis(10);
    let mut sleep = Box::pin(timer.with(|| sleep_until(deadline)));
    let first = Arc::new(WakeCount::default());
    let replacement = Arc::new(WakeCount::default());
    for counter in [&first, &replacement] {
        let waker = Waker::from(counter.clone());
        assert!(
            sleep
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }
    // A needless clone would run this callback even though a waker is stored.
    let cloned = Rc::new(Cell::new(false));
    let observed = cloned.clone();
    ON_CLONE.with(|slot| slot.replace(Some(Box::new(move || observed.set(true)))));
    for _ in 0..3 {
        assert!(
            sleep
                .as_mut()
                .poll(&mut Context::from_waker(&clone_callback_waker()))
                .is_pending()
        );
    }
    ON_CLONE.with(|slot| slot.take());
    assert!(!cloned.get());
    timer.advance(deadline);
    assert_eq!(first.0.load(Ordering::Relaxed), 1);
    assert_eq!(replacement.0.load(Ordering::Relaxed), 0);
    assert!(poll(sleep.as_mut()).is_ready());
}

#[test]
fn cloning_the_first_waker_can_expire_the_sleep_being_polled() {
    let epoch = Instant::now();
    let timer = Rc::new(ManualTimer::with_capacity(epoch, 1));
    let deadline = epoch + Duration::from_millis(1);
    let mut sleep = Box::pin(timer.with(|| sleep_until(deadline)));
    let callback_timer = timer.clone();
    ON_CLONE
        .with(|slot| *slot.borrow_mut() = Some(Box::new(move || callback_timer.advance(deadline))));
    assert_eq!(
        sleep
            .as_mut()
            .poll(&mut Context::from_waker(&clone_callback_waker())),
        Poll::Ready(())
    );
    assert!(sleep.is_elapsed());
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn cloning_a_waker_can_cancel_reuse_and_grow_other_slots() {
    let epoch = Instant::now();
    let timer = Rc::new(ManualTimer::with_capacity(epoch, 1));
    let mut sibling = Box::pin(timer.with(|| sleep_until(epoch + Duration::from_millis(1))));
    assert!(poll(sibling.as_mut()).is_pending());
    let mut first = Box::pin(timer.with(|| sleep_until(epoch + Duration::from_millis(2))));
    let created = Rc::new(RefCell::new(Vec::new()));
    let callback_timer = timer.clone();
    let callback_created = created.clone();
    let grew = Rc::new(Cell::new(false));
    let callback_grew = grew.clone();
    ON_CLONE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            drop(sibling);
            let capacity = callback_timer.capacity();
            for _ in 0..capacity + 1 {
                let mut sleep =
                    Box::pin(callback_timer.with(|| sleep_until(epoch + Duration::from_millis(3))));
                assert!(poll(sleep.as_mut()).is_pending());
                callback_created.borrow_mut().push(sleep);
            }
            callback_grew.set(callback_timer.capacity() > capacity);
        }))
    });
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&clone_callback_waker()))
            .is_pending()
    );
    assert!(grew.get());
    timer.advance(epoch + Duration::from_millis(1));
    assert!(poll(first.as_mut()).is_pending());
    timer.advance(epoch + Duration::from_millis(2));
    assert!(poll(first.as_mut()).is_ready());
    for sleep in created.borrow_mut().iter_mut() {
        assert!(poll(sleep.as_mut()).is_pending());
    }
    timer.advance(epoch + Duration::from_millis(3));
    for sleep in created.borrow_mut().iter_mut() {
        assert!(poll(sleep.as_mut()).is_ready());
    }
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn a_panicking_clone_leaves_the_registered_entry_retryable() {
    let epoch = Instant::now();
    let timer = ManualTimer::with_capacity(epoch, 1);
    let deadline = epoch + Duration::from_millis(1);
    let mut sleep = Box::pin(timer.with(|| sleep_until(deadline)));
    ON_CLONE.with(|slot| *slot.borrow_mut() = Some(Box::new(|| panic!("clone callback"))));
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            sleep
                .as_mut()
                .poll(&mut Context::from_waker(&clone_callback_waker()))
        }))
        .is_err()
    );
    assert!(poll(sleep.as_mut()).is_pending());
    timer.advance(deadline);
    assert!(poll(sleep.as_mut()).is_ready());
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn dropping_a_ready_polls_waker_can_reuse_its_just_removed_slot() {
    thread_local! {
        static ON_DROP: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }
    struct DropWake;
    #[allow(clippy::manual_noop_waker)] // The final Arc's destructor is observable.
    impl Wake for DropWake {
        fn wake(self: Arc<Self>) {}
    }
    impl Drop for DropWake {
        fn drop(&mut self) {
            if let Some(callback) = ON_DROP.with(|slot| slot.take()) {
                callback();
            }
        }
    }
    let epoch = Instant::now();
    let timer = Rc::new(ManualTimer::with_capacity(epoch, 1));
    let mut elapsed = Box::pin(timer.with(|| sleep_until(epoch)));
    let waker = Waker::from(Arc::new(DropWake));
    assert!(
        elapsed
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_ready()
    );
    drop(waker);
    let replacement = Rc::new(RefCell::new(None));
    let callback_timer = timer.clone();
    let callback_replacement = replacement.clone();
    ON_DROP.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            let mut sleep =
                Box::pin(callback_timer.with(|| sleep_until(epoch + Duration::from_millis(1))));
            assert!(poll(sleep.as_mut()).is_pending());
            *callback_replacement.borrow_mut() = Some(sleep);
        }))
    });
    drop(elapsed);
    assert!(replacement.borrow().is_some());
    timer.advance(epoch + Duration::from_millis(1));
    assert!(poll(replacement.borrow_mut().as_mut().unwrap().as_mut()).is_ready());
    assert_eq!(timer.next_wake(), None);
}
