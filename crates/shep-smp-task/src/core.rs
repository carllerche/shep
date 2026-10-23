// Adapted from Monoio's task/core.rs; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

use std::{
    cell::UnsafeCell,
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use std::sync::Arc;

use super::{
    raw::{self, Vtable},
    state::State,
    utils::UnsafeCellExt,
    waker::Notification,
};
use crate::{JoinError, Schedule, TaskId};

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

pub(crate) enum Stage<T: Future> {
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
    pub(crate) waker: UnsafeCell<Option<Waker>>,
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
                waker: UnsafeCell::new(None),
            },
        })
    }
}

impl<T: Future> CoreStage<T> {
    pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut Stage<T>) -> R) -> R {
        self.stage.with_mut(f)
    }

    pub(crate) fn poll(&self, mut cx: Context<'_>) -> Poll<T::Output> {
        let res = {
            self.with_mut(|ptr| {
                // Safety: The caller ensures mutual exclusion to the field.
                let future = match unsafe { &mut *ptr } {
                    Stage::Running(future) => future,
                    _ => unreachable!("unexpected stage"),
                };

                // Safety: The caller ensures the future is pinned.
                let future = unsafe { Pin::new_unchecked(future) };

                future.poll(&mut cx)
            })
        };

        if res.is_ready() {
            self.drop_future_or_output();
        }

        res
    }

    /// Drop the future
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `stage` field.
    pub(crate) fn drop_future_or_output(&self) {
        // Safety: the caller ensures mutual exclusion to the field.
        unsafe {
            self.set_stage(Stage::Consumed);
        }
    }

    /// Store the task output
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `stage` field.
    pub(crate) fn store_output(&self, output: Result<T::Output, JoinError>) {
        // Safety: the caller ensures mutual exclusion to the field.
        unsafe {
            self.set_stage(Stage::Finished(output));
        }
    }

    /// Take the task output
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `stage` field.
    pub(crate) fn take_output(&self) -> Result<T::Output, JoinError> {
        use std::mem;

        self.with_mut(|ptr| {
            // Safety:: the caller ensures mutual exclusion to the field.
            match mem::replace(unsafe { &mut *ptr }, Stage::Consumed) {
                Stage::Finished(output) => output,
                _ => panic!("JoinHandle polled after completion"),
            }
        })
    }

    unsafe fn set_stage(&self, stage: Stage<T>) {
        // Assignment drops the old future in place, preserving pinning, and
        // installs the replacement even if that destructor unwinds. Moving
        // the old stage out with ptr::replace would violate pinning.
        self.with_mut(|ptr| unsafe { *ptr = stage })
    }
}

impl Trailer {
    pub(crate) unsafe fn set_waker(&self, waker: Option<Waker>) {
        self.waker.with_mut(|ptr| unsafe {
            *ptr = waker;
        });
    }

    pub(crate) unsafe fn will_wake(&self, waker: &Waker) -> bool {
        self.waker
            .with(|ptr| unsafe { (*ptr).as_ref().unwrap().will_wake(waker) })
    }

    pub(crate) fn take_waker(&self) -> Option<Waker> {
        // Only the owning thread accesses the trailer.
        self.waker.with_mut(|ptr| unsafe { (*ptr).take() })
    }
}
