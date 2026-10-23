use std::cell::RefCell;
use std::future::{Future, pending};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use shep_futures::future::block_on;
use shep_smp_exec::{Executor, JoinHandle};
use shep_smp_time::{
    Duration, Instant, MissedTickBehavior, Timer, interval_at, sleep, sleep_until, timeout_at,
};

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

// The reactor will own these steps. Tests advance directly to the next wheel
// processing time, with no driver abstraction, clock trait, or wall-clock wait.
fn drive<T>(timer: &Timer, exec: &mut Executor, join: JoinHandle<T>) -> T {
    for _ in 0..256 {
        timer.with(|| exec.run_ready(64));
        if join.is_finished() {
            return block_on(join).unwrap();
        }
        if !exec.has_ready() {
            timer.advance(timer.next_wake().expect("pending task has no wake source"));
        }
    }
    panic!("timer task did not finish");
}

#[test]
fn advancing_timers_wakes_local_executor_tasks_without_waiting() {
    let now = Instant::now();
    let timer = Timer::new(now);
    let mut exec = Executor::new(Waker::noop().clone());
    let local = Rc::new(42);
    let join = exec.spawn(async move {
        sleep_until(now + Duration::from_secs(1)).await;
        local
    });
    assert_eq!(*drive(&timer, &mut exec, join), 42);
    assert!(exec.is_empty());
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn a_sleep_remains_bound_to_its_creation_timer() {
    let now = Instant::now();
    let owner = Timer::new(now);
    let other = Timer::new(now);
    let deadline = now + Duration::from_millis(10);
    let mut sleep = Box::pin(owner.with(|| sleep_until(deadline)));
    other.with(|| assert!(poll(sleep.as_mut()).is_pending()));
    assert_eq!(owner.next_wake(), Some(deadline));
    assert_eq!(other.next_wake(), None);
    other.advance(deadline);
    assert!(poll(sleep.as_mut()).is_pending());
    owner.advance(deadline);
    assert!(poll(sleep.as_mut()).is_ready());
    assert_eq!(owner.next_wake(), None);
}

#[test]
fn construction_is_lazy_and_dropping_a_registered_sleep_unlinks_it() {
    let now = Instant::now();
    let timer = Timer::new(now);
    let mut sleep = Box::pin(timer.with(|| sleep_until(now + Duration::from_secs(30))));
    assert_eq!(timer.next_wake(), None);
    timer.with(|| assert!(poll(sleep.as_mut()).is_pending()));
    assert!(timer.next_wake().is_some());
    drop(sleep);
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn resetting_later_preserves_the_extended_deadline() {
    let now = Instant::now();
    let timer = Timer::new(now);
    let mut sleep = Box::pin(timer.with(|| sleep_until(now + Duration::from_millis(2))));
    assert!(poll(sleep.as_mut()).is_pending());
    let extended = now + Duration::from_millis(8);
    sleep.as_mut().reset(extended);
    timer.advance(now + Duration::from_millis(2));
    assert!(poll(sleep.as_mut()).is_pending());
    assert_eq!(timer.next_wake(), Some(extended));
    timer.advance(extended);
    assert!(poll(sleep.as_mut()).is_ready());
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn reset_can_shorten_a_registered_timer_and_reuse_an_elapsed_one() {
    let now = Instant::now();
    let timer = Timer::new(now);
    let mut sleep = Box::pin(timer.with(|| sleep_until(now + Duration::from_secs(30))));
    assert!(poll(sleep.as_mut()).is_pending());
    let earlier = now + Duration::from_millis(2);
    sleep.as_mut().reset(earlier);
    assert_eq!(timer.next_wake(), Some(earlier));
    timer.advance(earlier);
    assert!(poll(sleep.as_mut()).is_ready());
    assert!(sleep.is_elapsed());
    let again = now + Duration::from_millis(4);
    sleep.as_mut().reset(again);
    timer.advance(now + Duration::from_millis(3));
    assert!(poll(sleep.as_mut()).is_pending());
    timer.advance(again);
    assert!(poll(sleep.as_mut()).is_ready());
}

#[test]
fn timeout_expires_and_a_ready_value_wins_even_with_a_past_deadline() {
    let now = Instant::now();
    let timer = Timer::new(now);
    let deadline = now + Duration::from_millis(5);
    let mut timeout = Box::pin(timer.with(|| timeout_at(deadline, pending::<()>())));
    assert!(poll(timeout.as_mut()).is_pending());
    timer.advance(deadline);
    assert!(matches!(poll(timeout.as_mut()), Poll::Ready(Err(_))));
    let mut ready = Box::pin(timer.with(|| timeout_at(now, async { 42 })));
    assert_eq!(poll(ready.as_mut()), Poll::Ready(Ok(42)));
}

#[test]
fn interval_preserves_burst_catchup_and_missed_tick_configuration() {
    let now = Instant::now();
    let timer = Timer::new(now);
    let start = now - Duration::from_secs(10);
    let mut interval = timer.with(|| interval_at(start, Duration::from_secs(1)));
    assert_eq!(interval.missed_tick_behavior(), MissedTickBehavior::Burst);
    assert_eq!(timer.with(|| block_on(interval.tick())), start);
    assert_eq!(
        timer.with(|| block_on(interval.tick())),
        start + Duration::from_secs(1)
    );
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    assert_eq!(interval.missed_tick_behavior(), MissedTickBehavior::Skip);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    assert_eq!(interval.missed_tick_behavior(), MissedTickBehavior::Delay);
}

#[test]
fn next_wake_includes_coarse_bucket_processing_before_the_deadline() {
    let now = Instant::now();
    let timer = Timer::new(now);
    let deadline = now + Duration::from_millis(100);
    let mut sleep = Box::pin(timer.with(|| sleep_until(deadline)));
    assert!(poll(sleep.as_mut()).is_pending());
    assert_eq!(timer.next_wake(), Some(now + Duration::from_millis(64)));
    timer.advance(now + Duration::from_millis(64));
    assert!(poll(sleep.as_mut()).is_pending());
    assert_eq!(timer.next_wake(), Some(deadline));
    timer.advance(now + Duration::from_millis(99));
    assert!(poll(sleep.as_mut()).is_pending());
    timer.advance(deadline);
    assert!(poll(sleep.as_mut()).is_ready());
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn fractional_millisecond_deadlines_round_up_without_firing_early() {
    let now = Instant::now();
    let timer = Timer::new(now);
    let mut sleep = Box::pin(timer.with(|| sleep_until(now + Duration::from_micros(100))));
    assert!(poll(sleep.as_mut()).is_pending());
    let tick = now + Duration::from_millis(1);
    assert_eq!(timer.next_wake(), Some(tick));
    timer.advance(now + Duration::from_nanos(999_999));
    assert!(poll(sleep.as_mut()).is_pending());
    timer.advance(tick);
    assert!(poll(sleep.as_mut()).is_ready());
}

#[test]
fn advancing_backwards_is_clamped_and_cannot_fire_a_future_timer() {
    let now = Instant::now();
    let timer = Timer::new(now);
    timer.advance(now + Duration::from_secs(1));
    let deadline = now + Duration::from_millis(1001);
    let mut sleep = Box::pin(timer.with(|| sleep_until(deadline)));
    assert!(poll(sleep.as_mut()).is_pending());
    timer.advance(now + Duration::from_millis(500));
    assert!(poll(sleep.as_mut()).is_pending());
    assert_eq!(timer.next_wake(), Some(deadline));
    timer.advance(deadline);
    assert!(poll(sleep.as_mut()).is_ready());
}

#[test]
fn waking_a_timer_can_drop_another_timer_without_borrow_conflicts() {
    // Wakers are Send + Sync; callbacks are taken only from the calling
    // thread's TLS. Waking from another thread finds no callback there.
    thread_local! {
        static ON_WAKE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }
    struct WakeLocal;
    impl Wake for WakeLocal {
        fn wake(self: Arc<Self>) {
            if let Some(callback) = ON_WAKE.with(|slot| slot.take()) {
                callback();
            }
        }
    }
    let now = Instant::now();
    let timer = Timer::new(now);
    let deadline = now + Duration::from_millis(1);
    let mut first = Box::pin(timer.with(|| sleep_until(deadline)));
    let waker = Waker::from(Arc::new(WakeLocal));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let mut sibling = Box::pin(timer.with(|| sleep_until(deadline)));
    assert!(poll(sibling.as_mut()).is_pending());
    ON_WAKE.with(|slot| *slot.borrow_mut() = Some(Box::new(move || drop(sibling))));
    timer.advance(deadline);
    assert!(poll(first.as_mut()).is_ready());
    assert_eq!(timer.next_wake(), None);
    assert!(ON_WAKE.with(|slot| slot.borrow().is_none()));
}

#[test]
fn cancelling_a_timer_can_drop_another_sleep_through_its_waker_destructor() {
    // Dropping the final Arc behind a standard Waker can run arbitrary user
    // destruction. The callback stays local rather than making sleeps Send.
    thread_local! {
        static ON_DROP: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }
    struct DropWake;
    // Waker::noop has no final-Arc destructor, which is the behavior under test.
    #[allow(clippy::manual_noop_waker)]
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

    let now = Instant::now();
    let timer = Timer::new(now);
    let deadline = now + Duration::from_millis(1);
    let mut first = Box::pin(timer.with(|| sleep_until(deadline)));
    let waker = Waker::from(Arc::new(DropWake));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);

    let mut sibling = Box::pin(timer.with(|| sleep_until(deadline)));
    assert!(poll(sibling.as_mut()).is_pending());
    ON_DROP.with(|slot| *slot.borrow_mut() = Some(Box::new(move || drop(sibling))));

    // The old implementation dropped the waker while borrowing the wheel.
    // This reentrant cancellation then panicked and freed a still-linked node.
    drop(first);
    assert!(ON_DROP.with(|slot| slot.borrow().is_none()));
    assert_eq!(timer.next_wake(), None);
    timer.advance(deadline);
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn a_panicking_waker_destructor_leaves_other_timer_entries_valid() {
    struct PanickingDropWake;
    // The destructor is intentionally observable even though wake does no work.
    #[allow(clippy::manual_noop_waker)]
    impl Wake for PanickingDropWake {
        fn wake(self: Arc<Self>) {}
    }
    impl Drop for PanickingDropWake {
        fn drop(&mut self) {
            panic!("waker destructor");
        }
    }
    let now = Instant::now();
    let timer = Timer::new(now);
    let deadline = now + Duration::from_millis(1);
    let mut first = Box::pin(timer.with(|| sleep_until(deadline)));
    let waker = Waker::from(Arc::new(PanickingDropWake));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);
    let mut sibling = Box::pin(timer.with(|| sleep_until(deadline)));
    assert!(poll(sibling.as_mut()).is_pending());
    assert!(catch_unwind(AssertUnwindSafe(|| drop(first))).is_err());
    timer.advance(deadline);
    assert!(poll(sibling.as_mut()).is_ready());
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn timer_context_is_restored_after_nested_scopes_and_unwind() {
    let now = Instant::now();
    let outer = Timer::new(now);
    let inner = Timer::new(now);
    outer.with(|| {
        assert!(catch_unwind(AssertUnwindSafe(|| inner.with(|| panic!("unwind")))).is_err());
        let mut sleep = Box::pin(sleep(Duration::from_secs(30)));
        assert!(poll(sleep.as_mut()).is_pending());
        assert!(outer.next_wake().is_some());
        assert_eq!(inner.next_wake(), None);
    });
    assert_eq!(outer.next_wake(), None);
    assert!(catch_unwind(|| sleep(Duration::ZERO)).is_err());
}

#[test]
fn a_wake_callback_can_reset_a_sibling_already_queued_for_firing() {
    thread_local! {
        static ON_WAKE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }
    struct WakeLocal;
    impl Wake for WakeLocal {
        fn wake(self: Arc<Self>) {
            if let Some(callback) = ON_WAKE.with(|slot| slot.take()) {
                callback();
            }
        }
    }

    for reset_to in [0, 5] {
        let now = Instant::now();
        let timer = Rc::new(Timer::new(now));
        let deadline = now + Duration::from_millis(1);
        let mut first = Box::pin(timer.with(|| sleep_until(deadline)));
        let waker = Waker::from(Arc::new(WakeLocal));
        assert!(
            first
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        let sibling = Rc::new(RefCell::new(Box::pin(timer.with(|| sleep_until(deadline)))));
        assert!(poll(sibling.borrow_mut().as_mut()).is_pending());
        let callback_timer = timer.clone();
        let callback_sibling = sibling.clone();
        ON_WAKE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                // Both deadlines have been detached from their bucket. The
                // sibling remains in the pending queue during this callback.
                assert_eq!(callback_timer.next_wake(), Some(deadline));
                callback_sibling
                    .borrow_mut()
                    .as_mut()
                    .reset(now + Duration::from_millis(reset_to));
            }));
        });
        timer.advance(deadline);
        assert!(poll(first.as_mut()).is_ready());
        assert_eq!(
            poll(sibling.borrow_mut().as_mut()).is_ready(),
            reset_to == 0
        );
        if reset_to > 0 {
            assert_eq!(
                timer.next_wake(),
                Some(now + Duration::from_millis(reset_to))
            );
            timer.advance(now + Duration::from_millis(reset_to));
            assert!(poll(sibling.borrow_mut().as_mut()).is_ready());
        }
        assert_eq!(timer.next_wake(), None);
    }
}
