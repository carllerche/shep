// Extracted from Monoio and adapted for Shep, commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

//! Reactor-owned timer wheel

use std::{cell::RefCell, fmt, rc::Rc};

use shep_util_slab::Slab;

use crate::time::{Duration, Instant, entry::Entry, wheel::Wheel};

/// A core-local timer wheel owned and advanced by the reactor.
///
/// [`advance`](Self::advance) processes elapsed time and wakes expired timers.
/// [`next_wake`](Self::next_wake) tells the reactor when the wheel next needs
/// processing. This type performs no waiting or I/O. Timer futures register
/// with the timer of the runtime entered when they are created.
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
/// [sleep]: crate::time::Sleep
/// [timeout]: crate::time::Timeout
/// [interval]: crate::time::Interval
#[derive(Debug)]
pub struct Timer {
    handle: Handle,
}

/// Conversion between absolute instants and wheel-relative millisecond ticks.
#[derive(Debug)]
pub(crate) struct ClockTime {
    start_time: Instant,
}

impl ClockTime {
    pub(crate) fn deadline_to_tick(&self, t: Instant) -> u64 {
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
pub(crate) struct Inner {
    time_source: ClockTime,
    pub(crate) state: RefCell<InnerState>,
}

pub(crate) struct InnerState {
    pub(crate) wheel: Wheel,
    pub(crate) entries: Slab<Entry>,
}

impl Timer {
    pub(crate) fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Create an empty wheel with `now` as its epoch, reserving 1,024 entry
    /// slots. The reactor supplies the same monotonic clock for subsequent
    /// calls to [`advance`](Self::advance).
    pub fn new(now: Instant) -> Self {
        Self::with_capacity(now, 1024)
    }

    /// Create a wheel reserving space for at least `capacity` entries.
    /// Sleeps occupy slots on first poll/reset and retain them until Drop,
    /// including after expiration. This is a reservation, not a limit; the
    /// arena grows as needed and retains its capacity for reuse.
    pub fn with_capacity(now: Instant, capacity: usize) -> Self {
        let inner = Inner {
            time_source: ClockTime { start_time: now },
            state: RefCell::new(InnerState {
                wheel: Wheel::new(),
                entries: Slab::with_capacity(capacity),
            }),
        };
        Self {
            handle: Handle {
                inner: Rc::new(inner),
            },
        }
    }

    /// The number of entry slots available without growing the arena.
    /// This includes occupied slots retained by elapsed sleeps.
    pub fn capacity(&self) -> usize {
        self.handle.get().state.borrow().entries.capacity()
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
                let InnerState { wheel, entries } = &mut *state;
                let Some(key) = wheel.poll(entries, now) else {
                    break;
                };
                // Finish the transition and consume the key before callbacks
                // can cancel an owner and reuse its slot.
                entries[key.index()].fire()
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

impl fmt::Debug for Inner {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Inner").finish()
    }
}

/// Local handle to a reactor-owned timer wheel.
#[derive(Clone)]
pub(crate) struct Handle {
    inner: Rc<Inner>,
}

impl Handle {
    /// Returns the time source associated with this handle
    pub(crate) fn time_source(&self) -> &ClockTime {
        &self.inner.time_source
    }

    /// Access the driver's inner structure
    pub(crate) fn get(&self) -> &Inner {
        &self.inner
    }

    /// The timer of the runtime entered on this thread.
    ///
    /// # Panics
    ///
    /// Panics outside a runtime context.
    pub(crate) fn current() -> Self {
        crate::rt::context::with_current(|rt| rt.shared().timer.clone())
            .expect("timers must be created from within a shep_smp2 runtime")
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Handle")
    }
}
