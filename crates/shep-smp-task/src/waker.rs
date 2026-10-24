// Adapted from Monoio's task notification state; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::task::Wake;

use crate::state::{State, TransitionToNotified};
use crate::{Schedule, TaskId};

/// Only this allocation is shared with foreign threads. It holds no pointer
/// to a task cell, future, output, join handle, or local executor state.
pub(crate) struct Notification {
    pub(crate) state: State,
    pub(crate) id: TaskId,
    pub(crate) scheduler: Arc<dyn Schedule>,
}

impl Wake for Notification {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let TransitionToNotified::Submit = self.state.transition_to_notified() {
            self.scheduler.schedule(self.id);
        }
    }
}
