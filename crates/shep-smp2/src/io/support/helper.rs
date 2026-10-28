//! One bounded blocking worker per driver. No runtime dependency.
use super::activity::wake_safely;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex, mpsc},
    task::{Context, Poll, Waker},
    thread,
};

type Job = Box<dyn FnOnce() + Send>;
pub(crate) struct Helper {
    sender: Option<mpsc::SyncSender<Job>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Helper {
    pub(crate) fn new(queue: usize) -> io::Result<Self> {
        if queue == 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let (sender, receiver) = mpsc::sync_channel::<Job>(queue);
        let thread = thread::Builder::new()
            .name("shep-io-worker".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    job();
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }
    pub(crate) fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<ResultFuture<T>> {
        let shared = Arc::new(Mutex::new(ResultState {
            result: None,
            wake: None,
        }));
        let output = shared.clone();
        let job = Box::new(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err(io::Error::other("blocking worker panicked")));
            let wake = {
                let mut state = output.lock().unwrap();
                state.result = Some(result);
                state.wake.take()
            };
            if let Some(wake) = wake {
                wake_safely(wake);
            }
        });
        self.sender
            .as_ref()
            .unwrap()
            .try_send(job)
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => io::Error::from(io::ErrorKind::WouldBlock),
                mpsc::TrySendError::Disconnected(_) => io::Error::from(io::ErrorKind::BrokenPipe),
            })?;
        Ok(ResultFuture(shared))
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        self.sender.take();
        // A fallback for callers omitting explicit drain/shutdown. Jobs never
        // retain Helper, so destruction cannot run on its own worker thread.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
struct ResultState<T> {
    result: Option<io::Result<T>>,
    wake: Option<Waker>,
}
pub(crate) struct ResultFuture<T>(Arc<Mutex<ResultState<T>>>);
impl<T> Future for ResultFuture<T> {
    type Output = io::Result<T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let wake = cx.waker().clone();
        let mut state = self.0.lock().unwrap();
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }
        let old = state.wake.replace(wake);
        drop(state);
        drop(old);
        Poll::Pending
    }
}
impl<T> Drop for ResultFuture<T> {
    fn drop(&mut self) {
        let wake = self.0.lock().unwrap().wake.take();
        drop(wake);
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }
    #[test]
    fn worker_survives_job_panic() {
        let helper = Helper::new(1).unwrap();
        let mut failed = Box::pin(
            helper
                .run(|| -> io::Result<()> { panic!("helper work failed") })
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Poll::Ready(result) = poll(failed.as_mut()) {
                assert!(result.is_err());
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        // The worker remains usable after a job panic.
        let mut next = Box::pin(helper.run(|| Ok(42)).unwrap());
        loop {
            if let Poll::Ready(result) = poll(next.as_mut()) {
                assert_eq!(result.unwrap(), 42);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
