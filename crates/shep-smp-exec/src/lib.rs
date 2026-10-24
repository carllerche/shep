//! Local task execution, following Monoio's scheduler/runtime architecture.
//!
//! An [`Executor`] owns every pending task. [`Executor::spawn`] queues its
//! first poll; [`spawn`] does the same from a task currently running here.
//! [`Executor::run_ready`] runs a bounded turn and returns control to the
//! runtime so it can service I/O and timers. Dropping the executor cancels all
//! pending tasks on its owning thread, including detached tasks.
//!
//! Futures, outputs, and the executor stay local. Only the task notification
//! inbox and the driver's [`Waker`] cross threads. This crate neither blocks
//! nor owns an I/O driver; the future io_uring runtime supplies its wakeup
//! mechanism when constructing the executor. See `FORK.md` for source context.
//!
//! ```
//! use std::task::Waker;
//! use shep_smp_exec::{Executor, spawn};
//!
//! // A manually driven executor needs no idle-driver wakeup.
//! let mut exec = Executor::new(Waker::noop().clone());
//! let result = exec.spawn(async { spawn(async { 42 }).await.unwrap() });
//! while exec.has_ready() {
//!     exec.run_ready(64);
//! }
//! assert!(result.is_finished());
//! ```
//!
//! The executor remains local even if its tasks happen to be `Send`:
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp_exec::Executor>();
//! ```
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<shep_smp_exec::Executor>();
//! ```
// Adapted from Monoio's scheduler/runtime architecture, commit
// 8675e9406ea7943c441d969e38c3f5fc198ef6d1. See FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Licensed under MIT.

#![forbid(unsafe_code)]

mod context;
mod scheduler;

pub use shep_smp_task::{JoinError, JoinHandle};

use std::future::Future;
use std::rc::Rc;
use std::task::Waker;

use context::{Context, Scope};

/// A FIFO executor for one runtime thread. It never polls tasks inline at spawn.
pub struct Executor {
    context: Rc<Context>,
}

/// Scoped owner context for task creation and local wake routing, including
/// reactor/timer callbacks outside task polling. Guards must be dropped in LIFO
/// order and cannot cross threads. They weakly reference executor state.
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<shep_smp_exec::EnterGuard>();
/// ```
#[must_use = "keep the guard alive while driving this executor's runtime"]
pub struct EnterGuard {
    _scope: Scope,
}

impl Executor {
    /// Create an executor using `wake` to interrupt its driver's idle wait.
    ///
    /// The waker must tolerate calls from foreign threads and retain a wakeup
    /// that races with the driver going to sleep. The driver must recheck
    /// [`has_ready`](Self::has_ready) before blocking. The waker should use
    /// weak ownership of driver resources so outstanding task wakers do not
    /// keep a shut-down reactor alive.
    pub fn new(wake: Waker) -> Self {
        Self {
            context: Rc::new(Context::new(wake)),
        }
    }

    /// Enter this executor's context while driving its runtime. Nested context
    /// scopes restore the previous owner; task polling still rejects recursion.
    pub fn enter(&self) -> EnterGuard {
        EnterGuard {
            _scope: Scope::enter(&self.context),
        }
    }

    /// Queue a future without polling it. Dropping the returned handle detaches
    /// the task. After shutdown, the future is dropped and its join is cancelled.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        self.context.spawn(future)
    }

    /// Process at most `budget` ready entries, returning the number of tasks
    /// polled. Self-wakes and children spawned during a turn join the FIFO.
    /// Stale notifications consume budget without polling a task.
    ///
    /// Remote ingress is also bounded by `budget`. Use [`has_ready`](Self::has_ready)
    /// before blocking; a zero poll count does not imply an empty ready queue.
    ///
    /// # Panics
    ///
    /// Panics if a task-polling turn is already active on this thread. A future's
    /// panic instead completes its join with [`JoinError::Panicked`].
    pub fn run_ready(&mut self, budget: usize) -> usize {
        let _scope = Scope::poll(&self.context);
        if self.context.is_closed() {
            return 0;
        }
        self.context.import_remote(budget);
        let mut polled = 0;
        for _ in 0..budget {
            let Some(id) = self.context.ready.borrow_mut().pop_front() else {
                break;
            };
            // Release the registry borrow before polling: user code can spawn
            // children, drop captures, or wake other tasks in this executor.
            let task = self.context.tasks.borrow_mut().remove(&id);
            if let Some(mut task) = task {
                polled += 1;
                if !task.run() {
                    self.context.tasks.borrow_mut().insert(id, task);
                }
            }
        }
        polled
    }

    /// Whether a turn can make progress, including on foreign notifications.
    /// A stale notification can make this true until a turn discards it.
    pub fn has_ready(&self) -> bool {
        if self.context.is_closed() {
            return false;
        }
        if self.context.ready.borrow().is_empty() {
            self.context.import_remote(1);
        }
        !self.context.ready.borrow().is_empty()
    }

    /// Whether all tasks have finished or been cancelled. Pending tasks waiting
    /// for a channel or I/O still belong to this executor, even with no ready work.
    pub fn is_empty(&self) -> bool {
        self.context.tasks.borrow().is_empty()
    }

    /// Close admission and cancel every pending task on this thread. Idempotent.
    /// Future destructors run in this executor's scope; any work they spawn is
    /// immediately cancelled. Foreign wakes after shutdown are harmless.
    pub fn shutdown(&mut self) {
        self.context.shutdown();
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        self.context.shutdown();
    }
}

/// Queue a future on the executor currently entered on this thread.
///
/// # Panics
///
/// Panics outside an entered runtime, executor turn or shutdown scope.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    context::current()
        .expect("spawn requires a running executor")
        .spawn(future)
}
