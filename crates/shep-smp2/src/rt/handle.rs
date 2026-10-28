#![forbid(unsafe_code)]

use std::future::Future;
use std::rc::Rc;

use super::{context, shared::Shared};
use crate::io::Io;
use crate::task::JoinHandle;

/// A local reference to a runtime's tasks, timers and I/O. Cloning is cheap.
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<shep_smp2::Handle>();
/// ```
#[derive(Clone)]
pub struct Handle {
    shared: Rc<Shared>,
}

impl Handle {
    pub(super) fn new(shared: Shared) -> Self {
        Self {
            shared: Rc::new(shared),
        }
    }

    /// The runtime entered on this thread.
    ///
    /// # Panics
    ///
    /// Panics outside a runtime context.
    pub fn current() -> Self {
        Self::try_current().expect("must be called from within a shep_smp2 runtime")
    }

    /// The runtime entered on this thread, if any.
    pub fn try_current() -> Option<Self> {
        context::with_current(Self::clone)
    }

    /// Queue a future without polling it. After shutdown, the future is dropped
    /// and its join is cancelled.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        self.shared.spawn(future)
    }

    pub fn io(&self) -> Io {
        self.shared.driver.io.clone()
    }

    pub(crate) fn shared(&self) -> &Shared {
        &self.shared
    }

    pub(super) fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.shared, &other.shared)
    }
}
