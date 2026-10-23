//! Streams: taking the next item, building one from a closure, and running
//! futures concurrently while yielding their outputs in order.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, ready};

use futures_core::Stream;

pub trait StreamExt: Stream {
    /// The next item, or `None` once the stream has ended.
    fn next(&mut self) -> Next<'_, Self>
    where
        Self: Unpin,
    {
        Next { stream: self }
    }
}

impl<S: Stream + ?Sized> StreamExt for S {}

/// The future [`StreamExt::next`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct Next<'a, S: ?Sized> {
    stream: &'a mut S,
}

impl<S: Stream + Unpin + ?Sized> Future for Next<'_, S> {
    type Output = Option<S::Item>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut *self.stream).poll_next(cx)
    }
}

/// A stream that ends at once.
pub fn empty<T>() -> Empty<T> {
    Empty(PhantomData)
}

/// The stream [`empty`] returns.
#[must_use = "streams do nothing unless polled"]
pub struct Empty<T>(PhantomData<fn() -> T>);

impl<T> Stream for Empty<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<T>> {
        Poll::Ready(None)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(0))
    }
}

/// A stream of what `f` yields: each call takes the state and resolves to
/// the next item and the state for the call after, or to `None` to end.
pub fn unfold<T, F, Fut, Item>(init: T, f: F) -> Unfold<T, F, Fut>
where
    F: FnMut(T) -> Fut,
    Fut: Future<Output = Option<(Item, T)>>,
{
    Unfold {
        f,
        state: UnfoldState::Value { value: init },
    }
}

pin_project_lite::pin_project! {
    /// The stream [`unfold`] returns.
    #[must_use = "streams do nothing unless polled"]
    pub struct Unfold<T, F, Fut> {
        f: F,
        #[pin]
        state: UnfoldState<T, Fut>,
    }
}

pin_project_lite::pin_project! {
    #[project = UnfoldStateProj]
    #[project_replace = UnfoldStateReplace]
    enum UnfoldState<T, Fut> {
        Value { value: T },
        Future { #[pin] future: Fut },
        Ended,
    }
}

impl<T, F, Fut, Item> Stream for Unfold<T, F, Fut>
where
    F: FnMut(T) -> Fut,
    Fut: Future<Output = Option<(Item, T)>>,
{
    type Item = Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Item>> {
        let mut this = self.project();
        if let UnfoldStateProj::Value { .. } = this.state.as_mut().project() {
            let UnfoldStateReplace::Value { value } =
                this.state.as_mut().project_replace(UnfoldState::Ended)
            else {
                unreachable!()
            };
            let future = (this.f)(value);
            this.state.set(UnfoldState::Future { future });
        }
        let UnfoldStateProj::Future { future } = this.state.as_mut().project() else {
            return Poll::Ready(None);
        };
        match ready!(future.poll(cx)) {
            Some((item, next)) => {
                this.state.set(UnfoldState::Value { value: next });
                Poll::Ready(Some(item))
            }
            None => {
                this.state.set(UnfoldState::Ended);
                Poll::Ready(None)
            }
        }
    }
}

/// Futures run concurrently, their outputs yielded in the order they were
/// pushed: a finished future's output waits until every future pushed before
/// it has been yielded. The stream yields `None` while it is empty, and more
/// futures may be pushed after.
///
/// Each wake polls every future still running, so it suits a short queue,
/// like a pipeline's window, not thousands.
///
/// It is `!Send`, whatever the futures are:
///
/// ```compile_fail
/// fn send<T: Send>(_: T) {}
/// send(shep_futures::stream::FuturesOrdered::<std::future::Ready<()>>::new());
/// ```
#[must_use = "streams do nothing unless polled"]
pub struct FuturesOrdered<F: Future> {
    queue: VecDeque<Entry<F>>,
    _local: PhantomData<Rc<()>>,
}

enum Entry<F: Future> {
    Running(Pin<Box<F>>),
    Done(F::Output),
}

impl<F: Future> FuturesOrdered<F> {
    pub fn new() -> FuturesOrdered<F> {
        FuturesOrdered {
            queue: VecDeque::new(),
            _local: PhantomData,
        }
    }

    /// Adds a future to the back of the queue. It is first polled the next
    /// time the stream is.
    pub fn push_back(&mut self, future: F) {
        self.queue.push_back(Entry::Running(Box::pin(future)));
    }

    /// The futures pushed whose outputs have not been yielded, running or
    /// finished.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

impl<F: Future> Default for FuturesOrdered<F> {
    fn default() -> FuturesOrdered<F> {
        FuturesOrdered::new()
    }
}

impl<F: Future> Stream for FuturesOrdered<F> {
    type Item = F::Output;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<F::Output>> {
        for entry in &mut self.queue {
            if let Entry::Running(future) = entry
                && let Poll::Ready(v) = future.as_mut().poll(cx)
            {
                *entry = Entry::Done(v);
            }
        }
        match self.queue.front() {
            None => Poll::Ready(None),
            Some(Entry::Running(_)) => Poll::Pending,
            Some(Entry::Done(_)) => match self.queue.pop_front() {
                Some(Entry::Done(v)) => Poll::Ready(Some(v)),
                _ => unreachable!(),
            },
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.queue.len(), None)
    }
}

// The futures are boxed, so moving the queue never moves them.
impl<F: Future> Unpin for FuturesOrdered<F> {}

impl<F: Future> fmt::Debug for FuturesOrdered<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FuturesOrdered")
            .field("len", &self.queue.len())
            .finish()
    }
}
