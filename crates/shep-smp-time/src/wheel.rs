// Extracted from Monoio and adapted for shep-smp-time; see FORK.md.
// Copyright (c) 2021 ihciah, dyxushuai and other Monoio Contributors.
// Portions Copyright (c) 2021 Tokio Contributors. Licensed under MIT.

use shep_util_slab::Slab;

use crate::{
    entry::{Entry, EntryId, EntryList},
    error::InsertError,
    level::{Expiration, Level},
};

/// Timing wheel implementation.
///
/// Buckets contain IDs into the owner's slab. A slot stays occupied until its
/// Sleep is dropped, and is unlinked before reuse. No IDs leave a transition
/// across user callbacks after their owner could be destroyed.
#[derive(Debug)]
pub(crate) struct Wheel {
    /// The number of milliseconds elapsed since the wheel started.
    elapsed: u64,

    /// Timer wheel.
    ///
    /// Levels:
    ///
    /// * 1 ms slots / 64 ms range
    /// * 64 ms slots / ~ 4 sec range
    /// * ~ 4 sec slots / ~ 4 min range
    /// * ~ 4 min slots / ~ 4 hr range
    /// * ~ 4 hr slots / ~ 12 day range
    /// * ~ 12 day slots / ~ 2 yr range
    levels: Box<[Level; NUM_LEVELS]>,

    /// Entries queued for firing
    pending: EntryList,
}

/// Number of levels. Each level has 64 slots. By using 6 levels with 64 slots
/// each, the timer is able to track time up to 2 years into the future with a
/// precision of 1 millisecond.
pub(crate) const NUM_LEVELS: usize = 6;

/// One rotation of the hierarchy; longer deadlines wrap through the top level.
const MAX_DURATION: u64 = (1 << (6 * NUM_LEVELS)) - 1;

impl Wheel {
    /// Create a new timing wheel
    pub(crate) fn new() -> Wheel {
        let levels = Box::new([
            Level::new(0),
            Level::new(1),
            Level::new(2),
            Level::new(3),
            Level::new(4),
            Level::new(5),
        ]);

        Wheel {
            elapsed: 0,
            levels,
            pending: EntryList::new(),
        }
    }

    /// Return the number of milliseconds that have elapsed since the timing
    /// wheel's creation.
    pub(crate) fn elapsed(&self) -> u64 {
        self.elapsed
    }

    /// Insert a live, detached entry from the owner's slab.
    pub(crate) fn insert(
        &mut self,
        entries: &mut Slab<Entry>,
        key: EntryId,
    ) -> Result<u64, (EntryId, InsertError)> {
        let when = entries[key.index()].sync_when();
        if when <= self.elapsed {
            return Err((key, InsertError::Elapsed));
        }
        let level = self.level_for(when);
        self.levels[level].add_entry(entries, key);
        debug_assert!({
            self.levels[level]
                .next_expiration(self.elapsed)
                .map(|e| e.deadline >= self.elapsed)
                .unwrap_or(true)
        });
        Ok(when)
    }

    /// Remove a registered or pending entry without releasing its slab slot.
    pub(crate) fn remove(&mut self, entries: &mut Slab<Entry>, key: EntryId) {
        if entries[key.index()].is_pending() {
            self.pending.remove(entries, key);
        } else {
            let when = entries[key.index()].cached_when();
            debug_assert!(
                self.elapsed <= when,
                "elapsed={}; when={}",
                self.elapsed,
                when
            );
            let level = self.level_for(when);
            self.levels[level].remove_entry(entries, key);
        }
    }

    /// Advances the timer up to the instant represented by `now`.
    pub(crate) fn poll(&mut self, entries: &mut Slab<Entry>, now: u64) -> Option<EntryId> {
        loop {
            if let Some(handle) = self.pending.pop_back(entries) {
                return Some(handle);
            }

            match self.next_expiration() {
                Some(ref expiration) if expiration.deadline <= now => {
                    self.process_expiration(entries, expiration);

                    self.set_elapsed(expiration.deadline);
                }
                _ => {
                    // in this case the poll did not indicate an expiration
                    // _and_ we were not able to find a next expiration in
                    // the current list of timers.  advance to the poll's
                    // current time and do nothing else.
                    self.set_elapsed(now);
                    break;
                }
            }
        }

        self.pending.pop_back(entries)
    }

