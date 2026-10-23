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
    /// Panics if an executor is already running on this thread. A future's
    /// panic instead completes its join with [`JoinError::Panicked`].
    pub fn run_ready(&mut self, budget: usize) -> usize {
        assert!(context::current().is_none(), "cannot run a nested executor");
        if self.context.is_closed() {
            return 0;
        }
        let _scope = Scope::enter(&self.context);
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

/// Queue a future on the executor currently polling this task, as in Monoio.
///
/// # Panics
///
/// Panics when called outside an executor turn or its shutdown scope.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    context::current()
        .expect("spawn requires a running executor")
        .spawn(future)
}
