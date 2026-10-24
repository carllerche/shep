//! Safe file futures, admission and completion delivery.
#![forbid(unsafe_code)]

use crate::core;
use crate::{Buffer, WaitOutcome, WakeHandle};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::poll_fn,
    io,
    rc::Rc,
    sync::Arc,
    task::{Context, Poll, Waker},
};

type Outcome = (io::Result<usize>, core::Operation);
struct Shared {
    operations: core::Operations,
    closed: Cell<bool>,
    failed: Cell<bool>,
    waiters: RefCell<BTreeMap<u64, Waker>>,
    next_waiter: Cell<u64>,
}
impl Shared {
    fn wake_waiters(&self) {
        let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
        for (_, waker) in waiters {
            wake(waker);
        }
    }
    fn close(&self) {
        self.closed.set(true);
        self.operations.close();
        self.wake_waiters();
    }
}

/// Local submission handle. Must be paired with a runtime driving its Reactor.
#[derive(Clone)]
pub struct Io {
    shared: Rc<Shared>,
}
impl Io {
    /// Bind an already-open descriptor. Opening files is a separate metadata
    /// concern: this API never performs a blocking open on the reactor thread.
    pub fn file(&self, file: std::fs::File) -> File {
        File {
            io: self.clone(),
            file: Arc::new(file),
        }
    }
    pub fn in_flight(&self) -> usize {
        self.shared.operations.in_flight()
    }
    /// Published SQEs waiting for the reactor to submit them to the kernel.
    pub fn has_pending(&self) -> bool {
        self.shared.operations.has_pending()
    }
    fn operation(&self, operation: core::Operation) -> Operation {
        let id = self.shared.next_waiter.get();
        self.shared
            .next_waiter
            .set(id.checked_add(1).expect("I/O waiter IDs exhausted"));
        Operation {
            shared: self.shared.clone(),
            operation: Some(operation),
            token: None,
            done: false,
            id,
        }
    }
}

/// Runtime-owned driver; processing completions runs no application futures.
pub struct Reactor {
    kernel: core::Reactor,
    shared: Rc<Shared>,
}
impl Reactor {
    pub fn new(capacity: usize) -> io::Result<(Self, Io)> {
        let kernel = core::Reactor::new(capacity)?;
        let shared = Rc::new(Shared {
            operations: kernel.operations(),
            closed: Cell::new(false),
            failed: Cell::new(false),
            waiters: RefCell::default(),
            next_waiter: Cell::new(0),
        });
        Ok((
            Self {
                kernel,
                shared: shared.clone(),
            },
            Io { shared },
        ))
    }
    pub fn wake_handle(&self) -> WakeHandle {
        self.kernel.wake_handle()
    }

    /// Submit queued work and deliver a CQ snapshot. Returns retired data ops.
    /// Each request owns its resources even if its result waiter was dropped.
    pub fn poll(&mut self) -> io::Result<usize> {
        if self.shared.failed.get() {
            return Err(io::Error::other("reactor has failed"));
        }
        let progress = match self.kernel.tick() {
            Ok(progress) => progress,
            Err(failure) => {
                self.notify(failure.progress);
                self.fail();
                return Err(failure.error);
            }
        };
        let retired = progress.retired;
        self.notify(progress);
        if retired != 0 {
            self.shared.wake_waiters();
        }
        Ok(retired)
    }
    fn notify(&self, progress: core::Progress) {
        for waker in progress.wakes {
            wake(waker);
        }
    }
    fn fail(&self) {
        self.shared.failed.set(true);
        self.shared.close();
        for waker in self.shared.operations.pending_wakers() {
            wake(waker);
        }
    }
    /// Caller rechecks executor readiness and Io::has_pending before waiting.
    pub fn prepare_park(&mut self) -> io::Result<Park<'_>> {
        self.kernel.prepare_park().map(|inner| Park { inner })
    }

    /// Close admission, complete accepted I/O and retire the wakeup descriptor.
    /// The caller first stops/cancels task consumers. Completion does not imply
    /// durability: applications must await sync_data/sync_all explicitly.
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.shared.close();
        // Kernel progress is not guaranteed after a terminal failure. Do not
        // wait on facade counters here. Kernel Drop independently drains or
        // retains its still-visible resources.
        if self.shared.failed.get() {
            return Err(io::Error::other("reactor has failed"));
        }
        while self.shared.operations.in_flight() != 0 {
            self.poll()?;
            if self.shared.operations.in_flight() != 0 {
                self.kernel.prepare_park()?.wait(None)?;
            }
        }
        match self.kernel.shutdown_retained() {
            Ok(progress) => {
                self.notify(progress);
                Ok(())
            }
            Err(failure) => {
                self.notify(failure.progress);
                self.fail();
                Err(failure.error)
            }
        }
    }
}
impl Drop for Reactor {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Prepared kernel wait. Recheck executor readiness and queued I/O before wait.
#[must_use = "recheck ready work and deadlines, then wait or abandon this park"]
pub struct Park<'a> {
    inner: core::Park<'a>,
}
impl Park<'_> {
    pub fn wait(self, deadline: Option<std::time::Instant>) -> io::Result<WaitOutcome> {
        self.inner.wait(deadline)
    }
}

