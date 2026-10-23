//! Local timers extracted from Monoio's `time` module.
//!
//! [`Timer`] owns Monoio's six-level hashed timing wheel. The reactor selects
//! it with [`Timer::with`], advances it with [`Timer::advance`], and uses
//! [`Timer::next_wake`] to bound its own I/O wait. [`sleep`], [`sleep_until`],
//! [`timeout`], [`timeout_at`], and [`interval`] use the selected timer.
//! Timers and their owner are `!Send`.
//!
//! This extraction preserves upstream's millisecond resolution and lazy
//! registration and reset. The wheel owns entries in a safe indexed slab.
//! The reactor owns execution,
//! waiting, and timer advancement; this crate supplies the timer functions.
//! See `FORK.md` for the exact source revision and adaptation details.
//!
//! ```
//! use shep_smp_time::{Duration, Instant, Timer, sleep_until};
//! use std::task::{Context, Waker};
//! use std::future::Future;
//!
//! let now = Instant::now();
//! let timer = Timer::new(now);
//! let deadline = now + Duration::from_millis(10);
//! let mut sleep = Box::pin(timer.with(|| sleep_until(deadline)));
//! let mut cx = Context::from_waker(Waker::noop());
//! assert!(sleep.as_mut().poll(&mut cx).is_pending());
//! assert_eq!(timer.next_wake(), Some(deadline));
//! timer.advance(deadline);
//! assert!(sleep.as_mut().poll(&mut cx).is_ready());
//! ```
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp_time::Timer>();
//! ```
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<shep_smp_time::Timer>();
//! ```
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp_time::Sleep>();
//! ```
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<shep_smp_time::Sleep>();
//! ```
// Adapted from Monoio commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

#![forbid(unsafe_code)]
// Retain the upstream wheel's formatting and control flow.
#![allow(clippy::collapsible_if)]

mod timer;
pub use timer::{
    Timer,
    sleep::{Sleep, sleep, sleep_until},
};

pub mod error;

mod instant;
pub use instant::Instant;

mod interval;
pub use interval::{Interval, MissedTickBehavior, interval, interval_at};

mod timeout;
pub use timeout::{Timeout, timeout, timeout_at};

pub use std::time::Duration;
