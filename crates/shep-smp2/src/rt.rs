//! The single-core runtime: one construct owning tasks, timers and the I/O
//! driver, entered through one scoped thread-local context.
//!
//! Runtime owns the scheduler core (local run queue and remote receiver) and
//! resource driver. Handle retains task cancellation ownership, submission
//! state, and resource registration handles. Entering a runtime exposes those
//! capabilities; executing a scheduler turn additionally lends its core to the
//! thread-local context. No context or task-collection borrow spans user code.
#![forbid(unsafe_code)]

pub(crate) mod context;
mod driver;
mod event;
mod handle;
mod remote;
mod scheduler;
mod shared;
mod state;

pub use context::EnterGuard;
#[doc(hidden)]
pub use driver::{Driver, Park, WakeHandle};
pub use handle::Handle;

use crate::task::JoinHandle;
use crate::time::Timer;
use shared::Shared;
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll, Waker},
};

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// SQ entries (1..=32768), rounded up to a power of two. Defaults to 256.
    /// Outstanding operations and retained completions grow independently.
    pub submission_queue_capacity: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            submission_queue_capacity: 256,
        }
    }
}

/// One local task/timer/I/O domain. Neither Send nor Sync.
/// Run one instance on each owning OS thread when building an SMP layer.
pub struct Runtime {
    handle: Handle,
    scheduler: scheduler::Scheduler,
    driver: Driver,
    closed: bool,
}
impl Runtime {
    pub fn new(config: Config) -> io::Result<Self> {
        Self::build(config, None, |wake| wake)
    }