    /// Returns the instant at which the next timeout expires.
    fn next_expiration(&self) -> Option<Expiration> {
        if !self.pending.is_empty() {
            // Expire immediately as we have things pending firing
            return Some(Expiration {
                level: 0,
                slot: 0,
                deadline: self.elapsed,
            });
        }

        // Check all levels
        for level in 0..NUM_LEVELS {
            if let Some(expiration) = self.levels[level].next_expiration(self.elapsed) {
                // There cannot be any expirations at a higher level that happen
                // before this one.
                debug_assert!(self.no_expirations_before(level + 1, expiration.deadline));

                return Some(expiration);
            }
        }

        None
    }

    /// Returns the tick at which this timer wheel next needs to perform some
    /// processing, or None if there are no timers registered.
    pub(crate) fn next_expiration_time(&self) -> Option<u64> {
        self.next_expiration().map(|ex| ex.deadline)
    }

    /// Used for debug assertions
    fn no_expirations_before(&self, start_level: usize, before: u64) -> bool {
        let mut res = true;

        for l2 in start_level..NUM_LEVELS {
            if let Some(e2) = self.levels[l2].next_expiration(self.elapsed) {
                if e2.deadline < before {
                    res = false;
                }
            }
        }

        res
    }

    /// iteratively find entries that are between the wheel's current
    /// time and the expiration time.  for each in that population either
    /// queue it for notification (in the case of the last level) or tier
    /// it down to the next level (in all other cases).
    pub(crate) fn process_expiration(
        &mut self,
        entries: &mut Slab<Entry>,
        expiration: &Expiration,
    ) {
        // Detach the whole bucket before reinsertion. At the top level a
        // far-future entry can return to the same bucket on another rotation.
        let mut bucket = self.take_entries(expiration);
        while let Some(key) = bucket.pop_back(entries) {
            if expiration.level == 0 {
                debug_assert_eq!(entries[key.index()].cached_when(), expiration.deadline);
            }
            match entries[key.index()].mark_pending(expiration.deadline) {
                Ok(()) => self.pending.push_front(entries, key),
                Err(tick) => {
                    let level = level_for(expiration.deadline, tick);
                    self.levels[level].add_entry(entries, key);
                }
            }
        }
    }

    fn set_elapsed(&mut self, when: u64) {
        assert!(
            self.elapsed <= when,
            "elapsed={:?}; when={:?}",
            self.elapsed,
            when
        );

        if when > self.elapsed {
            self.elapsed = when;
        }
    }

    /// Obtains the list of entries that need processing for the given
    /// expiration.
    fn take_entries(&mut self, expiration: &Expiration) -> EntryList {
        self.levels[expiration.level].take_slot(expiration.slot)
    }

    fn level_for(&self, when: u64) -> usize {
        level_for(self.elapsed, when)
    }
}

fn level_for(elapsed: u64, when: u64) -> usize {
    const SLOT_MASK: u64 = (1 << 6) - 1;

    // Mask in the trailing bits ignored by the level calculation in order to cap
    // the possible leading zeros
    let mut masked = elapsed ^ when | SLOT_MASK;

    if masked >= MAX_DURATION {
        // Fudge the timer into the top level
        masked = MAX_DURATION - 1;
    }

    let leading_zeros = masked.leading_zeros() as usize;
    let significant = 63 - leading_zeros;

    significant / 6
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_level_for() {
        for pos in 0..64 {
            assert_eq!(0, level_for(0, pos), "level_for({pos}) -- binary = {pos:b}");
        }

        for level in 1..5 {
            for pos in level..64 {
                let a = pos * 64_usize.pow(level as u32);
                assert_eq!(
                    level,
                    level_for(0, a as u64),
                    "level_for({a}) -- binary = {a:b}"
                );

                if pos > level {
                    let a = a - 1;
                    assert_eq!(
                        level,
                        level_for(0, a as u64),
                        "level_for({a}) -- binary = {a:b}"
                    );
                }

                if pos < 64 {
                    let a = a + 1;
                    assert_eq!(
                        level,
                        level_for(0, a as u64),
                        "level_for({a}) -- binary = {a:b}"
                    );
                }
            }
        }
    }
}
