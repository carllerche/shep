// Extracted from Monoio and adapted for Shep, commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

//! Sleeps retain a private key into their owner's slab until Drop. The wheel
//! holds keys, never references into futures or the slab's allocation. Entry
//! mutations borrow the owner; cloning, dropping, and waking wakers happen
//! after releasing that borrow so callbacks can operate on other timers.

use std::{
    num::NonZeroUsize,
    task::{Context, Poll, Waker},
};

use shep_util_slab::Slab;

use crate::time::{
    Instant,
    error::InsertError,
    timer::{Handle, InnerState},
};

type TimerResult = Result<(), crate::time::error::Error>;

/// Nonzero encoding of a slab key; optional links occupy one word. Keys stay
/// private and are never retained across callbacks after their owner is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EntryId(NonZeroUsize);

impl EntryId {
    fn new(key: usize) -> Self {
        Self(NonZeroUsize::new(key.checked_add(1).expect("timer key overflow")).unwrap())
    }

    pub(crate) fn index(self) -> usize {
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
    fn new() -> Self {
        Self {
            state: State::Elapsed,
            cached_when: 0,
            prev: None,
            next: None,
            waker: None,
        }
    }

    pub(crate) fn cached_when(&self) -> u64 {
        self.cached_when
    }

    pub(crate) fn sync_when(&mut self) -> u64 {
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

    pub(crate) fn mark_pending(&mut self, not_after: u64) -> Result<(), u64> {
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

    pub(crate) fn is_pending(&self) -> bool {
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

    pub(crate) fn fire(&mut self) -> Option<Waker> {
        if self.is_elapsed() {
            return None;
        }
        self.state = State::Elapsed;
        self.waker.take()
    }
}

/// Movable consumer-side ownership of a slot. The wheel retains only its key.
#[derive(Debug)]
pub(crate) struct TimerEntry {
    driver: Handle,
    key: Option<EntryId>,
}

impl TimerEntry {
    pub(crate) fn new(driver: Handle) -> Self {
        Self { driver, key: None }
    }

    #[inline]
    pub(crate) fn is_elapsed(&self) -> bool {
        self.key
            .is_some_and(|key| self.driver.get().state.borrow().entries[key.index()].is_elapsed())
    }

    pub(crate) fn reset(&mut self, new_time: Instant) {
        let tick = self.driver.time_source().deadline_to_tick(new_time);
        let waker = {
            let mut state = self.driver.get().state.borrow_mut();
            let InnerState { wheel, entries } = &mut *state;
            let key = if let Some(key) = self.key {
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
                self.key = Some(key);
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
        &mut self,
        deadline: Instant,
        cx: &mut Context<'_>,
    ) -> Poll<TimerResult> {
        if let Some(key) = self.key {
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
    fn poll_elapsed_slow(&mut self, deadline: Instant, cx: &mut Context<'_>) -> Poll<TimerResult> {
        if self.key.is_none() {
            self.reset(deadline);
        }
        let key = self.key.unwrap();
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

/// Doubly linked list of slab IDs. Checked, sequential mutations keep entry
/// links private without creating aliased mutable references.
#[derive(Debug, Default)]
pub(crate) struct EntryList {
    head: Option<EntryId>,
    tail: Option<EntryId>,
}

impl EntryList {
    pub(crate) const fn new() -> Self {
        Self {
            head: None,
            tail: None,
        }
    }

    pub(crate) fn push_front(&mut self, entries: &mut Slab<Entry>, key: EntryId) {
        assert_ne!(self.head, Some(key));
        debug_assert!(entries[key.index()].prev.is_none());
        debug_assert!(entries[key.index()].next.is_none());
        entries[key.index()].next = self.head;
        if let Some(head) = self.head {
            entries[head.index()].prev = Some(key);
        }
        self.head = Some(key);
        if self.tail.is_none() {
            self.tail = Some(key);
        }
    }

    pub(crate) fn pop_back(&mut self, entries: &mut Slab<Entry>) -> Option<EntryId> {
        let key = self.tail?;
        self.tail = entries[key.index()].prev;
        if let Some(prev) = self.tail {
            entries[prev.index()].next = None;
        } else {
            self.head = None;
        }
        entries[key.index()].prev = None;
        entries[key.index()].next = None;
        Some(key)
    }

    pub(crate) fn is_empty(&self) -> bool {
        if self.head.is_some() {
            return false;
        }
        assert!(self.tail.is_none());
        true
    }

    pub(crate) fn remove(&mut self, entries: &mut Slab<Entry>, key: EntryId) -> Option<EntryId> {
        let prev = entries[key.index()].prev;
        let next = entries[key.index()].next;
        if let Some(prev) = prev {
            debug_assert_eq!(entries[prev.index()].next, Some(key));
            entries[prev.index()].next = next;
        } else {
            if self.head != Some(key) {
                return None;
            }
            self.head = next;
        }
        if let Some(next) = next {
            debug_assert_eq!(entries[next.index()].prev, Some(key));
            entries[next.index()].prev = prev;
        } else {
            if self.tail != Some(key) {
                return None;
            }
            self.tail = prev;
        }
        entries[key.index()].prev = None;
        entries[key.index()].next = None;
        Some(key)
    }
}

// Adapt the four upstream list cases to owned entries and key identity.
#[cfg(test)]
mod tests {
    use super::*;

    fn entry(entries: &mut Slab<Entry>) -> EntryId {
        EntryId::new(entries.insert(Entry::new()))
    }

    fn collect(list: &mut EntryList, entries: &mut Slab<Entry>) -> Vec<EntryId> {
        let mut result = Vec::new();
        while let Some(key) = list.pop_back(entries) {
            result.push(key);
        }
        result
    }

    #[test]
    fn const_new() {
        const _: EntryList = EntryList::new();
    }

    #[test]
    fn push_and_drain() {
        let mut entries = Slab::new();
        let keys = [
            entry(&mut entries),
            entry(&mut entries),
            entry(&mut entries),
        ];
        let mut list = EntryList::new();
        assert!(list.is_empty());
        for key in keys {
            list.push_front(&mut entries, key);
        }
        assert!(!list.is_empty());
        assert_eq!(collect(&mut list, &mut entries), keys);
        assert!(list.is_empty());
        for key in keys {
            assert_eq!(entries[key.index()].prev, None);
            assert_eq!(entries[key.index()].next, None);
        }
    }

    #[test]
    fn push_pop_push_pop() {
        let mut entries = Slab::new();
        let a = entry(&mut entries);
        let b = entry(&mut entries);
        let mut list = EntryList::new();
        for key in [a, b] {
            list.push_front(&mut entries, key);
            assert_eq!(list.pop_back(&mut entries), Some(key));
            assert!(list.is_empty());
            assert_eq!(entries[key.index()].prev, None);
            assert_eq!(entries[key.index()].next, None);
        }
        assert_eq!(list.pop_back(&mut entries), None);
    }

    #[test]
    fn remove_by_id() {
        for count in 1..=3 {
            // Head, middle, tail, repeat removals, and missing detached node.
            for removals in [&[0, 1, 2][..], &[2, 1, 0], &[1, 1], &[0, 0], &[2, 2], &[3]] {
                let mut entries = Slab::new();
                let keys: Vec<_> = (0..4).map(|_| entry(&mut entries)).collect();
                let mut expected = keys[..count].to_vec();
                let mut list = EntryList::new();
                for &key in &expected {
                    list.push_front(&mut entries, key);
                }
                for &index in removals {
                    let key = keys[index];
                    let present = expected.contains(&key);
                    assert_eq!(list.remove(&mut entries, key).is_some(), present);
                    expected.retain(|&other| other != key);
                    assert_eq!(entries[key.index()].prev, None);
                    assert_eq!(entries[key.index()].next, None);
                    assert_eq!(list.head, expected.last().copied());
                    assert_eq!(list.tail, expected.first().copied());
                    for (position, &key) in expected.iter().enumerate() {
                        assert_eq!(
                            entries[key.index()].prev,
                            expected.get(position + 1).copied()
                        );
                        assert_eq!(
                            entries[key.index()].next,
                            position.checked_sub(1).map(|i| expected[i])
                        );
                    }
                }
                assert_eq!(collect(&mut list, &mut entries), expected);
                assert!(list.is_empty());
            }
        }
    }
}
