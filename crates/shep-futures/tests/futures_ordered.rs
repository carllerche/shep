//! `FuturesOrdered` against a queue, under any interleaving of pushes,
//! futures finishing (with a wake, or silently, to be found by the next
//! poll), spurious wakes, and polls. Whatever happens:
//!
//! - outputs come out in the order their futures were pushed, each once;
//! - a poll yields the front output once its future has finished, is
//!   pending while it hasn't, and ends while the queue is empty;
//! - `len` counts what has been pushed and not yet yielded;
//! - no future is polled after it finishes;
//! - no wakeup is lost: once every future has finished, polling only when
//!   woken drains the queue.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use hegel::TestCase;
use hegel::generators as gs;
use shep_futures::Stream;
use shep_futures::stream::FuturesOrdered;

/// A future that finishes, with its id, once `ready` is set.
#[derive(Default)]
struct Gate {
    ready: Cell<bool>,
    finished: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

impl Gate {
    /// Makes the future ready; `wake` wakes whoever polled it last, which
    /// otherwise finds out only by polling again.
    fn finish(&self, wake: bool) {
        self.ready.set(true);
        if wake && let Some(w) = self.waker.borrow_mut().take() {
            w.wake();
        }
    }
}

struct Wait(u32, Rc<Gate>);

impl Future for Wait {
    type Output = u32;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        let gate = &self.1;
        assert!(!gate.finished.get(), "future polled after it finished");
        if gate.ready.get() {
            gate.finished.set(true);
            gate.waker.borrow_mut().take();
            Poll::Ready(self.0)
        } else {
            *gate.waker.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[derive(Default)]
struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct Driver {
    stream: FuturesOrdered<Wait>,
    flag: Arc<Flag>,
    /// The model: ids pushed and not yet yielded, in order.
    queue: VecDeque<u32>,
    gates: Vec<Rc<Gate>>,
    /// The last poll returned `Pending`.
    waiting: bool,
}

impl Driver {
    fn poll(&mut self) {
        self.flag.0.store(false, Ordering::SeqCst);
        let waker = Waker::from(self.flag.clone());
        let polled = Pin::new(&mut self.stream).poll_next(&mut Context::from_waker(&waker));
        self.waiting = false;
        match (polled, self.queue.front()) {
            (Poll::Ready(None), None) => {}
            (Poll::Ready(Some(id)), Some(&front)) => {
                assert_eq!(id, front, "outputs out of push order");
                assert!(self.gates[id as usize].ready.get());
                self.queue.pop_front();
            }
            (Poll::Pending, Some(&front)) => {
                assert!(
                    !self.gates[front as usize].ready.get(),
                    "pending though the front future can finish"
                );
                self.waiting = true;
            }
            (polled, front) => panic!("polled {polled:?} with front {front:?}"),
        }
        assert_eq!(self.stream.len(), self.queue.len());
        assert_eq!(self.stream.is_empty(), self.queue.is_empty());
    }
}

#[hegel::test(test_cases = 3000)]
fn yields_in_push_order(tc: TestCase) {
    let mut d = Driver {
        stream: FuturesOrdered::new(),
        flag: Arc::default(),
        queue: VecDeque::new(),
        gates: Vec::new(),
        waiting: false,
    };
    let steps = tc.draw(gs::integers::<usize>().max_value(50));
    for _ in 0..steps {
        match tc.draw(gs::integers::<u8>().max_value(4)) {
            0 => {
                let id = d.gates.len() as u32;
                let gate = Rc::new(Gate::default());
                d.gates.push(gate.clone());
                d.stream.push_back(Wait(id, gate));
                d.queue.push_back(id);
            }
            1 | 2 if !d.gates.is_empty() => {
                let i = tc.draw(gs::integers::<usize>().max_value(d.gates.len() - 1));
                let wake = tc.draw(gs::booleans());
                d.gates[i].finish(wake);
            }
            3 => {
                for gate in &d.gates {
                    if let Some(w) = gate.waker.borrow().as_ref() {
                        w.wake_by_ref();
                    }
                }
            }
            _ => d.poll(),
        }
    }
    tc.event_value("left in queue", d.queue.len() as f64);

    // Finish everything, delivering the wakes owed, then poll only when
    // woken.
    for gate in &d.gates {
        if !gate.finished.get() {
            gate.finish(true);
        }
    }
    while !d.queue.is_empty() {
        if d.waiting && !d.flag.0.load(Ordering::SeqCst) {
            panic!("{} outputs never yielded: no wake came", d.queue.len());
        }
        d.poll();
    }
}

#[tokio::test(flavor = "local")]
async fn runs_concurrently_yields_in_order() {
    use std::time::Duration;

    use shep_futures::StreamExt;
    use shep_futures::future::LocalBoxFuture;

    let finished = Rc::new(RefCell::new(Vec::new()));
    let mut queue: FuturesOrdered<LocalBoxFuture<'_, u64>> = FuturesOrdered::new();
    for i in 0..4u64 {
        let finished = finished.clone();
        queue.push_back(Box::pin(async move {
            tokio::time::sleep(Duration::from_millis((4 - i) * 10)).await;
            finished.borrow_mut().push(i);
            i
        }));
    }
    let mut out = Vec::new();
    while let Some(i) = queue.next().await {
        out.push(i);
    }
    assert_eq!(out, vec![0, 1, 2, 3]);
    assert_eq!(*finished.borrow(), vec![3, 2, 1, 0]);
    // Empty is not the end: more can be pushed.
    assert_eq!(queue.next().await, None);
    queue.push_back(Box::pin(async { 9 }));
    assert_eq!(queue.next().await, Some(9));
}
