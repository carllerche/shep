# Wheel-owned indexed entries

The timer owns entries in `shep_util_slab::Slab<Entry>` and enforces
`#![forbid(unsafe_code)]`. The reactor still assembles a concrete Timer with
`with`, `advance`, and `next_wake`; there is no driver trait or waiting here.

The implementation uses flat sibling modules in `src/`: `timer.rs` owns the
clock, scoped handle, and shared state; `entry.rs` keeps registration state and
its ID list together; `wheel.rs` and `level.rs` implement the hierarchy;
`sleep.rs` provides the consumer future. Entry links stay private to `entry.rs`.

## Ownership

```text
Sleep:      owning timer handle + deadline + optional entry ID
TimerState: wheel buckets/pending queue + Slab<Entry>
Entry:      registered/pending/elapsed state + cached deadline + waker + links
```

Each ID wraps a zero-based slab key plus one in a NonZeroUsize, so optional
links occupy one word. Bucket heads/tails and previous/next links contain IDs.
Every lookup is checked. Growing the slab can move entries without changing
keys; there are no pointers into either futures or slab storage.

Unpolled sleeps have no slot. First poll or reset allocates/reuses a slot.
An expired sleep retains its slot until Drop, allowing resets and interval
ticks to reuse it. Drop unlinks first, removes the entry from the slab, then
drops the removed value after releasing the owner borrow. Free slots retain
no consumer/waker data. The existing local Rc handle keeps state alive until
the last consumer is gone, even if the public Timer owner was dropped first.

The wheel retains its six-level, 64-slot hierarchy, rounding, lazy deadline
extensions, constant-time removal, and top-level wrapping behavior. Entry fields
are plain data under one timer-state RefCell; per-entry Cells, waker RefCells,
non-owning pointers, and state sentinels are gone. Consumer-side keys are ordinary
optional IDs in movable SleepState. Sleep remains !Unpin through a wrapper marker
for API compatibility and !Send/!Sync through its local owner handle; its address
has no role in the wheel. Interval stores SleepState inline without a Box.
The immutable clock epoch lives once beside the shared state, outside its RefCell;
each handle contains only an Rc. Deadline conversion needs no additional borrow.

## Reservation and retention

`Timer::new(now)` reserves at least 1,024 entries.
`Timer::with_capacity(now, capacity)` selects another reservation, including zero.
`Timer::capacity()` reports the current slot capacity for tuning and observation.
Reservation does not occupy entries or impose a limit: storage grows when full
and keeps capacity for reuse. Capacity covers all slot-owning sleeps, including
elapsed ones not yet dropped.

On the measured x86_64 build, Sleep is 32 bytes (48 before sharing the epoch),
Interval is 56 bytes with no separate allocation, and each slab slot is 56 bytes.
The default slot reservation alone requests 57,344 bytes.
Future size is not total memory usage: a full arena amortizes that reservation,
but low occupancy and bursts followed by drops retain unused capacity. The
minimal slab intentionally has no shrinking/compaction API; reclamation would
need a separate measured policy that preserves live keys.

## Key reuse and callbacks

Keys remain private and have no generation field. A live Sleep keeps its slot;
a dropped sleep is unlinked from every bucket/pending list before reuse. No
popped ID is retained across a user callback after the owner could have gone
away. The advance loop finishes its transition, extracts the waker, and releases
the borrow before calling user code.

Cloning a custom waker can also call user code. Polling inspects an existing
registration under a short borrow, clones outside the arena borrow when needed,
then reacquires and rechecks state before installation. Any unused clone is
dropped outside the borrow. Waking and destruction likewise happen outside
the owner borrow, including a waker stored by polling an already elapsed sleep.

Cascading detaches the whole bucket before reinsertion and invokes no user
callbacks, including when a far-future entry returns to the same top bucket.
These rules permit plain reusable keys. Deferred batches that retain keys after
consumer destruction would require revisiting generations or ownership.

The inherited first-waker behavior from Monoio remains; replacing
it with most-recent-waker semantics is a separate behavior change.

## Verification

The existing wheel boundary/rotation tests, absolute-deadline model, reset and
cancellation tests remain. New integration cases cover reservations 0/1/256/
1,024/4,096, growth across capacity, reuse after drop, slot retention/rearming
after expiration, and clone/destructor callbacks that expire timers, reuse slots,
grow storage, or panic. A custom clone-callback test waker has one documented
unsafe construction in the integration-test crate; runtime/library code forbids
unsafe code. Large capacity tests use small pools under Miri to avoid its
quadratic mutable-slice retagging cost; full boundaries run in the normal suite.

There are 56 timer benchmark cases, including completed-sleep rearming and
registration at/above reservations. Staged retained-heap snapshots were taken
under DHAT with an `arena_memory` example, since removed. See `../../benches/time/README.md` and the
arena results in `benches/` for timings, memory, and their measurement limits.
