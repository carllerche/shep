// Extracted from Monoio and adapted for shep-smp-time; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

//! Local timer entries shared by a pinned sleep and the wheel.
//!
//! The wheel holds non-owning pointers into TimerEntry. Registration requires
//! pinning; dropping an entry unlinks it before its storage is released. The
//! outer UnsafeCell permits shared access to this inline state without exposing
//! a mutable reference to it. Individual fields use Cell or RefCell.
//!
//! List membership changes require the owning wheel's mutable borrow. The
//! unsafe raw-handle methods enforce liveness and detached-entry requirements;
//! the private Cell-only helpers implement the state transitions they permit.
//! Wakers must be invoked or dropped after releasing the wheel borrow.
//!
//! A lazy deadline extension updates the true deadline while leaving the cached
//! bucket deadline in place. The wheel reconciles them when visiting that bucket.

use std::{
    cell::{Cell, RefCell, UnsafeCell},
    marker::PhantomPinned,
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll, Waker},
};

use super::Handle;
use crate::{Instant, utils::linked_list};

type TimerResult = Result<(), crate::error::Error>;

const STATE_DEREGISTERED: u64 = u64::MAX;
const STATE_PENDING_FIRE: u64 = STATE_DEREGISTERED - 1;
const STATE_MIN_VALUE: u64 = STATE_PENDING_FIRE;

/// Local readiness, true deadline, and the task waiting on this timer.
#[derive(Debug)]
struct StateCell {
    /// The deadline, STATE_PENDING_FIRE, or STATE_DEREGISTERED.
    state: Cell<u64>,
    /// The last completion result, used only while deregistered.
    result: Cell<TimerResult>,
    waker: RefCell<Option<Waker>>,
}

impl StateCell {
    fn new() -> Self {
        Self {
            state: Cell::new(STATE_DEREGISTERED),
            result: Cell::new(Ok(())),
            waker: RefCell::new(None),
        }
    }

    fn when(&self) -> Option<u64> {
        let state = self.state.get();
        (state != STATE_DEREGISTERED).then_some(state)
    }

    fn poll(&self, waker: &Waker) -> Poll<TimerResult> {
        // Preserve Monoio's first-waker registration until this timer fires.
        let mut current = self.waker.borrow_mut();
        if current.is_none() {
            *current = Some(waker.clone());
        }
        if self.state.get() == STATE_DEREGISTERED {
            Poll::Ready(self.result.get())
        } else {
            Poll::Pending
        }
    }

    /// Called by a detached raw handle before the wheel enqueues it as pending.
    fn mark_pending(&self, not_after: u64) -> Result<(), u64> {
        let state = self.state.get();
        debug_assert!(state < STATE_MIN_VALUE);
        if state > not_after {
            return Err(state);
        }
        self.state.set(STATE_PENDING_FIRE);
        Ok(())
    }

    /// Complete a detached entry. The caller releases the wheel borrow before
    /// invoking or dropping the returned waker.
    fn fire(&self, result: TimerResult) -> Option<Waker> {
        if self.state.get() == STATE_DEREGISTERED {
            return None;
        }
        self.result.set(result);
        self.state.set(STATE_DEREGISTERED);
        self.waker.take()
    }

    fn set_expiration(&self, timestamp: u64) {
        debug_assert!(timestamp < STATE_MIN_VALUE);
        self.state.set(timestamp);
    }

    fn extend_expiration(&self, timestamp: u64) -> Result<(), ()> {
        let state = self.state.get();
        if timestamp < state || state >= STATE_MIN_VALUE {
            return Err(());
        }
        self.state.set(timestamp);
        Ok(())
    }

    fn might_be_registered(&self) -> bool {
        self.state.get() != STATE_DEREGISTERED
    }
}

/// The sleep's inline timer entry. It must be pinned before registration.
#[derive(Debug)]
pub(super) struct TimerEntry {
    /// Keeps the local wheel alive until this entry has been unlinked.
    driver: Handle,
    /// Raw wheel handles alias this state through shared references only.
    inner: UnsafeCell<TimerShared>,
    /// Consumed by first poll or reset; updating it never moves pinned state.
    initial_deadline: Cell<Option<Instant>>,
    _pin: PhantomPinned,
}

/// A non-owning handle to inline entry storage. It may only be used while the
/// owning TimerEntry remains alive and immovable. Wheel operations transfer
/// this handle between a single bucket, the pending queue, and detached state.
#[derive(Debug)]
pub(crate) struct TimerHandle {
    inner: NonNull<TimerShared>,
}

pub(super) type EntryList = linked_list::LinkedList<TimerShared, TimerShared>;

#[derive(Debug)]
pub(crate) struct TimerShared {
    state: StateCell,
    wheel_state: WheelState,
    _pin: PhantomPinned,
}

impl TimerShared {
    fn new() -> Self {
        Self {
            state: StateCell::new(),
            wheel_state: WheelState::new(),
            _pin: PhantomPinned,
        }
    }

    pub(super) fn cached_when(&self) -> u64 {
        self.wheel_state.cached_when.get()
    }

    // These helpers are private to the raw-handle implementation, which
    // requires a detached entry before changing its cached list membership.
    fn sync_when(&self) -> u64 {
        let when = self.state.when().expect("Timer already fired");
        self.wheel_state.cached_when.set(when);
        when
    }

    fn set_cached_when(&self, when: u64) {
        self.wheel_state.cached_when.set(when);
    }

    fn set_expiration(&self, when: u64) {
        self.state.set_expiration(when);
        self.wheel_state.cached_when.set(when);
    }

    fn extend_expiration(&self, when: u64) -> Result<(), ()> {
        self.state.extend_expiration(when)
    }

