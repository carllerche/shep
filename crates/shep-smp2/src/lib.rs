//! Native single-core runtime: bounded task turns, io_uring, and timer deadlines.
//! This parallel implementation leaves shep-smp and its consumers unchanged.
//! Thread-per-core orchestration and networking are subsequent components.
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<shep_smp2::Runtime>();
//! ```
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<shep_smp2::Runtime>();
//! ```
#![forbid(unsafe_code)]

use shep_smp_exec::Executor;
pub use shep_smp_exec::{JoinError, JoinHandle, spawn};
mod park;
pub use park::{Park, Parking, WakeHandle};
pub use shep_smp_fs2::File;
pub use shep_smp_io2::{Buffer, Io};
use shep_smp_time::{Instant, Timer};
pub use shep_smp_time::{sleep, sleep_until, timeout};
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll, Waker},
};

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub task_budget: usize,
    pub max_in_flight: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            task_budget: 64,
            max_in_flight: 256,
        }
    }
}

/// One local executor/reactor/timer domain. Neither Send nor Sync.
/// Run one instance on each owning OS thread when building an SMP layer.
pub struct Runtime {
    executor: Executor,
    reactor: Parking,
    timer: Timer,
    io: Io,
    task_budget: usize,
    closed: bool,
}
impl Runtime {
    pub fn new(config: Config) -> io::Result<Self> {
        if config.task_budget == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero task budget",
            ));
        }
        let (reactor, io) = Parking::new(config.max_in_flight)?;
        let executor = Executor::new(reactor.wake_handle().waker());
        Ok(Self {
            executor,
            reactor,
            timer: Timer::new(Instant::now()),
            io,
            task_budget: config.task_budget,
            closed: false,
        })
    }
    pub fn io(&self) -> Io {
        self.io.clone()
    }

    /// Queue a task without polling it. Background tasks survive block_on calls
    /// and are cancelled on shutdown. Futures and results stay on this thread.
    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output>
    where
        F::Output: 'static,
    {
        self.timer.with(|| self.executor.spawn(future))
    }

    /// Run a root task while driving local tasks, file I/O, and timers.
    /// Root tasks are 'static so task ownership remains explicit and safe.
    /// Task panics are returned as I/O errors with a JoinError cause.
    pub fn block_on<F: Future + 'static>(&mut self, future: F) -> io::Result<F::Output>
    where
        F::Output: 'static,
    {
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "runtime closed"));
        }
        // Keep owner routing active during CQEs, timers and parking preparation,
        // not just run_ready. Poll recursion has its own independent guard.
        let _core = self.executor.enter();
        let mut root = self.spawn(future);
        loop {
            self.reactor.poll()?;
            self.timer.advance(Instant::now());
            self.timer
                .with(|| self.executor.run_ready(self.task_budget));
            self.reactor.poll()?;
            self.timer.advance(Instant::now());
            if root.is_finished() {
                return match Pin::new(&mut root).poll(&mut Context::from_waker(Waker::noop())) {
                    Poll::Ready(result) => result.map_err(io::Error::other),
                    Poll::Pending => unreachable!("finished root has an output"),
                };
            }
            if self.executor.has_ready() || self.io.has_pending() {
                continue;
            }
            let park = self.reactor.prepare_park()?;
            // This check MUST follow publishing PARKING in prepare_park.
            // The notification RMW either synchronizes prior inbox publication,
            // or claims an eventfd write if it observes this parking attempt.
            self.timer.advance(Instant::now());
            if self.executor.has_ready() || self.io.has_pending() {
                drop(park);
                continue;
            }
            let deadline = self.timer.next_wake().map(Instant::into_std);
            park.wait(deadline)?;
        }
    }

    /// Stop task admission, cancel consumers, then retire all kernel resources.
    pub fn shutdown(&mut self) -> io::Result<()> {
        let _core = self.executor.enter();
        self.closed = true;
        self.timer.with(|| self.executor.shutdown());
        self.reactor.shutdown()
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let (reactor, io) = Parking::new(8).unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake = Waker::from(Arc::new(CountDriver {
            wakes: wakes.clone(),
            inner: reactor.wake_handle().waker(),
        }));
        let mut runtime = Runtime {
            executor: Executor::new(wake),
            reactor,
            timer: Timer::new(Instant::now()),
            io,
            task_budget: 8,
            closed: false,
        };
        let file = File::from_std(runtime.io(), tempfile::tempfile().unwrap());
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
