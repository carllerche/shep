//! Native per-core runtimes and SMP orchestration: tasks, io_uring and timers.
//! [`smp`] adds worker threads and cross-core execution without application state.
//!
//! - [`rt`]: the runtime owning tasks, timers and the I/O driver.
//! - [`task`]: local tasks, [`spawn`] and task IDs.
//! - [`time`]: the reactor-owned timing wheel, sleeps, timeouts and intervals.
//! - [`io`]: the owned-operation io_uring driver.
//! - [`fs`]: positional owned-buffer file operations over [`io`].
//! - [`net`]: owned-buffer TCP over [`io`].
//!
//! Callers should stop producing work and await all application operations before
//! [`Runtime::shutdown`] or runtime destruction. Cancel indefinite operations and
//! await their terminal completion first; dropping a future is not completion.
//! The I/O driver's destructor makes a best-effort cancellation attempt for any
//! remaining work and waits for terminal completions. If safe retirement fails,
//! it aborts the process instead of freeing inputs the kernel may still access.
//! This fallback does not guarantee successful writes, flushes, or transactions.
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
pub mod rt;
pub mod smp;
pub mod task;
pub mod time;

pub use fs::File;
pub use io::{Buffer, Io};
pub use rt::{Config, Handle, Runtime};
pub use smp::{CoreId, Smp, SmpConfig, SmpError, current_core};
pub use task::{JoinError, JoinHandle, spawn};
pub use time::{sleep, sleep_until, timeout};
