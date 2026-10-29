//! Admission and service handles; execution state belongs to the scheduler.
#![forbid(unsafe_code)]

use super::{context, remote::Remote};
use crate::task::{JoinHandle, LocalOwnedTasks, Notified, TaskId, new_task};
use std::sync::atomic::Ordering;
use std::{cell::RefCell, collections::VecDeque, future::Future, sync::Arc};

pub(crate) struct Shared {
    pub(super) owned: LocalOwnedTasks,
    inject: RefCell<VecDeque<Notified>>,
    pub(super) remote: Arc<Remote>,
    pub(crate) io: crate::io::Io,
    pub(crate) timer: crate::time::Handle,
}

impl Shared {
    pub(super) fn new(remote: Arc<Remote>, io: crate::io::Io, timer: crate::time::Handle) -> Self {
        Self {
            owned: LocalOwnedTasks::default(),
            inject: RefCell::default(),
            remote,
            io,
            timer,
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        self.remote.closed.load(Ordering::Acquire)
    }

    pub(super) fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (task, join) = new_task(future, self.remote.clone());
        if self.is_closed() {
            drop(task);
        } else {
            self.schedule(self.owned.bind(task));
        }
        join
    }

    fn schedule(&self, task: Notified) {
        // An entered handle alone does not grant access to the scheduler core.
        let mut task = Some(task);
        context::with_current(|handle| {
            if std::ptr::eq(self, handle.shared()) {
                context::with_scheduler(|core| core.ready.push_back(task.take().unwrap()));
            }
        });
        if let Some(task) = task {
            self.inject.borrow_mut().push_back(task);
        }
    }

    pub(super) fn schedule_local(&self, id: TaskId) {
        if let Some(task) = self.owned.notified(id) {
            self.schedule(task);
        }
    }

    pub(super) fn drain_into(&self, ready: &mut VecDeque<Notified>) {
        ready.append(&mut self.inject.borrow_mut());
    }

    pub(super) fn is_empty(&self) -> bool {
        self.owned.is_empty()
    }

    pub(super) fn shutdown(&self) {
        if self.remote.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // Close admission before invoking any task destructor.
        self.owned.shutdown();
        let queued = std::mem::take(&mut *self.inject.borrow_mut());
        drop(queued);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Runtime;

    #[test]
    fn entering_only_exposes_admission_while_executing_exposes_the_core() {
        let rt = Runtime::new(Default::default()).unwrap();
        let _enter = rt.enter();
        assert!(context::with_scheduler(|_| ()).is_none());
        let handle = rt.handle().clone();
        let parent = rt.spawn(async move {
            assert!(context::with_scheduler(|_| ()).is_some());
            let child = handle.spawn(async {});
            assert!(handle.shared().inject.borrow().is_empty());
            assert_eq!(context::with_scheduler(|core| core.ready.len()), Some(1));
            child.await.unwrap();
        });
        assert_eq!(rt.handle().shared().inject.borrow().len(), 1);
        assert_eq!(rt.run_ready(), 1);
        assert!(context::with_scheduler(|_| ()).is_none());
        while rt.has_ready() {
            rt.run_ready();
        }
        assert!(parent.is_finished());
    }
}
