use std::cell::Cell;
use std::future::{Future, pending, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use shep_smp2::time::{
    Duration, Instant, MissedTickBehavior, Timer, interval, interval_at, sleep, sleep_until,
    timeout, timeout_at,
};

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn relative_sleep_completes_after_200_milliseconds() {
    // Port of Monoio's runtime::tests::timer at the revision in docs/time/FORK.md.
    // Replace io_uring waiting and wall-clock tolerance with explicit advancement.
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let mut delay = Box::pin(timer.with(|| sleep(Duration::from_millis(200))));
    let deadline = delay.deadline();
    assert!(deadline >= epoch + Duration::from_millis(200));
    assert!(poll(delay.as_mut()).is_pending());
    timer.advance(deadline - Duration::from_nanos(1));
    assert!(poll(delay.as_mut()).is_pending());
    timer.advance(deadline + Duration::from_millis(1));
    assert!(poll(delay.as_mut()).is_ready());
    assert_eq!(timer.next_wake(), None);
}

struct DropFlag(Rc<Cell<bool>>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

#[test]
fn timeout_cancels_and_drops_the_pending_local_operation() {
    // Adapt Monoio's tests/tcp_connect.rs::connect_timeout_dst: the timeout
    // must release the losing operation. Use a local pending future in place
    // of an actual TCP connection to keep the test timer-only and deterministic.
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let dropped = Rc::new(Cell::new(false));
    let flag = DropFlag(dropped.clone());
    let operation = async move {
        let _flag = flag;
        pending::<()>().await;
    };
    let deadline = epoch + Duration::from_millis(10);
    let mut timeout = Box::pin(timer.with(|| timeout_at(deadline, operation)));
    assert!(poll(timeout.as_mut()).is_pending());
    timer.advance(deadline);
    assert!(matches!(poll(timeout.as_mut()), Poll::Ready(Err(_))));
    drop(timeout);
    assert!(dropped.get());
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn completing_the_operation_cancels_its_registered_timeout() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let ready = Rc::new(Cell::new(false));
    let operation = poll_fn(|_| {
        if ready.get() {
            Poll::Ready(42)
        } else {
            Poll::Pending
        }
    });
    let mut timeout =
        Box::pin(timer.with(|| timeout_at(epoch + Duration::from_secs(1), operation)));
    assert!(poll(timeout.as_mut()).is_pending());
    assert!(timer.next_wake().is_some());
    ready.set(true);
    assert_eq!(poll(timeout.as_mut()), Poll::Ready(Ok(42)));
    drop(timeout);
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn recovering_the_inner_future_cancels_an_unpolled_timeout() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let mut wrapped = timer.with(|| timeout(Duration::from_secs(1), std::future::ready(1)));
    assert_eq!(wrapped.get_ref().clone().into_inner(), 1);
    *wrapped.get_mut() = std::future::ready(42);
    let mut inner = wrapped.into_inner();
    assert_eq!(poll(Pin::new(&mut inner)), Poll::Ready(42));
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn dropping_a_pending_timeout_releases_its_operation_and_timer() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let dropped = Rc::new(Cell::new(false));
    let flag = DropFlag(dropped.clone());
    let mut wrapped = Box::pin(timer.with(|| {
        timeout(Duration::from_secs(1), async move {
            let _flag = flag;
            pending::<()>().await;
        })
    }));
    assert!(poll(wrapped.as_mut()).is_pending());
    drop(wrapped);
    assert!(dropped.get());
    assert_eq!(timer.next_wake(), None);
    timer.advance(epoch + Duration::from_secs(2));
}

#[test]
fn elapsed_timeout_converts_to_a_timed_out_io_error() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let mut wrapped = pin!(timer.with(|| timeout_at(epoch, pending::<()>())));
    let Poll::Ready(Err(elapsed)) = poll(wrapped.as_mut()) else {
        panic!("a deadline at the timer epoch must already be elapsed");
    };
    assert_eq!(elapsed.to_string(), "deadline has elapsed");
    let error: std::io::Error = elapsed.into();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn overflowing_relative_durations_use_far_future_timers() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let mut delay = Box::pin(timer.with(|| sleep(Duration::MAX)));
    let mut wrapped = Box::pin(timer.with(|| timeout(Duration::MAX, pending::<()>())));
    assert!(poll(delay.as_mut()).is_pending());
    assert!(poll(wrapped.as_mut()).is_pending());
    timer.advance(epoch + Duration::from_secs(365 * 86400));
    assert!(poll(delay.as_mut()).is_pending());
    assert!(poll(wrapped.as_mut()).is_pending());
    // Reach the fallback (~30 years), traversing the top level repeatedly.
    timer.advance(epoch + Duration::from_secs(31 * 365 * 86400));
    assert!(poll(delay.as_mut()).is_ready());
    assert!(matches!(poll(wrapped.as_mut()), Poll::Ready(Err(_))));
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn sleeps_before_the_epoch_are_ready_on_first_poll() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    for deadline in [epoch - Duration::from_secs(1), epoch] {
        let mut delay = pin!(timer.with(|| sleep_until(deadline)));
        assert!(poll(delay.as_mut()).is_ready());
        assert!(delay.is_elapsed());
    }
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn registered_sleeps_can_be_safely_dropped_after_the_owner() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let mut delays: Vec<_> = [1, 64, 4096, 262_144, 16_777_216, 1_073_741_824]
        .map(|ms| Box::pin(timer.with(|| sleep_until(epoch + Duration::from_millis(ms)))))
        .into();
    for delay in &mut delays {
        assert!(poll(delay.as_mut()).is_pending());
    }
    drop(timer);
    // Consumers retain the wheel state until they have unlinked their entries.
    drop(delays);
}

#[test]
fn burst_replays_each_missed_tick_before_waiting_again() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let period = Duration::from_secs(86400);
    let start = epoch - period * 2 - Duration::from_secs(6 * 3600);
    let mut interval = timer.with(|| interval_at(start, period));
    let mut cx = Context::from_waker(Waker::noop());
    for index in 0..3 {
        assert_eq!(
            interval.poll_tick(&mut cx),
            Poll::Ready(start + period * index)
        );
    }
    assert!(interval.poll_tick(&mut cx).is_pending());
    let next = start + period * 3;
    timer.advance(next);
    assert_eq!(interval.poll_tick(&mut cx), Poll::Ready(next));
    assert_eq!(interval.period(), period);
    drop(interval);
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn skip_keeps_the_original_schedule_without_replaying_missed_ticks() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let period = Duration::from_secs(86400);
    let start = epoch - period * 2 - Duration::from_secs(6 * 3600);
    let mut interval = timer.with(|| interval_at(start, period));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(interval.poll_tick(&mut cx), Poll::Ready(start));
    assert!(interval.poll_tick(&mut cx).is_pending());
    let next = start + period * 3;
    timer.advance(next - Duration::from_millis(1));
    assert!(interval.poll_tick(&mut cx).is_pending());
    timer.advance(next);
    assert_eq!(interval.poll_tick(&mut cx), Poll::Ready(next));
}

