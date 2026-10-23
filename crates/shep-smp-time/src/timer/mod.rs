// Extracted from Monoio and adapted for shep-smp-time; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

//! Reactor-owned timer wheel

mod entry;
use self::entry::{EntryList, TimerEntry, TimerHandle, TimerShared};

mod handle;
pub(crate) use self::handle::Handle;

mod wheel;

pub mod sleep;

use std::{cell::RefCell, fmt, ptr::NonNull, rc::Rc};

use crate::{Duration, Instant};

/// A core-local timer wheel owned and advanced by the reactor.
///
/// [`advance`](Self::advance) processes elapsed time and wakes expired timers.
/// [`next_wake`](Self::next_wake) tells the reactor when the wheel next needs
/// processing. This type performs no waiting or I/O. Use [`with`](Self::with)
/// to select this timer while constructing and polling timer futures.
///
/// Deadlines round up to one-millisecond ticks. Drop timer consumers before
/// this owner: outstanding sleeps retain their state, but cannot advance once
/// the timer is gone.
///
/// # Implementation
///
/// The timer wheel is based on the [paper by Varghese and Lauck][paper].
///
/// A hashed timing wheel is a vector of slots, where each slot handles a time
/// slice. As time progresses, the timer walks over the slot for the current
/// instant, and processes each entry for that slot. When the timer reaches the
/// end of the wheel, it starts again at the beginning.
///
/// The implementation maintains six wheels arranged in a set of levels. As the
/// levels go up, the slots of the associated wheel represent larger intervals
/// of time. At each level, the wheel has 64 slots. Each slot covers a range of
/// time equal to the wheel at the lower level. At level zero, each slot
/// represents one millisecond of time.
///
/// The wheels are:
///
/// * Level 0: 64 x 1 millisecond slots.
/// * Level 1: 64 x 64 millisecond slots.
/// * Level 2: 64 x ~4 second slots.
/// * Level 3: 64 x ~4 minute slots.
/// * Level 4: 64 x ~4 hour slots.
/// * Level 5: 64 x ~12 day slots.
///
/// When the timer processes entries at level zero, it will notify all the
/// `Sleep` instances as their deadlines have been reached. For all higher
/// levels, all entries will be redistributed across the wheel at the next level
/// down. Eventually, as time progresses, entries with [`Sleep`][sleep]
/// instances will either be canceled (dropped) or their associated entries will
/// reach level zero and be notified.
///
/// [paper]: http://www.cs.columbia.edu/~nahum/w6998/papers/ton97-timing-wheels.pdf
/// [sleep]: crate::Sleep
/// [timeout]: crate::Timeout
/// [interval]: crate::Interval
#[derive(Debug)]
pub struct Timer {
    handle: Handle,
}

/// Conversion between absolute instants and wheel-relative millisecond ticks.
#[derive(Debug, Clone)]
struct ClockTime {
    start_time: Instant,
}

impl ClockTime {
    fn deadline_to_tick(&self, t: Instant) -> u64 {
        self.instant_to_tick(t + Duration::from_nanos(999_999))
    }

    fn instant_to_tick(&self, t: Instant) -> u64 {
        let dur = t
            .checked_duration_since(self.start_time)
            .unwrap_or_default();
        dur.as_millis()
            .try_into()
            .expect("Duration too far into the future")
    }

    fn tick_to_instant(&self, t: u64) -> Instant {
        self.start_time + Duration::from_millis(t)
    }
}

/// Timer state shared locally by the owner and its registered entries.
struct Inner {
    state: RefCell<InnerState>,
}

struct InnerState {
    wheel: wheel::Wheel,
}

impl Timer {
    /// Create an empty wheel with `now` as its epoch. The reactor supplies the
    /// same monotonic clock for subsequent calls to [`advance`](Self::advance).
    pub fn new(now: Instant) -> Self {
        let inner = Inner {
            state: RefCell::new(InnerState {
                wheel: wheel::Wheel::new(),
            }),
        };
        Self {
            handle: Handle::new(Rc::new(inner), ClockTime { start_time: now }),
        }
    }

