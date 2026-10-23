//! One-shot retained-heap probe for Valgrind DHAT; see benches/README.md.
//! Intentionally forget the selected phase's owners so DHAT's end-of-process
//! heap records a snapshot of that live state, rather than normal teardown.
#![forbid(unsafe_code)]

use std::future::Future;
use std::hint::black_box;
use std::task::{Context, Waker};

use shep_smp_time::{Duration, Instant, Timer, sleep_until};

fn main() {
    let phase = std::env::args().nth(1).unwrap_or_else(|| "freed".into());
    let count: usize = std::env::args()
        .nth(2)
        .map(|arg| arg.parse().unwrap())
        .unwrap_or(32768);
    let capacity: usize = std::env::args()
        .nth(3)
        .map(|arg| arg.parse().unwrap())
        .unwrap_or(1024);
    assert!(["empty", "live", "expired", "freed"].contains(&phase.as_str()));
    let epoch = Instant::now();
    let timer = Timer::with_capacity(epoch, capacity);
    let deadline = epoch + Duration::from_millis(100);
    if phase != "empty" {
        let mut sleeps = Vec::with_capacity(count);
        let mut cx = Context::from_waker(Waker::noop());
        timer.with(|| {
            for _ in 0..count {
                let mut sleep = Box::pin(sleep_until(deadline));
                assert!(sleep.as_mut().poll(&mut cx).is_pending());
                sleeps.push(sleep);
            }
        });
        if phase == "expired" {
            timer.advance(deadline);
            for sleep in &mut sleeps {
                assert!(sleep.as_mut().poll(&mut cx).is_ready());
            }
        }
        if phase == "freed" {
            drop(sleeps);
            assert_eq!(timer.next_wake(), None);
        } else {
            std::mem::forget(black_box(sleeps));
        }
    }
    println!(
        "phase={phase}, consumers={count}, reserved_slots={}",
        timer.capacity()
    );
    std::mem::forget(black_box(timer));
}
