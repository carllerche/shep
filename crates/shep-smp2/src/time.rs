//! Local timers extracted from Monoio's `time` module.
//!
//! Each runtime owns a timer with Monoio's six-level hashed timing wheel. It
//! advances the wheel between task turns and bounds its I/O wait by the next
//! deadline. [`sleep`], [`sleep_until`], [`timeout`], [`timeout_at`], and
//! [`interval`] register with the timer of the runtime entered when they are
//! created. Timers are `!Send`.
//!
//! This extraction preserves upstream's millisecond resolution and lazy
//! registration and reset. The wheel owns entries in a safe indexed slab.
//! The reactor owns execution,
//! waiting, and timer advancement; this module supplies the timer functions.
//! Adapted from Monoio commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1.
//!
//! ```
//! use shep_smp2::{Runtime, time::{Duration, Instant, sleep_until}};
//!
//! let mut runtime = Runtime::new(Default::default())?;
//! let deadline = Instant::now() + Duration::from_millis(10);
//! runtime.block_on(async move { sleep_until(deadline).await })?;
//! assert!(Instant::now() >= deadline);
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp2::time::Timer>();
//! ```
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<shep_smp2::time::Timer>();
//! ```
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp2::time::Sleep>();
//! ```
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<shep_smp2::time::Sleep>();
//! ```
// Adapted from Monoio commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

#![forbid(unsafe_code)]
// Retain the upstream wheel's formatting and control flow.
#![allow(clippy::collapsible_if)]

mod entry;
pub mod error;
mod instant;
mod interval;
mod level;
mod sleep;
mod timeout;
mod timer;
mod wheel;

pub use instant::Instant;
pub use interval::{Interval, MissedTickBehavior, interval, interval_at};
pub use sleep::{Sleep, sleep, sleep_until};
pub use timeout::{Timeout, timeout, timeout_at};
#[doc(hidden)]
pub use timer::Timer;

pub use std::time::Duration;
