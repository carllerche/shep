//! Local tasks for Shep's per-core runtime, adapted from Monoio's task module.
//!
//! [`new_task`] allocates a pinned future and its [`JoinHandle`]. The executor
//! owns the returned [`Task`] until completion or shutdown, queues its [`TaskId`]
//! for the initial poll, and calls [`Task::run`] for subsequent notifications.
//! Dropping a task cancels it; dropping its join handle detaches it.
//!
//! Futures, outputs, tasks, and join handles stay on their creating thread.
//! Standard Rust wakers can cross threads: they only update shared notification
//! state and call [`Schedule::schedule`] with an opaque ID. They never access
//! the future or output. Executors must ignore IDs for tasks already removed.
//!
//! This crate supplies no executor, runtime context, timer, or I/O driver and
//! has no runtime dependencies. See `FORK.md` for provenance and adaptations.
//!
//! Tasks and join handles remain local even when the output is `Send`:
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp_task::Task>();
//! ```
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp_task::JoinHandle<()>>();
//! ```
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<shep_smp_task::Task>();
//! ```
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<shep_smp_task::JoinHandle<()>>();
//! ```
// Adapted from Monoio, commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

mod core;
mod harness;
mod join;
mod raw;
mod state;
mod utils;
mod waker;

pub use join::JoinHandle;

use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use self::raw::RawTask;

/// A process-unique task notification token. IDs are never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(u64);

impl TaskId {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(
            NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("task IDs exhausted"),
        )
    }
}

/// The executor's notification queue and wakeup mechanism.
///
/// Called by standard Rust wakers, potentially from another thread. Enqueue
/// the ID and wake the owning executor; do not poll a task here. Notifications
/// may race with task removal, so stale IDs must be ignored. A scheduler should
/// return harmlessly after its executor shuts down and should not panic.
pub trait Schedule: Send + Sync + 'static {
    fn schedule(&self, task: TaskId);
}

/// The executor's local ownership of a pinned future.
///
/// Keep this handle while the task is pending, including when it has no waker.
/// Dropping it drops a pending future on this thread and completes its join
/// handle with [`JoinError::Cancelled`]. Wakers can safely outlive this handle.
pub struct Task {
    raw: RawTask,
    _local: PhantomData<Rc<()>>,
}

impl Task {
    /// The token to put in the executor's ready queue for the first poll.
    pub fn id(&self) -> TaskId {
        self.raw.header().shared.id
    }

    /// Poll once if notified. Returns whether the task has completed.
    ///
    /// Duplicate notifications and calls after completion are harmless. A wake
    /// during polling queues another turn, rather than polling recursively.
    /// Panics from the future become [`JoinError::Panicked`].
    pub fn run(&mut self) -> bool {
        let state = self.raw.header().shared.state.load();
        if !state.is_complete() && state.is_notified() {
            self.raw.poll();
        }
        self.is_finished()
    }

    pub fn is_finished(&self) -> bool {
        self.raw.header().shared.state.load().is_complete()
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.raw.cancel();
        if self.raw.header().shared.state.ref_dec() {
            self.raw.dealloc();
        }
    }
}

/// Allocate a local task without polling or enqueueing it.
///
/// The executor must retain `Task` and enqueue its ID for the initial poll.
/// The scheduler is shared notification plumbing; it does not own the future.
pub fn new_task<F>(future: F, scheduler: Arc<dyn Schedule>) -> (Task, JoinHandle<F::Output>)
where
    F: Future + 'static,
    F::Output: 'static,
{
    let raw = RawTask::new(future, scheduler);
    let task = Task {
        raw,
        _local: PhantomData,
    };
    (task, JoinHandle::new(raw))
}

/// Why a task did not produce an output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinError {
    /// The executor dropped its task before completion.
    Cancelled,
    /// The future panicked during execution, including its completion drop.
    Panicked,
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "task cancelled",
            Self::Panicked => "task panicked",
        })
    }
}

impl std::error::Error for JoinError {}
