// Adapted from Monoio's runtime Context and scheduler TaskQueue; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Licensed under MIT.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::task::Waker;

use shep_smp_task::{JoinHandle, Task, TaskId, new_task};

use crate::scheduler::Scheduler;

thread_local! {
    static CURRENT: RefCell<Option<Weak<Context>>> = const { RefCell::new(None) };
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
}

impl Scope {
    pub(crate) fn enter(context: &Rc<Context>) -> Self {
        let previous = CURRENT.with(|slot| slot.replace(Some(Rc::downgrade(context))));
        Self { previous }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.replace(self.previous.take()));
    }
}
