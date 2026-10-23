// Extracted from Monoio and adapted for shep-smp-time; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

use std::{cell::RefCell, fmt, rc::Rc};

use crate::driver::ClockTime;

thread_local! {
    static CURRENT: RefCell<Option<Handle>> = const { RefCell::new(None) };
}

/// Handle to time driver instance.
#[derive(Clone)]
pub(crate) struct Handle {
    time_source: ClockTime,
    inner: Rc<super::Inner>,
}

impl Handle {
    /// Creates a new timer `Handle` from a shared `Inner` timer state.
    pub(super) fn new(inner: Rc<super::Inner>) -> Self {
        let time_source = inner.state.borrow_mut().time_source.clone();
        Handle { time_source, inner }
    }

    /// Returns the time source associated with this handle
    pub(super) fn time_source(&self) -> &ClockTime {
        &self.time_source
    }

    /// Access the driver's inner structure
    pub(super) fn get(&self) -> &super::Inner {
        &self.inner
    }

    /// Standalone replacement for the enclosing Monoio runtime's time handle.
    pub(super) fn with<R>(&self, f: impl FnOnce() -> R) -> R {
        struct Reset(Option<Handle>);
        impl Drop for Reset {
            fn drop(&mut self) {
                CURRENT.with(|slot| slot.replace(self.0.take()));
            }
        }
        let _reset = Reset(CURRENT.with(|slot| slot.replace(Some(self.clone()))));
        f()
    }
}

impl Handle {
    /// Tries to get a handle to the current timer.
    ///
    /// # Panics
    ///
    /// This function panics if there is no current timer set.
    ///
    /// Timers must be constructed inside `TimeDriver::with`.
    pub(crate) fn current() -> Self {
        CURRENT.with(|slot| {
            slot.borrow()
                .clone()
                .expect("timer requires TimeDriver::with")
        })
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Handle")
    }
}
