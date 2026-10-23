//! A future shared between tasks on one thread.

use std::cell::RefCell;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

/// A future whose output every clone gets a copy of. The first clone polled
/// drives the inner future; when that future wakes, every clone waiting on it
/// is woken, so one clone being dropped or left unpolled never strands the
/// others.
///
/// Clones are `!Send`; the inner future runs on the thread that made it.
#[must_use = "futures do nothing unless polled"]
pub struct Shared<F: Future> {
    /// `None` once this clone has resolved.
    inner: Option<Rc<Inner<F>>>,
    /// This clone's place in the waker list, once it has waited.
    slot: Option<usize>,
}

struct Inner<F: Future> {
    state: RefCell<State<F>>,
    wakers: Arc<Wakers>,
}

enum State<F: Future> {
    Pending(Pin<Box<F>>),
    /// The inner future is being polled, further up this thread's stack.
    Polling,
    Done(F::Output),
    /// Polling the inner future panicked.
    Poisoned,
}

/// The waker the inner future sees. It has to be `Send + Sync` to be a
/// [`Waker`], so it is behind an `Arc` and a `Mutex`, which is never
/// contended on one thread.
#[derive(Default)]
struct Wakers {
    slots: Mutex<Vec<Slot>>,
}

enum Slot {
    Free,
    /// Held by a clone; the waker is taken when it is woken.
    Held(Option<Waker>),
}

impl Wake for Wakers {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let wakers: Vec<Waker> = {
            let mut slots = self.slots.lock().unwrap();
            let held = slots.iter_mut().filter_map(|slot| match slot {
                Slot::Held(waker) => waker.take(),
                Slot::Free => None,
            });
            held.collect()
        };
        for waker in wakers {
            waker.wake();
        }
    }
}

impl<F: Future> Shared<F> {
    pub(super) fn new(future: F) -> Shared<F> {
        Shared {
            inner: Some(Rc::new(Inner {
                state: RefCell::new(State::Pending(Box::pin(future))),
                wakers: Arc::default(),
            })),
            slot: None,
        }
    }

    /// Records that this clone is waiting, to be woken with `waker`.
    fn register(&mut self, inner: &Inner<F>, waker: &Waker) {
        let mut slots = inner.wakers.slots.lock().unwrap();
        let slot = *self.slot.get_or_insert_with(|| {
            match slots.iter().position(|s| matches!(s, Slot::Free)) {
                Some(free) => {
                    slots[free] = Slot::Held(None);
                    free
                }
                None => {
                    slots.push(Slot::Held(None));
                    slots.len() - 1
                }
            }
        });
        match &mut slots[slot] {
            Slot::Held(Some(w)) if w.will_wake(waker) => {}
            held => *held = Slot::Held(Some(waker.clone())),
        }
    }

    fn unregister(&mut self) {
        if let (Some(inner), Some(slot)) = (&self.inner, self.slot.take()) {
            inner.wakers.slots.lock().unwrap()[slot] = Slot::Free;
        }
    }
}

impl<F: Future> Future for Shared<F>
where
    F::Output: Clone,
{
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = &mut *self;
        let inner = this.inner.clone().expect("Shared polled after completion");

        let state = std::mem::replace(&mut *inner.state.borrow_mut(), State::Polling);
        let mut future = match state {
            State::Pending(future) => future,
            State::Done(v) => {
                *inner.state.borrow_mut() = State::Done(v.clone());
                this.unregister();
                this.inner = None;
                return Poll::Ready(v);
            }
            State::Polling => {
                // Polled from inside its own inner future: it can only
                // finish once that poll returns.
                this.register(&inner, cx.waker());
                return Poll::Pending;
            }
            State::Poisoned => {
                *inner.state.borrow_mut() = State::Poisoned;
                panic!("Shared's inner future panicked");
            }
        };

        this.register(&inner, cx.waker());
        let waker = Waker::from(inner.wakers.clone());
        let poisoned = Poison(&inner.state);
        let polled = future.as_mut().poll(&mut Context::from_waker(&waker));
        std::mem::forget(poisoned);
        match polled {
            Poll::Pending => {
                *inner.state.borrow_mut() = State::Pending(future);
                Poll::Pending
            }
            Poll::Ready(v) => {
                *inner.state.borrow_mut() = State::Done(v.clone());
                this.unregister();
                this.inner = None;
                inner.wakers.wake_by_ref();
                Poll::Ready(v)
            }
        }
    }
}

/// Marks the state poisoned if the inner future's poll unwinds.
struct Poison<'a, F: Future>(&'a RefCell<State<F>>);

impl<F: Future> Drop for Poison<'_, F> {
    fn drop(&mut self) {
        *self.0.borrow_mut() = State::Poisoned;
    }
}

impl<F: Future> Clone for Shared<F> {
    fn clone(&self) -> Shared<F> {
        Shared {
            inner: self.inner.clone(),
            slot: None,
        }
    }
}

impl<F: Future> Drop for Shared<F> {
    fn drop(&mut self) {
        self.unregister();
    }
}

impl<F: Future> fmt::Debug for Shared<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match &self.inner {
            None => "resolved",
            Some(inner) => match inner.state.try_borrow().as_deref() {
                Ok(State::Pending(_)) => "pending",
                Ok(State::Done(_)) => "done",
                Ok(State::Poisoned) => "poisoned",
                Ok(State::Polling) | Err(_) => "polling",
            },
        };
        f.debug_struct("Shared").field("state", &state).finish()
    }
}