struct Operation {
    shared: Rc<Shared>,
    // Only unaccepted requests live here. Once admitted, the slab owns all
    // operation/completion state and this handle keeps just its token.
    operation: Option<core::Operation>,
    token: Option<core::OperationId>,
    done: bool,
    id: u64,
}
impl Operation {
    fn take_result(&mut self) -> Option<Outcome> {
        let token = self.token?;
        let completion = self.shared.operations.take(token)?;
        debug_assert_eq!(completion.id, token);
        self.token = None;
        self.done = true;
        // Driver interest ended with the terminal CQE. Taking the result removes
        // the entry; this handle releases its token before any destructor runs.
        let core::Completion {
            result, operation, ..
        } = completion;
        Some((result, operation))
    }
    fn poll_operation(&mut self, cx: &mut Context<'_>) -> Poll<Outcome> {
        assert!(!self.done, "I/O operation polled after completion");
        let waker = cx.waker().clone(); // No arena borrow across user callbacks.
        if let Some(result) = self.take_result() {
            return Poll::Ready(result);
        }
        if let Some(token) = self.token {
            assert!(
                !self.shared.failed.get() && !self.shared.operations.is_failed(),
                "io_uring reactor failed with unresolved I/O"
            );
            let old = self.shared.operations.wait(token, waker);
            drop(old);
        } else if self.shared.closed.get() {
            self.done = true;
            return Poll::Ready((
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "I/O admission closed",
                )),
                self.operation.take().unwrap(),
            ));
        } else {
            let operation = self.operation.take().expect("unaccepted operation");
            match self.shared.operations.queue(operation) {
                Ok(token) => {
                    self.token = Some(token);
                    let old = self.shared.operations.wait(token, waker);
                    drop(old);
                    // Install token/waker before releasing an admission waker
                    // whose destructor may reenter and complete this operation.
                    let old = self.shared.waiters.borrow_mut().remove(&self.id);
                    drop(old);
                }
                Err(core::Rejected { error, operation })
                    if error.kind() == io::ErrorKind::WouldBlock =>
                {
                    self.operation = Some(operation);
                    let old = self.shared.waiters.borrow_mut().insert(self.id, waker);
                    drop(old);
                    return Poll::Pending;
                }
                Err(core::Rejected { error, operation }) => {
                    self.done = true;
                    return Poll::Ready((Err(error), operation));
                }
            }
        }
        match self.take_result() {
            Some(result) => Poll::Ready(result),
            None => Poll::Pending,
        }
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        let admission = self.shared.waiters.borrow_mut().remove(&self.id);
        if let Some(token) = self.token.take() {
            self.shared.operations.abandon(token);
        }
        drop(admission);
    }
}

