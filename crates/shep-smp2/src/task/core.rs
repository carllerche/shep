// Adapted from Monoio's task/core.rs, commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

use std::{
    cell::{Cell as StdCell, UnsafeCell},
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use std::sync::Arc;

use super::{
    raw::{self, Vtable},
    state::State,
    waker::Notification,
};
use crate::task::{JoinError, Schedule, TaskId};

// Header must stay first: RawTask erases Cell<T> to its header pointer, and
// the matching T vtable restores that original allocation for final destruction.
#[repr(C)]
pub(crate) struct Cell<T: Future> {
    pub(crate) header: Header,
    pub(crate) core: Core<T>,
    pub(crate) trailer: Trailer,
}

pub(crate) struct Core<T: Future> {
    /// Either the future or the output
    pub(crate) stage: CoreStage<T>,
}
pub(crate) struct CoreStage<T: Future> {
    stage: UnsafeCell<Stage<T>>,
}

enum Stage<T: Future> {
    Running(T),
    Finished(Result<T::Output, JoinError>),
    Consumed,
}

#[repr(C)]
pub(crate) struct Header {
    /// Thread-safe notification metadata, separate from the local allocation.
    pub(crate) shared: Arc<Notification>,
    /// Table of function pointers for executing actions on the task.
    pub(crate) vtable: &'static Vtable,
}

pub(crate) struct Trailer {
    /// Consumer task waiting on completion of this task.
    waker: StdCell<Option<Waker>>,
}

impl<T: Future> Cell<T> {
    /// Allocates a new task cell, containing the header, trailer, and core
    /// structures.
    pub(crate) fn new(future: T, scheduler: Arc<dyn Schedule>) -> Box<Cell<T>> {
        Box::new(Cell {
            header: Header {
                shared: Arc::new(Notification {
                    state: State::new(),
                    id: TaskId::next(),
                    scheduler,
                }),
                vtable: raw::vtable::<T>(),
            },
            core: Core {
                stage: CoreStage {
                    stage: UnsafeCell::new(Stage::Running(future)),
                },
            },
            trailer: Trailer {
                waker: StdCell::new(None),
            },
        })
    }
}

impl<T: Future> CoreStage<T> {
    /// Access is confined to the owner thread. While polling or dropping a
    /// Running future, the lifecycle is RUNNING: reentrant join detachment
    /// cannot touch the stage. Finished outputs are moved out before invoking
    /// their destructors, so those callbacks cannot alias this reference.
    fn with_stage<R>(&self, f: impl FnOnce(&mut Stage<T>) -> R) -> R {
        // SAFETY: The local handles and lifecycle enforce the exclusion above.
        // This private closure API prevents the reference escaping the access.
        f(unsafe { &mut *self.stage.get() })
    }

    pub(crate) fn poll(&self, mut cx: Context<'_>) -> Poll<T::Output> {
        let res = self.with_stage(|stage| {
            let future = match stage {
                Stage::Running(future) => future,
                _ => unreachable!("unexpected stage"),
            };
            // SAFETY: The task cell has a stable heap address until deallocation.
            // No method moves a Running stage: replacement drops it in place,
            // and take_output only moves Finished. The RUNNING lifecycle prevents
            // reentrant stage access while polling.
            unsafe { Pin::new_unchecked(future) }.poll(&mut cx)
        });
        if res.is_ready() {
            self.drop_future_or_output();
        }
        res
    }

    pub(crate) fn drop_future_or_output(&self) {
        // Outputs are not pinned. Move them out before running user Drop code,
        // which can reenter local handles after COMPLETE has been published.
        let output = self.with_stage(|stage| {
            if matches!(stage, Stage::Finished(_)) {
                Some(std::mem::replace(stage, Stage::Consumed))
            } else {
                // A Running future is pinned: drop it in place. Assignment
                // installs Consumed even when its destructor unwinds.
                *stage = Stage::Consumed;
                None
            }
        });
        drop(output);
    }

    pub(crate) fn store_output(&self, output: Result<T::Output, JoinError>) {
        // Running is only replaced during cancellation, before COMPLETE.
        self.with_stage(|stage| *stage = Stage::Finished(output));
    }

    pub(crate) fn take_output(&self) -> Result<T::Output, JoinError> {
        self.with_stage(|stage| {
            // Never move a Running future, even on an invalid join poll.
            assert!(
                matches!(stage, Stage::Finished(_)),
                "JoinHandle polled after completion"
            );
            match std::mem::replace(stage, Stage::Consumed) {
                Stage::Finished(output) => output,
                _ => unreachable!(),
            }
        })
    }
}

impl Trailer {
    pub(crate) fn take_waker(&self) -> Option<Waker> {
        self.waker.take()
    }

    pub(crate) fn set_waker(&self, waker: Waker) {
        // Drop the old waker after replacement, so a reentrant destructor sees
        // valid storage and no outstanding mutable reference or cell borrow.
        drop(self.waker.replace(Some(waker)));
    }

    pub(crate) fn will_wake(&self, waker: &Waker) -> bool {
        let old = self.waker.take();
        let same = old.as_ref().is_some_and(|old| old.will_wake(waker));
        self.waker.set(old);
        same
    }
}