    /// Build a runtime around a timer whose epoch the caller controls, so
    /// tests can advance it through synthetic instants, and wrap the waker
    /// that foreign task wakes use to interrupt the driver.
    #[doc(hidden)]
    pub fn build(
        config: Config,
        timer: Option<Timer>,
        wake: impl FnOnce(Waker) -> Waker,
    ) -> io::Result<Self> {
        let (mut driver, io) = Driver::new(config.submission_queue_capacity)?;
        if let Some(timer) = timer {
            driver.timer = timer;
        }
        let (send, receive) = std::sync::mpsc::channel();
        let remote = std::sync::Arc::new(remote::Remote::new(
            send,
            wake(driver.wake_handle().waker()),
        ));
        let shared = Shared::new(remote, io, driver.timer.handle());
        Ok(Self {
            handle: Handle::new(shared),
            scheduler: scheduler::Scheduler::new(receive),
            driver,
            closed: false,
        })
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    pub fn io(&self) -> crate::io::Io {
        self.handle.io()
    }

    #[doc(hidden)]
    pub fn timer(&self) -> &Timer {
        &self.driver.timer
    }

    /// Run one task turn without driving I/O or timers.
    ///
    /// Panics if called from a task or while another runtime is entered.
    #[doc(hidden)]
    pub fn run_ready(&self) -> usize {
        let _enter = context::try_enter(&self.handle);
        self.scheduler.run_ready(&self.handle)
    }

    #[doc(hidden)]
    pub fn has_ready(&self) -> bool {
        self.scheduler.has_ready(self.handle.shared())
    }

    /// Whether every task has finished or been cancelled.
    #[doc(hidden)]
    pub fn is_empty(&self) -> bool {
        self.handle.shared().is_empty()
    }

    /// Enter this runtime's context so `spawn`, timers and other ambient
    /// operations find it. `block_on`, `spawn` and `shutdown` enter on their
    /// own; they reuse this context when it is already entered.
    ///
    /// # Panics
    ///
    /// Panics if any runtime is already entered on this thread.
    pub fn enter(&self) -> EnterGuard {
        context::enter(&self.handle)
    }

    /// Queue a task without polling it. Background tasks survive block_on calls
    /// and are cancelled on shutdown. Futures and results stay on this thread.
    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output>
    where
        F::Output: 'static,
    {
        let _enter = context::try_enter(&self.handle);
        self.handle.spawn(future)
    }

    /// Run a root task while driving local tasks, file I/O, and timers.
    /// Root tasks are 'static so task ownership remains explicit and safe.
    /// Task panics are returned as I/O errors with a JoinError cause.
    ///
    /// # Panics
    ///
    /// Panics when called from a task or while another runtime is entered.
    pub fn block_on<F: Future + 'static>(&mut self, future: F) -> io::Result<F::Output>
    where
        F::Output: 'static,
    {
        assert!(
            context::current_task_id().is_none(),
            "cannot block_on from within a task"
        );
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "runtime closed"));
        }
        // Stay entered during CQEs, timers and parking preparation, not just
        // task turns, so wakes route locally and destructors can spawn.
        let _enter = context::try_enter(&self.handle);
        let shared = self.handle.shared();
        let mut root = shared.spawn(future);
        loop {
            self.driver.tick()?;
            self.scheduler.run_ready(&self.handle);
            self.driver.tick()?;
            if root.is_finished() {
                return match Pin::new(&mut root).poll(&mut Context::from_waker(Waker::noop())) {
                    Poll::Ready(result) => result.map_err(io::Error::other),
                    Poll::Pending => unreachable!("finished root has an output"),
                };
            }
            self.driver
                .park_if_idle(|| self.scheduler.has_ready(shared))?;
        }
    }

    /// Stop task admission, cancel consumers, then retire all kernel resources.
    ///
    /// Callers should stop application producers and await their operations before
    /// shutdown. Explicitly cancel indefinite operations and await their terminal
    /// completions; dropping their futures alone is not sufficient. The runtime
    /// cancels its own parking watch. Shutdown does not flush application buffers,
    /// commit transactions, or imply filesystem durability.
    ///
    /// If shutdown returns an error, outstanding inputs remain owned. Driver
    /// destruction attempts cancellation and waits for terminal completions as a
    /// memory-safety fallback; a fatal teardown error or panic before retirement
    /// aborts the entire process. Callers must not rely on this fallback to finish
    /// application work successfully.
    ///
    /// # Panics
    ///
    /// Panics while another runtime is entered on this thread.
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.closed = true;
        if !self.handle.shared().is_closed() {
            let _enter = context::try_enter(&self.handle);
            self.handle.shared().shutdown();
        }
        self.scheduler.shutdown();
        self.driver.shutdown()
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        // Unwinding out of another runtime's task cannot enter this one, and a
        // second panic would abort. Cancel without installing another context.
        if std::thread::panicking() && context::entered_elsewhere(&self.handle) {
            self.closed = true;
            self.handle.shared().shutdown();
            self.scheduler.shutdown();
            let _ = self.driver.shutdown();
            return;
        }
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Buffer, File, sleep, spawn};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::task::Wake;
    struct CountDriver {
        wakes: Arc<AtomicUsize>,
        inner: Waker,
    }
    impl Wake for CountDriver {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::Relaxed);
            self.inner.wake_by_ref();
        }
    }
    #[test]
    fn local_io_timer_and_channel_completions_never_invoke_the_driver_waker() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let mut runtime = Runtime::build(
            Config {
                submission_queue_capacity: 8,
            },
            None,
            |inner| {
                Waker::from(Arc::new(CountDriver {
                    wakes: wakes.clone(),
                    inner,
                }))
            },
        )
        .unwrap();
        let file = runtime
            .block_on(File::from_std(tempfile::tempfile().unwrap()))
            .unwrap()
            .unwrap();
        // Import uses the foreign metadata worker. Its result can be observed
        // before it finishes notifying, so fence that job before measuring
        // notifications caused solely by local I/O, timers and channels.
        let (send, recv) = std::sync::mpsc::channel();
        drop(
            runtime
                .io()
                .shared
                .worker
                .run(move || {
                    send.send(()).unwrap();
                    Ok(())
                })
                .unwrap(),
        );
        recv.recv().unwrap();
        wakes.store(0, Ordering::Relaxed);
        runtime
            .block_on(async move {
                let (send, recv) = tokio::sync::oneshot::channel();
                spawn(async move {
                    sleep(std::time::Duration::from_millis(2)).await;
                    send.send(()).unwrap();
                });
                recv.await.unwrap();
                let (result, buffer) = file.write_at(Buffer::new(4096, 4096).unwrap(), 0).await;
                assert_eq!(result.unwrap(), 4096);
                file.sync_data().await.unwrap();
                let (result, _) = file.read_at(buffer, 0).await;
                assert_eq!(result.unwrap(), 4096);
            })
            .unwrap();
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        runtime.shutdown().unwrap();
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
    }
}
