// Extracted from Monoio and adapted for shep-smp-time; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

//! Sleeps retain a private key into their owner's slab until Drop. The wheel
//! holds keys, never references into futures or the slab's allocation. Entry
//! mutations borrow the owner; cloning, dropping, and waking wakers happen
//! after releasing that borrow so callbacks can operate on other timers.

use std::{
    cell::Cell,
    marker::PhantomPinned,
    num::NonZeroUsize,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use super::{Handle, InnerState};
use crate::{Instant, error::InsertError};

mod list;
pub(super) use list::EntryList;

type TimerResult = Result<(), crate::error::Error>;

/// Nonzero encoding of a slab key; optional links occupy one word. Keys stay
/// private and are never retained across callbacks after their owner is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EntryId(NonZeroUsize);

impl EntryId {
    pub(super) fn new(key: usize) -> Self {
        Self(NonZeroUsize::new(key.checked_add(1).expect("timer key overflow")).unwrap())
    }

    pub(super) fn index(self) -> usize {
        self.0.get() - 1
    }
}

#[derive(Debug)]
enum State {
    Registered(u64),
    Pending,
    Elapsed,
}

/// State owned by the slab. Plain fields suffice under the timer's borrow.
#[derive(Debug)]
pub(crate) struct Entry {
    state: State,
    cached_when: u64,
    prev: Option<EntryId>,
    next: Option<EntryId>,
    waker: Option<Waker>,
}

impl Entry {
    pub(super) fn new() -> Self {
        Self {
            state: State::Elapsed,
            cached_when: 0,
            prev: None,
            next: None,
            waker: None,
        }
    }

    pub(super) fn cached_when(&self) -> u64 {
        self.cached_when
    }

    pub(super) fn sync_when(&mut self) -> u64 {
        let State::Registered(when) = self.state else {
            panic!("timer is not registered");
        };
        self.cached_when = when;
        when
    }

    fn extend_expiration(&mut self, when: u64) -> bool {
        if let State::Registered(current) = self.state {
            if when >= current {
                self.state = State::Registered(when);
                return true;
            }
        }
        false
    }

    fn set_expiration(&mut self, when: u64) {
        self.state = State::Registered(when);
        self.cached_when = when;
    }

    pub(super) fn mark_pending(&mut self, not_after: u64) -> Result<(), u64> {
        let State::Registered(when) = self.state else {
            panic!("timer is not registered");
        };
        if when > not_after {
            self.cached_when = when;
            return Err(when);
        }
        self.state = State::Pending;
        Ok(())
    }

    pub(super) fn is_pending(&self) -> bool {
        matches!(self.state, State::Pending)
    }

    fn is_elapsed(&self) -> bool {
        matches!(self.state, State::Elapsed)
    }

    fn readiness(&self) -> Poll<TimerResult> {
        if self.is_elapsed() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    pub(super) fn fire(&mut self) -> Option<Waker> {
        if self.is_elapsed() {
            return None;
        }
        self.state = State::Elapsed;
        self.waker.take()
    }
}

/// Consumer-side ownership of a slot. Preserve Sleep's !Unpin API while making
/// its address irrelevant to the wheel. The key Cell permits safe pinned access.
#[derive(Debug)]
pub(super) struct TimerEntry {
    driver: Handle,
    key: Cell<Option<EntryId>>,
    _pin: PhantomPinned,
}

impl TimerEntry {
    pub(crate) fn new(handle: &Handle) -> Self {
        Self {
            driver: handle.clone(),
            key: Cell::new(None),
            _pin: PhantomPinned,
        }
    }

    #[inline]
    pub(crate) fn is_elapsed(&self) -> bool {
        self.key
            .get()
            .is_some_and(|key| self.driver.get().state.borrow().entries[key.index()].is_elapsed())
    }

    pub(crate) fn reset(self: Pin<&mut Self>, new_time: Instant) {
        let tick = self.driver.time_source().deadline_to_tick(new_time);
        let waker = {
            let mut state = self.driver.get().state.borrow_mut();
            let InnerState { wheel, entries } = &mut *state;
            let key = if let Some(key) = self.key.get() {
                if entries[key.index()].extend_expiration(tick) {
                    return;
                }
                if !entries[key.index()].is_elapsed() {
                    wheel.remove(entries, key);
                }
                entries[key.index()].set_expiration(tick);
                key
            } else {
                let mut entry = Entry::new();
                entry.set_expiration(tick);
                let key = EntryId::new(entries.insert(entry));
                self.key.set(Some(key));
                key
            };
            match wheel.insert(entries, key) {
                Ok(_) => None,
                Err((key, InsertError::Elapsed)) => entries[key.index()].fire(),
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(crate) fn poll_elapsed(
        self: Pin<&mut Self>,
        deadline: Instant,
        cx: &mut Context<'_>,
    ) -> Poll<TimerResult> {
        if let Some(key) = self.key.get() {
            let state = self.driver.get().state.borrow();
            let entry = &state.entries[key.index()];
            // Retain upstream's first-waker behavior, including ready polls.
            // The common pending path does not clone an already stored waker.
            if entry.waker.is_some() {
                return entry.readiness();
            }
        }
        self.poll_elapsed_slow(deadline, cx)
    }

    // Keep registration and callback setup out of the repeated-poll stack frame.
    #[inline(never)]
    fn poll_elapsed_slow(
        mut self: Pin<&mut Self>,
        deadline: Instant,
        cx: &mut Context<'_>,
    ) -> Poll<TimerResult> {
        if self.key.get().is_none() {
            self.as_mut().reset(deadline);
        }
        let key = self.key.get().unwrap();
        // Clone may invoke arbitrary code. This Sleep retains its slot, but
        // the callback can expire it or grow the slab by registering siblings.
        let mut cloned = Some(cx.waker().clone());
        let result = {
            let mut state = self.driver.get().state.borrow_mut();
            let entry = &mut state.entries[key.index()];
            if entry.waker.is_none() {
                entry.waker = cloned.take();
            }
            entry.readiness()
        };
        drop(cloned);
        result
    }
}

impl Drop for TimerEntry {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else { return };
        let removed = {
            let mut state = self.driver.get().state.borrow_mut();
            let InnerState { wheel, entries } = &mut *state;
            if !entries[key.index()].is_elapsed() {
                wheel.remove(entries, key);
            }
            entries.remove(key.index())
        };
        // Includes wakers stored by polling an already elapsed sleep. Slot
        // removal precedes callbacks, and this consumer no longer retains a key.
        drop(removed);
    }
}
