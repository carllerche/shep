use std::cell::RefCell;
use std::future::{Future, pending};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use shep_futures::future::block_on;
use shep_smp_exec::{Executor, JoinHandle};
use shep_smp_time::{
    Driver, Duration, Instant, MissedTickBehavior, TimeDriver, interval_at, sleep, sleep_until,
    timeout, timeout_at,
};

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Submit,
    Park,
    Timeout(Duration),
}

/// Test-only waiting driver. Production waiting will come from io_uring.
struct Wait {
    calls: Rc<RefCell<Vec<Call>>>,
    wait: bool,
}

impl Driver for Wait {
    fn with<R>(&self, f: impl FnOnce() -> R) -> R {
        f()
    }
    fn submit(&self) -> std::io::Result<()> {
        self.calls.borrow_mut().push(Call::Submit);
        Ok(())
    }
    fn park(&self) -> std::io::Result<()> {
        self.calls.borrow_mut().push(Call::Park);
        Ok(())
    }
    fn park_timeout(&self, duration: Duration) -> std::io::Result<()> {
        self.calls.borrow_mut().push(Call::Timeout(duration));
        if self.wait {
            std::thread::sleep(duration);
        }
        Ok(())
    }
}

fn driver(wait: bool) -> (TimeDriver<Wait>, Rc<RefCell<Vec<Call>>>) {
    let calls = Rc::new(RefCell::new(Vec::new()));
    (
        TimeDriver::new(Wait {
            calls: calls.clone(),
            wait,
        }),
        calls,
    )
}

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

fn drive<T>(timer: &TimeDriver<Wait>, exec: &mut Executor, join: JoinHandle<T>) -> T {
    let end = std::time::Instant::now() + Duration::from_secs(2);
    while !join.is_finished() {
        assert!(std::time::Instant::now() < end, "timer failed to complete");
        timer.with(|| exec.run_ready(64));
        if !join.is_finished() && !exec.has_ready() {
            timer.park().unwrap();
        }
    }
    block_on(join).unwrap()
}

#[test]
fn sleep_wakes_local_executor_tasks_without_tokio() {
    let (timer, _) = driver(true);
    let mut exec = Executor::new(Waker::noop().clone());
    let local = Rc::new(42);
    let deadline = Instant::now() + Duration::from_millis(3);
    let join = exec.spawn(async move {
        sleep_until(deadline).await;
        assert!(Instant::now() >= deadline);
        local
    });
    assert_eq!(*drive(&timer, &mut exec, join), 42);
    assert!(exec.is_empty());
}

#[test]
fn a_sleep_remains_bound_to_its_creation_driver() {
    let (owner, owner_calls) = driver(false);
    let (other, other_calls) = driver(false);
    let mut sleep = Box::pin(owner.with(|| sleep(Duration::from_secs(30))));
    other.with(|| assert!(poll(sleep.as_mut()).is_pending()));
    owner.park_timeout(Duration::ZERO).unwrap();
    other.park().unwrap();
    assert_eq!(*owner_calls.borrow(), [Call::Timeout(Duration::ZERO)]);
    assert_eq!(*other_calls.borrow(), [Call::Park]);
    drop(sleep);
    owner.park().unwrap();
    assert_eq!(owner_calls.borrow().last(), Some(&Call::Park));
}

#[test]
fn construction_is_lazy_and_dropping_a_registered_sleep_unlinks_it() {
    let (timer, calls) = driver(false);
    let mut sleep = Box::pin(timer.with(|| sleep(Duration::from_secs(30))));
    timer.park().unwrap();
    assert_eq!(*calls.borrow(), [Call::Park]);
    timer.with(|| assert!(poll(sleep.as_mut()).is_pending()));
    timer.park_timeout(Duration::ZERO).unwrap();
    assert_eq!(calls.borrow().last(), Some(&Call::Timeout(Duration::ZERO)));
    drop(sleep);
    timer.park().unwrap();
    assert_eq!(calls.borrow().last(), Some(&Call::Park));
}

#[test]
fn resetting_later_preserves_the_extended_deadline() {
    let (timer, _) = driver(true);
    let mut exec = Executor::new(Waker::noop().clone());
    let join = exec.spawn(async {
        let mut sleep = Box::pin(sleep(Duration::from_millis(2)));
        std::future::poll_fn(|cx| {
            assert!(sleep.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let extended = Instant::now() + Duration::from_millis(8);
        sleep.as_mut().reset(extended);
        sleep.await;
        assert!(Instant::now() >= extended);
    });
    drive(&timer, &mut exec, join);
}

#[test]
fn reset_can_shorten_a_registered_timer_and_reuse_an_elapsed_one() {
    let (timer, _) = driver(true);
    let mut exec = Executor::new(Waker::noop().clone());
    let join = exec.spawn(async {
        let mut sleep = Box::pin(sleep(Duration::from_secs(30)));
        std::future::poll_fn(|cx| {
            assert!(sleep.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let earlier = Instant::now() + Duration::from_millis(2);
        sleep.as_mut().reset(earlier);
        sleep.as_mut().await;
        assert!(Instant::now() >= earlier);
        assert!(sleep.is_elapsed());
        let again = Instant::now() + Duration::from_millis(2);
        sleep.as_mut().reset(again);
        sleep.await;
        assert!(Instant::now() >= again);
    });
    drive(&timer, &mut exec, join);
}

#[test]
fn timeout_expires_and_a_ready_value_wins_even_with_a_past_deadline() {
    let (timer, _) = driver(true);
    let mut exec = Executor::new(Waker::noop().clone());
    let join = exec.spawn(async {
        assert!(
            timeout(Duration::from_millis(2), pending::<()>())
                .await
                .is_err()
        );
        let past = Instant::now() - Duration::from_secs(1);
        assert_eq!(timeout_at(past, async { 42 }).await, Ok(42));
    });
    drive(&timer, &mut exec, join);
}

#[test]
fn interval_preserves_burst_catchup_and_missed_tick_configuration() {
    let (timer, _) = driver(false);
    let start = Instant::now() - Duration::from_secs(10);
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
fn driver_submit_and_wait_limits_are_forwarded() {
    let (timer, calls) = driver(false);
    timer.submit().unwrap();
    timer.park().unwrap();
    timer.park_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        *calls.borrow(),
        [
            Call::Submit,
            Call::Park,
            Call::Timeout(Duration::from_secs(1))
        ]
    );
    let mut sleep = Box::pin(timer.with(|| sleep(Duration::from_secs(30))));
    timer.with(|| assert!(poll(sleep.as_mut()).is_pending()));
    timer.park_timeout(Duration::from_millis(1)).unwrap();
    assert_eq!(
        calls.borrow().last(),
        Some(&Call::Timeout(Duration::from_millis(1)))
    );
}

#[test]
fn timer_context_is_restored_after_nested_scopes_and_unwind() {
    let (outer, calls) = driver(false);
    let (inner, _) = driver(false);
    outer.with(|| {
        assert!(catch_unwind(AssertUnwindSafe(|| inner.with(|| panic!("unwind")))).is_err());
        let mut sleep = Box::pin(sleep(Duration::from_secs(30)));
        assert!(poll(sleep.as_mut()).is_pending());
        outer.park_timeout(Duration::ZERO).unwrap();
    });
    assert_eq!(*calls.borrow(), [Call::Timeout(Duration::ZERO)]);
    assert!(catch_unwind(|| sleep(Duration::ZERO)).is_err());
}
