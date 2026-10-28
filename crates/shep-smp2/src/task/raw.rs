// Adapted from Monoio's task/raw.rs; see docs/task/FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

use std::{
    future::Future,
    ptr::NonNull,
    task::{Poll, Waker},
};

use crate::task::{
    JoinError, Schedule,
    core::{Cell, Header},
    harness::Harness,
};
use std::sync::Arc;

/// Non-owning token used by exactly two local owners: Task and JoinHandle.
/// Copying this token does not acquire a reference. See docs/task/SAFETY.md for the
/// allocation, matching-vtable and final-release invariants.
pub(crate) struct RawTask {
    ptr: NonNull<Header>,
}

impl Clone for RawTask {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for RawTask {}

/// Every entry requires a live Cell<F> pointer paired with this F's vtable,
/// on its owner thread. Deallocation additionally requires the final local
/// reference; output reads require a destination for precisely F::Output.
pub(crate) struct Vtable {
    /// Poll the future
    pub(crate) poll: unsafe fn(NonNull<Header>),
    /// Deallocate the memory
    pub(crate) dealloc: unsafe fn(NonNull<Header>),

    /// Read the task output, if complete
    pub(crate) try_read_output: unsafe fn(NonNull<Header>, *mut (), &Waker),

    /// The join handle has been dropped
    pub(crate) drop_join_handle_slow: unsafe fn(NonNull<Header>),

    /// Cancel a future on its owning thread.
    pub(crate) cancel: unsafe fn(NonNull<Header>),
}

/// Get the vtable for the requested future type.
pub(super) fn vtable<T: Future>() -> &'static Vtable {
    &Vtable {
        poll: poll::<T>,
        dealloc: dealloc::<T>,
        try_read_output: try_read_output::<T>,
        drop_join_handle_slow: drop_join_handle_slow::<T>,
        cancel: cancel::<T>,
    }
}

impl RawTask {
    pub(crate) fn new<T>(task: T, scheduler: Arc<dyn Schedule>) -> RawTask
    where
        T: Future,
    {
        let ptr = NonNull::from(Box::leak(Cell::new(task, scheduler))).cast();

        RawTask { ptr }
    }

    pub(crate) fn header(&self) -> &Header {
        // SAFETY: Only live local owners call this accessor.
        unsafe { self.ptr.as_ref() }
    }

    /// Safety: mutual exclusion is required to call this function.
    pub(crate) fn poll(self) {
        let vtable = self.header().vtable;
        // SAFETY: Task::run has unique owner access and checked notification.
        unsafe { (vtable.poll)(self.ptr) }
    }

    pub(crate) fn dealloc(self) {
        let vtable = self.header().vtable;
        // SAFETY: Both release paths call this only at reference count zero.
        unsafe {
            (vtable.dealloc)(self.ptr);
        }
    }

    /// Safety: `dst` must be a `*mut Poll<Result<T::Output, JoinError>>` where `T`
    /// is the future stored by the task.
    pub(crate) unsafe fn try_read_output(self, dst: *mut (), waker: &Waker) {
        let vtable = self.header().vtable;
        // SAFETY: The caller supplies the matching output slot; this token
        // and its vtable were constructed together for the stored future.
        unsafe {
            (vtable.try_read_output)(self.ptr, dst, waker);
        }
    }

    pub(crate) fn drop_join_handle_slow(self) {
        let vtable = self.header().vtable;
        // SAFETY: Called once by the unique local JoinHandle on final drop.
        unsafe { (vtable.drop_join_handle_slow)(self.ptr) }
    }

    pub(crate) fn cancel(self) {
        let vtable = self.header().vtable;
        // SAFETY: Called by the local Task owner before releasing its reference.
        unsafe { (vtable.cancel)(self.ptr) }
    }
}

unsafe fn poll<T: Future>(ptr: NonNull<Header>) {
    // SAFETY: This entry is installed only in Cell<T>'s vtable, and the
    // caller retains a local owner until the operation releases it.
    let harness = unsafe { Harness::<T>::from_raw(ptr) };
    harness.poll();
}

unsafe fn dealloc<T: Future>(ptr: NonNull<Header>) {
    // SAFETY: This entry is installed only in Cell<T>'s vtable. The final
    // reference was released, but the allocation remains live until freed below.
    let harness = unsafe { Harness::<T>::from_raw(ptr) };
    harness.dealloc();
}

unsafe fn cancel<T: Future>(ptr: NonNull<Header>) {
    // SAFETY: This entry is installed only in Cell<T>'s vtable, and the
    // caller retains a local owner until the operation releases it.
    let harness = unsafe { Harness::<T>::from_raw(ptr) };
    harness.cancel();
}

unsafe fn try_read_output<T: Future>(ptr: NonNull<Header>, dst: *mut (), waker: &Waker) {
    // SAFETY: The private JoinHandle constructor pairs its output type with
    // this task, and poll supplies an exclusive stack slot of that type.
    let out = unsafe { &mut *(dst as *mut Poll<Result<T::Output, JoinError>>) };

    // SAFETY: This entry is installed only in Cell<T>'s vtable, and the
    // caller retains a local owner until the operation releases it.
    let harness = unsafe { Harness::<T>::from_raw(ptr) };
    harness.try_read_output(out, waker);
}

unsafe fn drop_join_handle_slow<T: Future>(ptr: NonNull<Header>) {
    // SAFETY: This entry is installed only in Cell<T>'s vtable, and the
    // caller retains a local owner until the operation releases it.
    let harness = unsafe { Harness::<T>::from_raw(ptr) };
    harness.drop_join_handle_slow()
}
