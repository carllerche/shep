// Adapted from Monoio's task/raw.rs; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

use std::{
    future::Future,
    ptr::NonNull,
    task::{Poll, Waker},
};

use crate::{
    JoinError, Schedule,
    core::{Cell, Header},
    harness::Harness,
};
use std::sync::Arc;

pub(crate) struct RawTask {
    ptr: NonNull<Header>,
}

impl Clone for RawTask {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for RawTask {}

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
        let ptr = Box::into_raw(Cell::new(task, scheduler));
        let ptr = unsafe { NonNull::new_unchecked(ptr as *mut Header) };

        RawTask { ptr }
    }

    pub(crate) fn header(&self) -> &Header {
        unsafe { self.ptr.as_ref() }
    }

    /// Safety: mutual exclusion is required to call this function.
    pub(crate) fn poll(self) {
        let vtable = self.header().vtable;
        unsafe { (vtable.poll)(self.ptr) }
    }

    pub(crate) fn dealloc(self) {
        let vtable = self.header().vtable;
        unsafe {
            (vtable.dealloc)(self.ptr);
        }
    }

    /// Safety: `dst` must be a `*mut Poll<Result<T::Output, JoinError>>` where `T`
    /// is the future stored by the task.
    pub(crate) unsafe fn try_read_output(self, dst: *mut (), waker: &Waker) {
        let vtable = self.header().vtable;
        unsafe {
            (vtable.try_read_output)(self.ptr, dst, waker);
        }
    }

    pub(crate) fn drop_join_handle_slow(self) {
        let vtable = self.header().vtable;
        unsafe { (vtable.drop_join_handle_slow)(self.ptr) }
    }

    pub(crate) fn cancel(self) {
        let vtable = self.header().vtable;
        unsafe { (vtable.cancel)(self.ptr) }
    }
}

unsafe fn poll<T: Future>(ptr: NonNull<Header>) {
    let harness = unsafe { Harness::<T>::from_raw(ptr) };
    harness.poll();
}

unsafe fn dealloc<T: Future>(ptr: NonNull<Header>) {
    let harness = unsafe { Harness::<T>::from_raw(ptr) };
    harness.dealloc();
}

unsafe fn cancel<T: Future>(ptr: NonNull<Header>) {
    let harness = unsafe { Harness::<T>::from_raw(ptr) };
    harness.cancel();
}

unsafe fn try_read_output<T: Future>(ptr: NonNull<Header>, dst: *mut (), waker: &Waker) {
    let out = unsafe { &mut *(dst as *mut Poll<Result<T::Output, JoinError>>) };

    let harness = unsafe { Harness::<T>::from_raw(ptr) };
    harness.try_read_output(out, waker);
}

unsafe fn drop_join_handle_slow<T: Future>(ptr: NonNull<Header>) {
    let harness = unsafe { Harness::<T>::from_raw(ptr) };
    harness.drop_join_handle_slow()
}
