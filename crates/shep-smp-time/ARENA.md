# Wheel-owned indexed entries (proposal)

The outer-cell removal and concrete entry list are implemented. This document
describes a possible next change; the indexed arena is not implemented yet.

The aim is to keep the current six-level wheel, millisecond resolution, lazy
registration/extensions, and constant-time cancellation while removing the
wheel's pointers into pinned futures. All entry storage would belong to the
timer, and all bucket/pending links would be checked indices into that storage.
The reactor would continue to own a concrete Timer and call the existing
`with`, `advance`, and `next_wake` functions. No driver trait is needed.

## Ownership and slot lifetime

Conceptually:

```text
Sleep:      timer handle + deadline + optional entry ID
TimerState: wheel buckets/pending queue + shep_util_slab::Slab<Entry>
Entry:      deadline/state + waker + previous/next IDs + bucket membership
```

Use `shep_util_slab::Slab<Entry>`, the small checked storage core now extracted
from slab 0.4.12 with `#![forbid(unsafe_code)]`. It provides reusable integer
keys, vacant-slot bookkeeping, checked access, and construction-time capacity
reservation. The utility has no normal dependencies. Its upstream comparison
tests and benchmarks are separate from this proposed timer integration; the
existing I/O caller still uses upstream slab. No additional arena abstraction,
per-entry Rc, or per-entry allocation is needed.

Wrap zero-based slab keys in an opaque nonzero entry ID (key plus one) so
`Option<EntryId>` fits in one word. Slab growth may move entries physically,
but their keys remain usable: the wheel and futures retain IDs, never references
into its backing allocation. Avoid compacting/rekeying occupied entries.

Keep construction lazy: an unpolled Sleep has no arena slot. First registration
allocates/reuses a slot. A reset before first poll follows the same registration
behavior as today. **Retain that slot until Sleep is dropped**, including after
expiration. Completion only changes its state and takes the registered waker.
This preserves cheap reset/reuse of completed sleeps and intervals, and keeps
every live Sleep's ID valid without allocating a new slot on each tick.

Dropping Sleep unlinks its entry and removes it from the slab for reuse. A free
slot retains no waker or consumer data. The arena stays alive through the
existing local Rc timer handle, so owner/consumer drop ordering remains safe.

Pinning would no longer be needed for the wheel's references. Public pin-shaped
methods can remain unchanged; relaxing Sleep's !Unpin property and removing
Interval's Box can be considered separately. Rc keeps these types !Send.

## Initial capacity

Preallocate the slab when constructing the timer with
`Slab::with_capacity(initial_capacity)`. Propose **1,024 entries per core** as an
initial benchmark candidate, with these constructor semantics:

```text
Timer::new(now)                         uses the default initial capacity
Timer::with_capacity(now, capacity)     selects an explicit initial capacity
```

The second constructor is proposed, not implemented yet. Capacity zero remains
useful for cold-growth benchmarks. Reservation does not insert dummy entries:
Sleep construction remains lazy, and a slot is occupied only on registration.
Capacity is a starting reservation, not a timer limit; allow Slab to grow when
needed and retain capacity for reuse initially.

Size the reservation for sleeps retaining slots, including elapsed sleeps not
yet dropped and interval sleeps. Measure actual entry/slab storage before
claiming a byte budget. Compare capacities 0, 256, 1,024, and 4,096, including
crossing the reserved capacity, before fixing the default based on workload.
The utility's API and reservation semantics are documented
[here](../shep-util-slab/README.md).

## Lists and key reuse

Initially keep doubly linked lists using entry IDs for head, tail, previous,
and next. A node records its bucket or pending-queue membership so reset/drop
can unlink it in constant time. Taking/cascading a slot still detaches its
contents before reinsertion, preserving correctness over top-level rotations.
Read neighbor IDs, then mutate those entries sequentially; no unchecked access
or simultaneous mutable references to different entries are necessary.

I would begin with **private indices without generations**, under these rules:

- A live Sleep retains its slot, even when elapsed.
- A slot is unlinked from every wheel/pending list before reuse.
- No popped ID is retained across a user callback; finish its state transition
  and extract its waker before releasing the timer borrow.
- IDs never escape into another component or a deferred wake batch.

Those rules prevent an old ID from observing a reused slot. Add generations if
later batching or APIs retain IDs after their owner drops or across callbacks.
Generations would then detect stale keys; they are not a substitute for
unlinking entries correctly. All lookups remain checked in either design.

## Callback boundaries

Arena ownership makes Rust's memory safety straightforward, but callback
reentrancy still needs an explicit protocol. Wake callbacks and waker
destructors can drop/reset another Sleep. Waker cloning is also a callout for
custom raw wakers.

- Unlink/fire under a short timer borrow, take the waker, release the borrow,
  then wake it.
- On cancellation, move the removed entry out of the arena, release the timer
  borrow, then drop it. This also handles a stored waker on an elapsed entry.
- Clone a replacement waker outside the arena borrow, then reacquire the borrow
  and recheck the entry state before installing it. Move any replaced waker
  out for destruction after releasing the borrow.
- Preserve a fast pending-poll path that can observe an existing registration
  without cloning a waker on every poll.

The inherited first-waker behavior is a separate behavioral issue documented
in FORK.md. Decide on that change explicitly rather than hiding it in the
storage replacement.

## Performance questions

| Area | Possible benefit | Possible cost |
|---|---|---|
| Future storage | Smaller Sleep and fewer bytes in task allocations | State moves to a separate arena; total bytes may not fall |
| Register/cancel | Reusable slots and contiguous entry storage | Bounds checks, slab bookkeeping, and growth beyond the reservation |
| Pending poll/readiness | Entry fields need no individual Cell/RefCell wrappers | Timer-state borrow and index lookup replace direct inline access |
| Lazy extension | Same constant-time deferred bucket update | Requires arena access where today's path touches inline Cells |
| Expiration/cascades | Better entry locality without per-entry allocation | Index lookup and neighbor updates still cost work |
| Memory reclamation | Freed slots can be reused immediately | Slab capacity retains the peak live population |

Keep peak capacity initially so the storage experiment remains small. Measure
grow/shrink/reuse behavior before choosing reclamation. The minimal slab has no
shrinking API; add one only if measurements justify a reclamation policy, with
tests for vacant-list maintenance and live-key stability. Do not compact/rekey
live IDs or introduce shrinking on every cancellation. A paged arena would be
a separate experiment.

Use the existing benchmarks for both cold registration and steady churn,
including inline sleeps and executor completion. Add a rearm-after-expiration
case and allocation/retained-memory measurements before evaluating the arena.
The existing absolute-deadline model, pending-queue reset, same-bucket removal,
reentrant/panicking waker tests, and Miri remain the correctness checks. Add
explicit free-slot reuse and clone-callout tests for the new storage boundary.

The elapsed-time baseline on this shared VM has substantial repeat drift.
Validate small performance claims on a stable machine or supplement them with
instruction/cache measurements. Compare indexed links first; vector buckets
with position tracking would be a separate experiment after that baseline.