/// A local cloneable file handle with explicit positional, owned-buffer I/O.
/// Independent operations are unordered and may complete short.
#[derive(Clone)]
pub struct File {
    io: Io,
    file: Arc<std::fs::File>,
}
impl File {
    pub async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        let mut operation = self.io.operation(core::Operation::Read {
            file: self.file.clone(),
            buffer,
            offset,
        });
        let (result, operation) = poll_fn(|cx| operation.poll_operation(cx)).await;
        (result, operation.into_buffer().expect("read buffer"))
    }
    pub async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        let mut operation = self.io.operation(core::Operation::Write {
            file: self.file.clone(),
            buffer,
            offset,
        });
        let (result, operation) = poll_fn(|cx| operation.poll_operation(cx)).await;
        (result, operation.into_buffer().expect("write buffer"))
    }
    pub async fn sync_data(&self) -> io::Result<()> {
        self.sync(true).await
    }
    pub async fn sync_all(&self) -> io::Result<()> {
        self.sync(false).await
    }
    async fn sync(&self, data_only: bool) -> io::Result<()> {
        let mut operation = self.io.operation(core::Operation::Sync {
            file: self.file.clone(),
            data_only,
        });
        poll_fn(|cx| operation.poll_operation(cx))
            .await
            .0
            .map(|_| ())
    }
}

