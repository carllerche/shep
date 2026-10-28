//! Single-owner execution state, lent to the active scheduler context per turn.
#![forbid(unsafe_code)]

use super::{Handle, context, shared::Shared};
use crate::task::{Notified, TaskId};
use std::{cell::Cell, collections::VecDeque, sync::mpsc::Receiver};

pub(super) struct Scheduler {
    core: Cell<Option<Core>>,
}

pub(super) struct Core {
    pub(super) ready: VecDeque<Notified>,
    remote: Receiver<TaskId>,
}

impl Core {
    fn import(&mut self, shared: &Shared) {
        shared.drain_into(&mut self.ready);
        while let Ok(id) = self.remote.try_recv() {
            if let Some(task) = shared.owned.notified(id) {
                self.ready.push_back(task);
            }
        }
    }
}

impl Scheduler {
    pub(super) fn new(remote: Receiver<TaskId>) -> Self {
        Self {
            core: Cell::new(Some(Core {
                ready: VecDeque::new(),
                remote,
            })),
        }
    }

    pub(super) fn run_ready(&self, handle: &Handle) -> usize {
        assert!(
            context::current_task_id().is_none(),
            "cannot run the scheduler from a task"
        );
        let shared = handle.shared();
        if shared.is_closed() {
            return 0;
        }
        let core = self.core.take().expect("scheduler already running");
        context::set_scheduler(Some(core));
        let _restore = Restore(self);
        let turn = context::with_scheduler(|core| {
            core.import(shared);
            core.ready.len()
        })
        .expect("active scheduler core missing");
        let mut polled = 0;
        // Freeze the turn length: self-wakes and children wait until next turn.
        for _ in 0..turn {
            let task = context::with_scheduler(|core| core.ready.pop_front()).flatten();
            let Some(task) = task else { break };
            let id = task.id();
            polled += 1;
            // No core or ownership borrow may span user code.
            if task.run() {
                shared.owned.release(id);
            }
        }
        polled
    }

    pub(super) fn has_ready(&self, shared: &Shared) -> bool {
        if shared.is_closed() {
            return false;
        }
        if let Some(core) = self.core.take() {
            let mut restore = RestoreParked {
                slot: &self.core,
                core: Some(core),
            };
            let core = restore.core.as_mut().unwrap();
            core.import(shared);
            !core.ready.is_empty()
        } else {
            // A task may inspect its own runtime while the core is in TLS.
            context::with_scheduler(|core| {
                core.import(shared);
                !core.ready.is_empty()
            })
            .expect("active scheduler core missing")
        }
    }

    pub(super) fn shutdown(&mut self) {
        let core = self
            .core
            .get_mut()
            .as_mut()
            .expect("scheduler already running");
        core.ready.clear();
        while core.remote.try_recv().is_ok() {}
    }
}

struct Restore<'a>(&'a Scheduler);
impl Drop for Restore<'_> {
    fn drop(&mut self) {
        self.0.core.set(context::set_scheduler(None));
    }
}

/// Return a temporarily inspected core even if importing notifications unwinds.
struct RestoreParked<'a> {
    slot: &'a Cell<Option<Core>>,
    core: Option<Core>,
}

impl Drop for RestoreParked<'_> {
    fn drop(&mut self) {
        self.slot.set(self.core.take());
    }
}
