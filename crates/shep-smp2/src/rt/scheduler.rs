// Adapted from Monoio's LocalScheduler and foreign-wake routing, commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Licensed under MIT.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::task::Waker;
use std::thread::{self, ThreadId};

use super::context;
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
        let local = context::with_current(|rt| {
            let shared = rt.shared();
            let local = std::ptr::eq(self, shared.scheduler.as_ref());
            if local {
                shared.schedule_local(task);
            }
            local
        });
        if local == Some(true) {
            return;
        }
        // Publish before waking. The driver must retain a wake token that
        // races with the final ready check and entering its idle wait.
        // Another entered runtime (or a wake outside any runtime)
        // uses the inbox, but this owner thread can never concurrently be
        // blocked in its reactor. Do not invoke the driver waker in that case.
        if self.remote.send(task).is_ok() && self.owner != thread::current().id() {
            self.wake.wake_by_ref();
        }
    }
}
