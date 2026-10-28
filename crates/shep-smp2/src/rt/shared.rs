// Adapted from Monoio's runtime Context and scheduler TaskQueue, commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Licensed under MIT.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::task::Waker;

use super::scheduler::Scheduler;
use crate::io::Io;
use crate::task::{JoinHandle, Task, TaskId, new_task};
use crate::time::Timer;

/// Runtime state reachable from its handle: tasks, timers and I/O admission.
pub(crate) struct Shared {
    tasks: RefCell<HashMap<TaskId, Task>>,
    ready: RefCell<VecDeque<TaskId>>,
    remote: Receiver<TaskId>,
    pub(super) scheduler: Arc<Scheduler>,
    pub(crate) timer: Timer,
    pub(crate) io: Io,
}

impl Shared {
    pub(super) fn new(wake: Waker, timer: Timer, io: Io) -> Self {
        let (send, remote) = mpsc::channel();
        Self {
            tasks: RefCell::default(),
            ready: RefCell::default(),
            remote,
            scheduler: Arc::new(Scheduler::new(send, wake)),
            timer,
            io,
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        self.scheduler.closed.load(Ordering::Acquire)
    }

    /// Queue a future without polling it. After shutdown, the future is
    /// dropped and its join is cancelled.
    pub(super) fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
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

    pub(super) fn schedule_local(&self, id: TaskId) {
        self.ready.borrow_mut().push_back(id);
    }

    /// A task has at most one queued notification, so this is bounded by the
    /// number of live tasks even during a foreign wake storm.
    fn import_remote(&self) {
        let mut ready = self.ready.borrow_mut();
        while let Ok(id) = self.remote.try_recv() {
            ready.push_back(id);
        }
    }

    /// Poll the tasks that are ready when the turn starts, returning the number
    /// polled. Self-wakes and children spawned during the turn wait for the next
    /// one, so the runtime services I/O and timers between turns. Stale
    /// notifications are discarded without polling a task.
    pub(super) fn run_ready(&self) -> usize {
        if self.is_closed() {
            return 0;
        }
        self.import_remote();
        let turn = self.ready.borrow().len();
        let mut polled = 0;
        for _ in 0..turn {
            let Some(id) = self.ready.borrow_mut().pop_front() else {
                break;
            };
            // Release the registry borrow before polling: user code can spawn
            // children, drop captures, or wake other tasks in this runtime.
            let task = self.tasks.borrow_mut().remove(&id);
            if let Some(mut task) = task {
                polled += 1;
                if !task.run() {
                    self.tasks.borrow_mut().insert(id, task);
                }
            }
        }
        polled
    }

    /// Whether a turn can make progress, including on foreign notifications.
    /// A stale notification can make this true until a turn discards it.
    pub(super) fn has_ready(&self) -> bool {
        if self.is_closed() {
            return false;
        }
        self.import_remote();
        !self.ready.borrow().is_empty()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.tasks.borrow().is_empty()
    }

    /// Close admission and cancel every pending task. Idempotent. Must run in
    /// this runtime's context: future destructors may spawn, and that work is
    /// immediately cancelled.
    pub(super) fn shutdown(&self) {
        if self.scheduler.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.ready.borrow_mut().clear();
        // Close first, then release the registry borrow before user destructors
        // run. They may spawn work, which must see the closed admission gate.
        let tasks = std::mem::take(&mut *self.tasks.borrow_mut());
        drop(tasks);
        while self.remote.try_recv().is_ok() {}
    }
}

#[cfg(test)]
mod tests {
    use crate::Runtime;
    use std::{
        cell::RefCell,
        future::poll_fn,
        rc::Rc,
        sync::Arc,
        task::{Poll, Wake, Waker},
    };
    struct UnexpectedDriverWake;
    impl Wake for UnexpectedDriverWake {
        fn wake(self: Arc<Self>) {
            panic!("owner wake reached driver");
        }
    }
    #[test]
    fn entered_runtime_routes_owner_wakes_directly_to_its_ready_queue() {
        let rt = Runtime::build(Default::default(), None, |_| {
            Waker::from(Arc::new(UnexpectedDriverWake))
        })
        .unwrap();
        let shared = rt.handle().shared();
        let saved = Rc::new(RefCell::new(None));
        let capture = saved.clone();
        let _enter = rt.enter();
        rt.spawn(poll_fn(move |cx| {
            *capture.borrow_mut() = Some(cx.waker().clone());
            Poll::<()>::Pending
        }));
        assert_eq!(shared.run_ready(), 1);
        let wake: Waker = saved.borrow_mut().take().unwrap();
        wake.wake_by_ref();
        assert_eq!(shared.ready.borrow().len(), 1);
        assert!(shared.remote.try_recv().is_err());
        assert_eq!(shared.run_ready(), 1);
    }
}