// User-defined wakers may panic. A bad callback must not strand other retired
// CQEs or turn resource-draining Drop into a second unwinding panic.
fn wake(waker: Waker) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake()));
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn operation() -> core::Operation {
        core::Operation::Sync {
            file: Arc::new(std::fs::File::open("/dev/null").unwrap()),
            data_only: true,
        }
    }
    fn poll(f: &mut Operation) -> Poll<Outcome> {
        f.poll_operation(&mut Context::from_waker(Waker::noop()))
    }
    fn finish_kernel(reactor: &mut Reactor) -> core::Progress {
        let mut progress = core::Progress::default();
        let end = Instant::now() + Duration::from_secs(2);
        while reactor.shared.operations.in_flight() != 0 {
            // Don't dispatch waiters: test repolling before an old admission
            // waker has been taken, including arbitrary Drop callbacks.
            let mut turn = reactor.kernel.tick().unwrap();
            progress.retired += turn.retired;
            progress.wakes.append(&mut turn.wakes);
            assert!(Instant::now() < end);
        }
        progress
    }
    #[test]
    fn cancelled_admission_waiters_are_removed_and_entries_are_reclaimed() {
        let (mut reactor, io) = Reactor::new(1).unwrap();
        let mut first = io.operation(operation());
        assert!(poll(&mut first).is_pending());
        for _ in 0..100 {
            let mut waiting = io.operation(operation());
            assert!(poll(&mut waiting).is_pending());
            assert_eq!(io.shared.waiters.borrow().len(), 1);
            drop(waiting);
            assert!(io.shared.waiters.borrow().is_empty());
        }
        assert_eq!(io.shared.operations.live_entries(), 1);
        finish_kernel(&mut reactor);
        assert!(poll(&mut first).is_ready());
        assert_eq!(io.shared.operations.live_entries(), 0);
    }
    #[test]
    fn closing_admission_wakes_waiters_and_preserves_accepted_work() {
        let (mut reactor, io) = Reactor::new(1).unwrap();
        let mut first = io.operation(operation());
        assert!(poll(&mut first).is_pending());
        let mut waiting = io.operation(operation());
        assert!(poll(&mut waiting).is_pending());
        io.shared.close();
        assert!(io.shared.waiters.borrow().is_empty());
        let Poll::Ready((result, _)) = poll(&mut waiting) else {
            panic!("closed waiter");
        };
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        reactor.shutdown().unwrap();
        assert!(poll(&mut first).is_ready());
    }
    #[test]
    fn reentrant_wake_can_reuse_a_released_slot_without_misdirecting_notifications() {
        check(false);
        check(true);
    }
    fn check(abandon: bool) {
        thread_local! { static CALLBACK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None); }
        struct OnWake;
        impl std::task::Wake for OnWake {
            fn wake(self: Arc<Self>) {
                let callback = CALLBACK.with(|slot| slot.borrow_mut().take());
                if let Some(callback) = callback {
                    callback();
                }
            }
        }
        #[derive(Default)]
        struct Count(std::sync::atomic::AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let (mut reactor, io) = Reactor::new(2).unwrap();
        let mut first = io.operation(operation());
        let waker = Waker::from(Arc::new(OnWake));
        assert!(
            first
                .poll_operation(&mut Context::from_waker(&waker))
                .is_pending()
        );
        let mut second = io.operation(operation());
        assert!(poll(&mut second).is_pending());
        let second_token = second.token.unwrap();
        // Establish both terminal CQEs before dispatching this notification batch.
        let progress = finish_kernel(&mut reactor);
        let next = Rc::new(RefCell::new(None));
        let next_slot = next.clone();
        let count = Arc::new(Count::default());
        let count_waker = Waker::from(count.clone());
        let callback_io = io.clone();
        let callback_finished = Rc::new(Cell::new(false));
        let finished = callback_finished.clone();
        CALLBACK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                if abandon {
                    drop(second);
                } else {
                    assert!(poll(&mut second).is_ready());
                    assert!(second.token.is_none());
                }
                let mut third = callback_io.operation(operation());
                assert!(
                    third
                        .poll_operation(&mut Context::from_waker(&count_waker))
                        .is_pending()
                );
                assert_eq!(third.token.unwrap(), second_token);
                *next_slot.borrow_mut() = Some(third);
                finished.set(true);
            }));
        });
        reactor.notify(progress);
        assert!(callback_finished.get()); // Waker panics are caught by the facade.
        assert_eq!(count.0.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(io.shared.operations.live_entries(), 2); // First + third, not second.
        assert!(poll(&mut first).is_ready());
        reactor.shutdown().unwrap();
        assert_eq!(count.0.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(poll(next.borrow_mut().as_mut().unwrap()).is_ready());
        reactor.poll().unwrap();
        assert_eq!(io.shared.operations.live_entries(), 0);
    }
    #[test]
    fn completed_values_can_be_taken_after_the_reactor_is_dropped() {
        let (mut reactor, io) = Reactor::new(1).unwrap();
        let mut future = io.operation(operation());
        assert!(poll(&mut future).is_pending());
        reactor.shutdown().unwrap();
        drop(reactor);
        assert!(poll(&mut future).is_ready());
        assert!(future.token.is_none());
        // Both parties have finished, so no empty entry or later driver turn is needed.
        assert_eq!(io.shared.operations.live_entries(), 0);
        drop(future);
        drop(io);
    }
    #[test]
    fn reentrant_admission_waker_drop_can_complete_the_new_operation() {
        thread_local! { static CALLBACK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None); }
        struct OnDrop;
        #[allow(clippy::manual_noop_waker)]
        impl std::task::Wake for OnDrop {
            fn wake(self: Arc<Self>) {}
        }
        impl Drop for OnDrop {
            fn drop(&mut self) {
                let callback = CALLBACK.with(|slot| slot.borrow_mut().take());
                if let Some(callback) = callback {
                    callback();
                }
            }
        }
        let (reactor, io) = Reactor::new(1).unwrap();
        let driver = Rc::new(RefCell::new(reactor));
        let mut first = io.operation(operation());
        assert!(poll(&mut first).is_pending());
        let mut waiting = io.operation(operation());
        let waker = Waker::from(Arc::new(OnDrop));
        assert!(
            waiting
                .poll_operation(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(waker);
        finish_kernel(&mut driver.borrow_mut());
        let callback_driver = driver.clone();
        CALLBACK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                finish_kernel(&mut callback_driver.borrow_mut());
            }))
        });
        assert!(poll(&mut waiting).is_ready());
        assert!(poll(&mut first).is_ready());
        driver.borrow_mut().poll().unwrap();
        assert_eq!(io.shared.operations.live_entries(), 0);
    }
}