    /// Mint a raw handle; it does not extend this entry's lifetime.
    pub(super) fn handle(&self) -> TimerHandle {
        TimerHandle {
            inner: NonNull::from(self),
        }
    }

    pub(super) fn might_be_registered(&self) -> bool {
        self.state.might_be_registered()
    }
}

#[derive(Debug)]
struct WheelState {
    /// The deadline used to locate the current bucket, or u64::MAX for pending.
    cached_when: Cell<u64>,
    pointers: linked_list::Pointers<TimerShared>,
}

impl WheelState {
    fn new() -> Self {
        Self {
            cached_when: Cell::new(0),
            pointers: linked_list::Pointers::new(),
        }
    }
}

// SAFETY: TimerEntry registers only after pinning and unlinks on Drop. Handles
// retain no ownership, so their callers must uphold liveness. Links use shared
// Cell access; this implementation never creates &mut to an aliased entry.
unsafe impl linked_list::Link for TimerShared {
    type Handle = TimerHandle;
    type Target = TimerShared;

    fn as_raw(handle: &Self::Handle) -> NonNull<Self::Target> {
        handle.inner
    }

    unsafe fn from_raw(ptr: NonNull<Self::Target>) -> Self::Handle {
        TimerHandle { inner: ptr }
    }

    unsafe fn pointers(
        target: NonNull<Self::Target>,
    ) -> NonNull<linked_list::Pointers<Self::Target>> {
        // SAFETY: Link's caller guarantees that target is a live pinned entry.
        NonNull::from(&unsafe { target.as_ref() }.wheel_state.pointers)
    }
}

impl TimerEntry {
    pub(crate) fn new(handle: &Handle, deadline: Instant) -> Self {
        Self {
            driver: handle.clone(),
            inner: UnsafeCell::new(TimerShared::new()),
            initial_deadline: Cell::new(Some(deadline)),
            _pin: PhantomPinned,
        }
    }

    fn inner(&self) -> &TimerShared {
        // SAFETY: The cell belongs to this live entry. It is never replaced or
        // mutably referenced. Shared mutations use its Cell/RefCell fields.
        unsafe { &*self.inner.get() }
    }

    pub(crate) fn is_elapsed(&self) -> bool {
        !self.inner().state.might_be_registered() && self.initial_deadline.get().is_none()
    }

    fn cancel(&self) {
        // SAFETY: This entry remains alive and immovable for this call. If it
        // was registered, reset/poll established pinning, and Drop unlinks it
        // before its storage is released. An unregistered entry never escapes.
        unsafe { self.driver.clear_entry(NonNull::from(self.inner())) };
    }

    pub(crate) fn reset(self: Pin<&mut Self>, new_time: Instant) {
        self.as_ref().get_ref().initial_deadline.set(None);
        let tick = self.driver.time_source().deadline_to_tick(new_time);
        if self.inner().extend_expiration(tick).is_ok() {
            return;
        }
        // SAFETY: self pins the entry before its address is registered. The
        // driver is its owning wheel and cancellation unlinks it on Drop.
        unsafe { self.driver.reregister(tick, self.inner().into()) };
    }

    pub(crate) fn poll_elapsed(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TimerResult> {
        if let Some(deadline) = self.initial_deadline.get() {
            self.as_mut().reset(deadline);
        }
        self.as_ref().get_ref().inner().state.poll(cx.waker())
    }
}

impl TimerHandle {
    /// # Safety
    /// The pointee must remain live and immovable for this call.
    pub(super) unsafe fn cached_when(&self) -> u64 {
        // SAFETY: Guaranteed by the caller; access is shared and Cell-based.
        unsafe { self.inner.as_ref() }.cached_when()
    }

    /// # Safety
    /// The entry must be live, pinned, and detached from every wheel list.
    /// The owner must retain a mutable wheel borrow during the transition.
    pub(super) unsafe fn sync_when(&self) -> u64 {
        // SAFETY: The caller guarantees liveness and detached membership.
        unsafe { self.inner.as_ref() }.sync_when()
    }

    /// # Safety
    /// The entry must be live, pinned, and detached from every wheel list.
    pub(super) unsafe fn set_expiration(&self, tick: u64) {
        // SAFETY: The caller guarantees liveness and detached membership.
        unsafe { self.inner.as_ref() }.set_expiration(tick);
    }

    /// Mark an expired entry pending, or reconcile its cached deadline.
    ///
    /// # Safety
    /// The entry must be live, pinned, and detached. The caller must enqueue
    /// it in the pending list on success or reinsert it in a bucket on error.
    pub(super) unsafe fn mark_pending(&self, not_after: u64) -> Result<(), u64> {
        // SAFETY: The caller retains the owner and has removed its list node.
        let entry = unsafe { self.inner.as_ref() };
        match entry.state.mark_pending(not_after) {
            Ok(()) => {
                entry.set_cached_when(u64::MAX);
                Ok(())
            }
            Err(tick) => {
                entry.set_cached_when(tick);
                Err(tick)
            }
        }
    }

    /// Complete a detached entry and return its waker for use after the wheel
    /// borrow is released. Consumes the raw handle before user callbacks run.
    ///
    /// # Safety
    /// The entry must be live and detached from every wheel list.
    pub(super) unsafe fn fire(self, result: TimerResult) -> Option<Waker> {
        // SAFETY: The caller guarantees a live, detached entry. All state
        // mutation is local and uses Cell/RefCell.
        unsafe { self.inner.as_ref() }.state.fire(result)
    }
}

impl Drop for TimerEntry {
    fn drop(&mut self) {
        self.cancel();
    }
}
