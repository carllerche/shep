//! Local cancellation ownership and lookup for ID-only wake notifications.
#![forbid(unsafe_code)]

use super::{Notified, Task, TaskId};
use std::{cell::RefCell, collections::HashMap};

#[derive(Default)]
pub(crate) struct LocalOwnedTasks {
    tasks: RefCell<HashMap<TaskId, Task>>,
}

impl LocalOwnedTasks {
    pub(crate) fn bind(&self, task: Task) -> Notified {
        let notified = task.notified();
        self.tasks.borrow_mut().insert(task.id(), task);
        notified
    }

    pub(crate) fn notified(&self, id: TaskId) -> Option<Notified> {
        self.tasks.borrow().get(&id).map(Task::notified)
    }

    pub(crate) fn release(&self, id: TaskId) {
        let task = self.tasks.borrow_mut().remove(&id);
        // Future/output destructors and join wakers may spawn or schedule work.
        drop(task);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tasks.borrow().is_empty()
    }

    pub(crate) fn shutdown(&self) {
        let tasks = std::mem::take(&mut *self.tasks.borrow_mut());
        drop(tasks);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Schedule, new_task};
    use std::{
        cell::Cell,
        future::{pending, poll_fn},
        rc::Rc,
        sync::Arc,
        task::Poll,
    };

    struct Noop;
    impl Schedule for Noop {
        fn schedule(&self, _: TaskId) {}
    }

    struct DropProbe(Rc<Cell<usize>>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn runnable_references_do_not_transfer_cancellation_ownership() {
        let owned = LocalOwnedTasks::default();
        let drops = Rc::new(Cell::new(0));
        let probe = DropProbe(drops.clone());
        let (task, join) = new_task(
            async move {
                let _probe = probe;
                pending::<()>().await;
            },
            Arc::new(Noop),
        );
        let id = task.id();
        let queued = owned.bind(task);
        drop(queued);
        assert_eq!(drops.get(), 0);
        let queued = owned.notified(id).unwrap();
        owned.shutdown();
        assert!(join.is_finished());
        assert_eq!(drops.get(), 1);
        // The join and ownership references can disappear before the queued token.
        drop(join);
        assert!(queued.run());
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn polling_retains_membership_and_releases_all_borrows() {
        let owned = Rc::new(LocalOwnedTasks::default());
        let inspect = owned.clone();
        let (task, join) = new_task(
            poll_fn(move |_| {
                assert!(!inspect.is_empty());
                // Spawning while polling must be able to mutate task ownership.
                let (child, _) = new_task(pending::<()>(), Arc::new(Noop));
                drop(inspect.bind(child));
                Poll::Ready(())
            }),
            Arc::new(Noop),
        );
        let id = task.id();
        let queued = owned.bind(task);
        assert!(queued.run());
        owned.release(id);
        assert!(owned.notified(id).is_none());
        assert!(join.is_finished());
        owned.shutdown();
        assert!(owned.is_empty());
    }
}
