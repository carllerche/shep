// Adapted from Monoio's LocalScheduler and foreign-wake routing; see docs/task/EXECUTOR-FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Licensed under MIT.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::task::Waker;
use std::thread::{self, ThreadId};

use crate::task::{Schedule, TaskId};

pub(crate) struct Scheduler {
    pub(crate) closed: AtomicBool,
    owner: ThreadId,
    remote: Sender<TaskId>,
    wake: Waker,
}

impl Scheduler {
    pub(crate) fn new(remote: Sender<TaskId>, wake: Waker) -> Self {
        Self {
            closed: AtomicBool::new(false),
            owner: thread::current().id(),
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
        if let Some(context) = crate::task::context::current()
            && std::ptr::eq(self, context.scheduler.as_ref())
        {
            context.ready.borrow_mut().push_back(task);
            return;
        }
        // Publish before waking. The driver must retain a wake token that
        // races with the final ready check and entering its idle wait.
        // A mismatched/nested context (or a wake outside any entered scope)
        // uses the inbox, but this owner thread can never concurrently be
        // blocked in its reactor. Do not invoke the driver waker in that case.
        if self.remote.send(task).is_ok() && self.owner != thread::current().id() {
            self.wake.wake_by_ref();
        }
    }
}
