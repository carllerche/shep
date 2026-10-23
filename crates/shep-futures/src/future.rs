//! Futures: running many at once, sharing one result, catching panics.

use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, UnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

mod shared;

pub use shared::Shared;

/// A boxed future that need not be `Send`.
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub trait FutureExt: Future {
    /// Boxes the future, erasing its type.
    fn boxed_local<'a>(self) -> LocalBoxFuture<'a, Self::Output>
    where
        Self: Sized + 'a,
    {
        Box::pin(self)
    }

    /// A future any number of clones can await, each getting a clone of the
    /// output. The future runs once, driven by whichever clone is polled.
    fn shared(self) -> Shared<Self>
    where
        Self: Sized,
        Self::Output: Clone,
    {
        Shared::new(self)
    }

    /// Resolves to `Err` with the panic's payload if polling the future
    /// panics, instead of unwinding through the caller.
    fn catch_unwind(self) -> CatchUnwind<Self>
    where
        Self: Sized + UnwindSafe,
    {
        CatchUnwind { future: self }
    }
}

impl<F: Future + ?Sized> FutureExt for F {}

pin_project_lite::pin_project! {
    /// The future [`FutureExt::catch_unwind`] returns.
    #[must_use = "futures do nothing unless polled"]
    pub struct CatchUnwind<F> {
        #[pin]
        future: F,
    }
}

impl<F: Future + UnwindSafe> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let future = self.project().future;
        match catch_unwind(AssertUnwindSafe(|| future.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    }
}

/// Runs every future concurrently and resolves to their outputs, in the
/// order given, once all have finished. Each wake polls every future still
/// running, so it suits tens of futures, not thousands.
pub fn join_all<I>(futures: I) -> JoinAll<I::Item>
where
    I: IntoIterator,
    I::Item: Future,
{
    JoinAll {
        slots: futures
            .into_iter()
            .map(|f| Slot::Running(Box::pin(f)))
            .collect(),
    }
}

/// The future [`join_all`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct JoinAll<F: Future> {
    slots: Vec<Slot<F>>,
}

enum Slot<F: Future> {
    Running(Pin<Box<F>>),
    Done(F::Output),
    Taken,
}

impl<F: Future> Future for JoinAll<F> {
    type Output = Vec<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut done = true;
        for slot in &mut self.slots {
            if let Slot::Running(f) = slot {
                match f.as_mut().poll(cx) {
                    Poll::Ready(v) => *slot = Slot::Done(v),
                    Poll::Pending => done = false,
                }
            }
        }
        if !done {
            return Poll::Pending;
        }
        let outputs = self
            .slots
            .iter_mut()
            .map(|slot| match std::mem::replace(slot, Slot::Taken) {
                Slot::Done(v) => v,
                _ => panic!("JoinAll polled after completion"),
            })
            .collect();
        Poll::Ready(outputs)
    }
}

// The futures are boxed, so moving the slots never moves them.
impl<F: Future> Unpin for JoinAll<F> {}
