// Adapted from Monoio's runtime Context and scheduler TaskQueue; see docs/task/EXECUTOR-FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Licensed under MIT.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::task::Waker;

use crate::task::{JoinHandle, Task, TaskId, new_task};

use crate::task::scheduler::Scheduler;

thread_local! {
    static CURRENT: RefCell<Option<Weak<Context>>> = const { RefCell::new(None) };
    // Entering the runtime context is distinct from polling tasks. Driver and
    // timer work runs in the context too; only recursive task polling is banned.
    static POLLING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct Context {
    pub(crate) tasks: RefCell<HashMap<TaskId, Task>>,
    pub(crate) ready: RefCell<VecDeque<TaskId>>,
    pub(crate) scheduler: Arc<Scheduler>,
    remote: Receiver<TaskId>,
}

impl Context {
    pub(crate) fn new(wake: Waker) -> Self {
        let (send, remote) = mpsc::channel();
        Self {
            tasks: RefCell::default(),
            ready: RefCell::default(),
            scheduler: Arc::new(Scheduler::new(send, wake)),
            remote,
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.scheduler.closed.load(Ordering::Acquire)
    }

    pub(crate) fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (task, join) = new_task(future, self.scheduler.clone());
        if self.is_closed() {
            drop(task);
        } else {
            let id = task.id();
            self.tasks.borrow_mut().insert(id, task);
            self.ready.borrow_mut().push_back(id);
        }
        join
    }

    pub(crate) fn import_remote(&self, budget: usize) {
        let mut ready = self.ready.borrow_mut();
        for _ in 0..budget {
            let Ok(id) = self.remote.try_recv() else {
                break;
            };
            ready.push_back(id);
        }
    }

    pub(crate) fn shutdown(self: &Rc<Self>) {
        if self.scheduler.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let _scope = Scope::enter(self);
        self.ready.borrow_mut().clear();
        // Close first, then release the registry borrow before user destructors
        // run. They may spawn work, which must see the closed admission gate.
        let tasks = std::mem::take(&mut *self.tasks.borrow_mut());
        drop(tasks);
        while self.remote.try_recv().is_ok() {}
    }
}

/// Clone out of TLS before calling user code or accessing queues. In particular,
/// no RefCell borrow survives a wake callback or a future's destructor.
pub(crate) fn current() -> Option<Rc<Context>> {
    CURRENT
        .try_with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade))
        .ok()
        .flatten()
}

/// Restore the previous context even on unwind. Nested shutdown scopes can
/// occur when a task owns another executor; nested execution is rejected.
pub(crate) struct Scope {
    previous: Option<Weak<Context>>,
    polling: bool,
}

impl Scope {
    pub(crate) fn enter(context: &Rc<Context>) -> Self {
        let previous = CURRENT.with(|slot| slot.replace(Some(Rc::downgrade(context))));
        Self {
            previous,
            polling: false,
        }
    }
    pub(crate) fn poll(context: &Rc<Context>) -> Self {
        assert!(!POLLING.with(Cell::get), "cannot run a nested executor");
        let mut scope = Self::enter(context);
        POLLING.with(|polling| polling.set(true));
        scope.polling = true;
        scope
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.replace(self.previous.take()));
        if self.polling {
            POLLING.with(|polling| polling.set(false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Executor;
    use std::{
        future::poll_fn,
        task::{Poll, Wake},
    };
    struct UnexpectedDriverWake;
    impl Wake for UnexpectedDriverWake {
        fn wake(self: Arc<Self>) {
            panic!("owner wake reached driver");
        }
    }
    fn executor_with_waker() -> (Executor, Waker) {
        let mut exec = Executor::new(Waker::from(Arc::new(UnexpectedDriverWake)));
        let saved = Rc::new(RefCell::new(None));
        let capture = saved.clone();
        exec.spawn(poll_fn(move |cx| {
            *capture.borrow_mut() = Some(cx.waker().clone());
            Poll::<()>::Pending
        }));
        exec.run_ready(1);
        let wake = saved.borrow_mut().take().unwrap();
        (exec, wake)
    }
    #[test]
    fn entered_driver_context_routes_directly_to_its_ready_queue() {
        let (mut exec, wake) = executor_with_waker();
        let _core = exec.enter();
        wake.wake_by_ref();
        assert_eq!(exec.context.ready.borrow().len(), 1);
        assert!(exec.context.remote.try_recv().is_err());
        assert_eq!(exec.run_ready(1), 1); // Entered does not mean recursively polling.
    }
    #[test]
    fn nested_context_checks_identity_and_restores_the_outer_owner() {
        let (mut owner, wake) = executor_with_waker();
        let other = Executor::new(Waker::noop().clone());
        let _outer = owner.enter();
        {
            let _inner = other.enter();
            assert!(Rc::ptr_eq(&current().unwrap(), &other.context));
            wake.wake_by_ref();
            assert!(owner.context.ready.borrow().is_empty());
            let id = owner.context.remote.try_recv().unwrap();
            owner.context.ready.borrow_mut().push_back(id);
        }
        assert!(Rc::ptr_eq(&current().unwrap(), &owner.context));
        owner.run_ready(1);
        wake.wake_by_ref();
        assert_eq!(owner.context.ready.borrow().len(), 1);
        assert!(owner.context.remote.try_recv().is_err());
    }
    #[test]
    fn entered_context_restores_on_unwind_and_does_not_keep_tasks_alive() {
        let (owner, _) = executor_with_waker();
        let outer = owner.enter();
        let inner = Executor::new(Waker::noop().clone());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _scope = inner.enter();
                panic!("scope unwind");
            }))
            .is_err()
        );
        assert!(Rc::ptr_eq(&current().unwrap(), &owner.context));
        drop(owner);
        assert!(current().is_none());
        drop(outer);
        assert!(current().is_none());
    }
}
