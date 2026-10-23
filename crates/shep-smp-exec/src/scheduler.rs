// Adapted from Monoio's LocalScheduler and foreign-wake routing; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Licensed under MIT.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::task::Waker;

use shep_smp_task::{Schedule, TaskId};

pub(crate) struct Scheduler {
    pub(crate) closed: AtomicBool,
    remote: Sender<TaskId>,
    wake: Waker,
}

impl Scheduler {
    pub(crate) fn new(remote: Sender<TaskId>, wake: Waker) -> Self {
        Self {
            closed: AtomicBool::new(false),
            remote,
            wake,
        }
    }
}

impl Schedule for Scheduler {
    fn schedule(&self, task: TaskId) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        if let Some(context) = crate::context::current()
            && std::ptr::eq(self, context.scheduler.as_ref())
        {
            context.ready.borrow_mut().push_back(task);
            return;
        }
        // Publish before waking. The driver must retain a wake token that
        // races with the final ready check and entering its idle wait.
        if self.remote.send(task).is_ok() {
            self.wake.wake_by_ref();
        }
    }
}
