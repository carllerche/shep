//! Completion-owned accounting shared by file clones and helper threads.
use std::{
    collections::BTreeMap,
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

#[derive(Default)]
pub(crate) struct Activity(Mutex<State>);
#[derive(Default)]
struct State {
    active: usize,
    closed: bool,
    next: u64,
    waiters: BTreeMap<u64, Waker>,
}
impl Activity {
    pub(crate) fn begin(self: &Arc<Self>) -> io::Result<Guard> {
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        state.active += 1;
        Ok(Guard(self.clone()))
    }
    pub(crate) fn close(&self) {
        self.0.lock().unwrap().closed = true;
    }
    pub(crate) fn idle(self: &Arc<Self>) -> Idle {
        Idle {
            activity: self.clone(),
            id: None,
        }
    }
}
pub(crate) struct Guard(Arc<Activity>);
impl Drop for Guard {
    fn drop(&mut self) {
        let wakes = {
            let mut state = self.0.0.lock().unwrap();
            state.active -= 1;
            if state.active == 0 {
                std::mem::take(&mut state.waiters)
            } else {
                BTreeMap::new()
            }
        };
        for wake in wakes.into_values() {
            wake_safely(wake);
        }
    }
}
pub(crate) fn wake_safely(wake: Waker) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wake.wake()));
}
pub(crate) struct Idle {
    activity: Arc<Activity>,
    id: Option<u64>,
}
impl Future for Idle {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let wake = cx.waker().clone();
        let mut state = this.activity.0.lock().unwrap();
        if state.active == 0 {
            return Poll::Ready(());
        }
        let id = *this.id.get_or_insert_with(|| {
            let id = state.next;
            state.next = state
                .next
                .checked_add(1)
                .expect("waiter identity exhausted");
            id
        });
        let old = state.waiters.insert(id, wake);
        drop(state);
        drop(old);
        Poll::Pending
    }
}
impl Drop for Idle {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let old = self.activity.0.lock().unwrap().waiters.remove(&id);
            drop(old);
        }
    }
}
