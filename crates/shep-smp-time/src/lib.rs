//! Local timers extracted from Monoio's `time` module.
//!
//! [`TimeDriver`] wraps a waiting [`Driver`] with Monoio's six-level hashed
//! timing wheel. [`sleep`], [`sleep_until`], [`timeout`], [`timeout_at`], and
//! [`interval`] use the timer selected by [`Driver::with`]. Create and poll
//! timer futures inside that scope. Timers and their driver are `!Send`.
//!
//! This extraction preserves upstream's millisecond resolution, intrusive
//! pinned entries, lazy registration and reset, and processing after park.
//! The future io_uring reactor will implement the small [`Driver`] interface;
//! this crate supplies no I/O backend or runtime loop. See `FORK.md` for the
//! exact source revision and extraction changes.
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

// Keep upstream's unsafe implementation intact while extracting from its
// edition-2021 crate. These operations will be adapted in later changes.
#![allow(unsafe_op_in_unsafe_fn)]
// Retain the upstream wheel's formatting and control flow.
#![allow(clippy::collapsible_if)]

mod clock;
pub(crate) use clock::Clock;

pub mod driver;
pub use driver::{
    Driver, TimeDriver,
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

mod utils {
    // Preserve the upstream helper and its tests, including methods not used
    // by this extraction of Monoio.
    #[allow(dead_code)]
    pub(crate) mod linked_list;
}
