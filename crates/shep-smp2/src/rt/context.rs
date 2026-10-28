//! The single scoped thread-local that tracks the entered runtime.
//!
//! A thread enters at most one runtime at a time; entering never nests, so a
//! guard only clears what it installed. Readers must not run user code, wake
//! tasks or drop futures while the handle is borrowed.
#![forbid(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::rc::Rc;

use super::{Handle, scheduler::Core};
use crate::task::TaskId;

struct Context {
    /// The runtime entered on this thread.
    current: RefCell<Option<Handle>>,
    scheduler: RefCell<Option<Core>>,
    /// The task whose future is being polled or dropped.
    current_task_id: Cell<Option<TaskId>>,
}

thread_local! {
    static CONTEXT: Context = const {
        Context {
            current: RefCell::new(None),
            scheduler: RefCell::new(None),
            current_task_id: Cell::new(None),
        }
    };
}

/// Runtime context for the current thread, cleared on drop or unwind.
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<shep_smp2::rt::EnterGuard>();
/// ```
#[must_use = "the runtime is exited when the guard is dropped"]
pub struct EnterGuard {
    _local: PhantomData<Rc<()>>,
}

impl Drop for EnterGuard {
    fn drop(&mut self) {
        // Release the borrow before the handle drops: it may be the last owner
        // of the runtime, whose task destructors read this context.
        let previous = CONTEXT
            .try_with(|cx| cx.current.borrow_mut().take())
            .ok()
            .flatten();
        drop(previous);
    }
}

/// Enter `handle`, or return None if it is already entered on this thread.
///
/// # Panics
///
/// Panics if a different runtime is entered on this thread.
pub(super) fn try_enter(handle: &Handle) -> Option<EnterGuard> {
    CONTEXT.with(|cx| {
        let mut current = cx.current.borrow_mut();
        match &*current {
            Some(entered) if entered.ptr_eq(handle) => None,
            Some(_) => panic!("cannot enter a runtime while another runtime is entered"),
            None => {
                *current = Some(handle.clone());
                Some(EnterGuard {
                    _local: PhantomData,
                })
            }
        }
    })
}

/// # Panics
///
/// Panics if any runtime is already entered on this thread.
pub(super) fn enter(handle: &Handle) -> EnterGuard {
    try_enter(handle).expect("runtime is already entered on this thread")
}

/// Whether a runtime other than `handle` is entered on this thread.
pub(super) fn entered_elsewhere(handle: &Handle) -> bool {
    with_current(|entered| !entered.ptr_eq(handle)).unwrap_or(false)
}

/// Borrow the entered runtime. Returns None outside a runtime or during
/// thread-local destruction.
pub(crate) fn with_current<R>(f: impl FnOnce(&Handle) -> R) -> Option<R> {
    CONTEXT
        .try_with(|cx| cx.current.borrow().as_ref().map(f))
        .ok()
        .flatten()
}

pub(crate) fn set_current_task_id(id: Option<TaskId>) -> Option<TaskId> {
    CONTEXT
        .try_with(|cx| cx.current_task_id.replace(id))
        .unwrap_or_default()
}

pub(crate) fn current_task_id() -> Option<TaskId> {
    CONTEXT
        .try_with(|cx| cx.current_task_id.get())
        .unwrap_or_default()
}

/// Access is brief and must never span polling, destruction, or user callbacks.
pub(super) fn with_scheduler<R>(f: impl FnOnce(&mut Core) -> R) -> Option<R> {
    CONTEXT.with(|cx| cx.scheduler.borrow_mut().as_mut().map(f))
}

pub(super) fn set_scheduler(core: Option<Core>) -> Option<Core> {
    CONTEXT.with(|cx| cx.scheduler.replace(core))
}
