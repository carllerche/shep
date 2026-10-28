//! Native single-core runtime: bounded task turns, io_uring, and timer deadlines.
//! This parallel implementation leaves shep-smp and its consumers unchanged.
//! Thread-per-core orchestration is subsequent.
//!
//! - [`task`]: local tasks and the FIFO executor.
//! - [`time`]: the reactor-owned timing wheel, sleeps, timeouts and intervals.
//! - [`io`]: the owned-operation io_uring driver.
//! - [`fs`]: positional owned-buffer file operations over [`io`].
//! - [`net`]: owned-buffer TCP over [`io`].
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp2::Runtime>();
//! ```
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<shep_smp2::Runtime>();
//! ```
#![deny(unsafe_op_in_unsafe_fn)]

pub mod fs;
pub mod io;
pub mod net;
mod rt;
pub mod task;
pub mod time;

pub use fs::File;
pub use io::{Buffer, Io};
pub use rt::{Config, Park, Parking, Runtime, WakeHandle};
pub use task::{JoinError, JoinHandle, spawn};
pub use time::{sleep, sleep_until, timeout};