#[test]
fn delay_schedules_a_full_period_after_observing_a_missed_tick() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let period = Duration::from_secs(86400);
    let start = epoch - period * 2;
    let mut interval = timer.with(|| interval_at(start, period));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut cx = Context::from_waker(Waker::noop());
    let before = Instant::now();
    assert_eq!(interval.poll_tick(&mut cx), Poll::Ready(start));
    let after = Instant::now();
    assert!(interval.poll_tick(&mut cx).is_pending());
    timer.advance(before + period - Duration::from_millis(1));
    assert!(interval.poll_tick(&mut cx).is_pending());
    timer.advance(after + period + Duration::from_millis(1));
    let Poll::Ready(next) = interval.poll_tick(&mut cx) else {
        panic!("next tick should have elapsed");
    };
    // Interval policy reads the real monotonic clock. Bracket the observation
    // rather than sleeping or assuming advancement changes Instant::now().
    assert!(next >= before + period);
    assert!(next <= after + period);
}

#[test]
fn dropping_a_pending_tick_future_does_not_consume_the_tick() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let first = epoch + Duration::from_secs(86400);
    let period = Duration::from_secs(1);
    let mut interval = timer.with(|| interval_at(first, period));
    {
        let mut tick = pin!(interval.tick());
        assert!(poll(tick.as_mut()).is_pending());
    }
    timer.advance(first);
    {
        let mut tick = pin!(interval.tick());
        assert_eq!(poll(tick.as_mut()), Poll::Ready(first));
    }
    assert!(
        interval
            .poll_tick(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    timer.advance(first + period);
    assert_eq!(
        interval.poll_tick(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(first + period)
    );
    drop(interval);
    assert_eq!(timer.next_wake(), None);
}

#[test]
fn relative_intervals_start_immediately_and_reject_zero_periods() {
    let epoch = Instant::now();
    let timer = Timer::new(epoch);
    let before = Instant::now();
    let mut interval = timer.with(|| interval(Duration::from_secs(1)));
    let after = Instant::now();
    timer.advance(after + Duration::from_millis(1));
    let Poll::Ready(first) = interval.poll_tick(&mut Context::from_waker(Waker::noop())) else {
        panic!("the first relative interval tick should be ready");
    };
    assert!(first >= before && first <= after);
    assert!(
        catch_unwind(AssertUnwindSafe(
            || timer.with(|| shep_smp2::time::interval(Duration::ZERO))
        ))
        .is_err()
    );
    assert!(
        catch_unwind(AssertUnwindSafe(
            || timer.with(|| interval_at(epoch, Duration::ZERO))
        ))
        .is_err()
    );
}

#[test]
fn standard_clock_instants_support_reactor_deadline_arithmetic() {
    let standard = std::time::Instant::now() - Duration::from_secs(5);
    let epoch = Instant::from_std(standard);
    assert_eq!(epoch.into_std(), standard);
    assert_eq!(Instant::from(standard), epoch);
    assert_eq!(std::time::Instant::from(epoch), standard);
    let duration = Duration::from_millis(125);
    let mut deadline = epoch;
    deadline += duration;
    assert_eq!(deadline.duration_since(epoch), duration);
    assert_eq!(deadline.checked_duration_since(epoch), Some(duration));
    assert_eq!(epoch.checked_duration_since(deadline), None);
    assert_eq!(epoch.saturating_duration_since(deadline), Duration::ZERO);
    assert_eq!(deadline.saturating_duration_since(epoch), duration);
    assert_eq!(epoch.checked_add(duration), Some(deadline));
    assert_eq!(deadline.checked_sub(duration), Some(epoch));
    assert_eq!(epoch.checked_add(Duration::MAX), None);
    assert_eq!(epoch.checked_sub(Duration::MAX), None);
    deadline -= duration;
    assert_eq!(deadline, epoch);
    assert!(epoch.elapsed() >= Duration::from_secs(5));

    let timer = Timer::new(epoch);
    let mut delay = pin!(timer.with(|| sleep_until(epoch + duration)));
    assert!(poll(delay.as_mut()).is_pending());
    timer.advance(Instant::from_std(standard + duration));
    assert!(poll(delay.as_mut()).is_ready());
}