    /// Select this timer for a closure, restoring the previous selection on
    /// return or unwind. The reactor uses this scope around task execution.
    pub fn with<R>(&self, f: impl FnOnce() -> R) -> R {
        self.handle.with(f)
    }

    /// Process timers through `now` and wake the tasks whose deadlines elapsed.
    /// Call between task turns, including while tasks remain runnable. Time
    /// before the wheel's last processed tick is clamped to that tick.
    pub fn advance(&self, now: Instant) {
        let now = self.handle.time_source().instant_to_tick(now);
        loop {
            let waker = {
                let mut state = self.handle.get().state.borrow_mut();
                let now = now.max(state.wheel.elapsed());
                let Some(entry) = state.wheel.poll(now) else {
                    break;
                };
                // The entry is unlinked before it is fired, and only the
                // owning thread accesses the timer's intrusive lists.
                unsafe { entry.fire(Ok(())) }
            };
            // A wake can reset or drop another timer. Release the wheel borrow
            // before invoking arbitrary user wakeup code.
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    /// The next absolute instant at which the wheel needs processing, or None
    /// when no timer is registered. This can precede the earliest user deadline
    /// when a coarse bucket must cascade into a finer wheel. Recheck after
    /// advancing time or running tasks, since they may reset or cancel timers.
    pub fn next_wake(&self) -> Option<Instant> {
        self.handle
            .get()
            .state
            .borrow()
            .wheel
            .next_expiration_time()
            .map(|tick| self.handle.time_source().tick_to_instant(tick))
    }
}

impl Handle {
    /// Unlink an entry and mark it cancelled without waking its task.
    ///
    /// The timer will be moved to the cancelled state. Wakers will _not_ be
    /// invoked. If the timer is already completed, this function is a no-op.
    ///
    /// # Safety
    ///
    /// `entry` must point to a live, immovable entry belonging to this timer.
    /// It must remain valid until cancellation returns. All list mutations
    /// happen on this thread while the wheel is mutably borrowed.
    pub(self) unsafe fn clear_entry(&self, entry: NonNull<TimerShared>) {
        // SAFETY: The caller keeps the entry alive and immovable. Removal uses
        // its cached membership before fire marks it unregistered.
        let waker = unsafe {
            let mut state = self.get().state.borrow_mut();
            if entry.as_ref().might_be_registered() {
                state.wheel.remove(entry);
            }
            entry.as_ref().handle().fire(Ok(()))
        };
        // A waker's destructor can drop another sleep. Release the wheel
        // borrow before dropping it so that sleep can unlink its own entry.
        drop(waker);
    }

    /// Remove and re-add an entry at its new deadline.
    ///
    /// # Safety
    ///
    /// `entry` must be live and pinned, and either unregistered or registered
    /// with this timer. Its owner must keep it pinned until cancellation.
    pub(self) unsafe fn reregister(&self, new_tick: u64, entry: NonNull<TimerShared>) {
        // SAFETY: The caller keeps the entry alive and pinned. The wheel is
        // borrowed for the entire remove/update/insert transition.
        let waker = unsafe {
            let mut state = self.get().state.borrow_mut();

            if entry.as_ref().might_be_registered() {
                state.wheel.remove(entry);
            }

            // The entry is detached; its handle can now update membership.
            let entry = entry.as_ref().handle();

            entry.set_expiration(new_tick);

            match state.wheel.insert(entry) {
                Ok(_) => None,
                Err((entry, super::error::InsertError::Elapsed)) => entry.fire(Ok(())),
            }
        };

        // The timer was fired synchronously as a result of the reregistration.
        // Wake the waker; this is needed because we might reset _after_ a poll,
        // and otherwise the task won't be awoken to poll again.
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl fmt::Debug for Inner {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Inner").finish()
    }
}
