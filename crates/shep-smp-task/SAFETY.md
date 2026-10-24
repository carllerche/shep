# Local task safety

`Task` and `JoinHandle` stay on their creating thread (including Send outputs).
`Task::run` requires exclusive access to the owner. Standard wakers hold only an
`Arc<Notification>` containing atomic state, a unique ID and the Send + Sync
scheduler. They have no pointer to the task cell, future, output or join waker.
The scheduler queues IDs; it must not poll the task in its wake callback.
Stale IDs must be ignored by the executor.

## Allocation and type erasure

`RawTask::new` leaks a boxed `Cell<F>` into a non-null header pointer. `Cell`
is repr(C), with Header first. Its vtable is selected from exactly F. The two
local owners are Task and JoinHandle; copying RawTask does not acquire another
reference. The packed reference count starts at two. Every owner releases it
once, and only the final release calls the matching F vtable's deallocator.
Wakers own the separate notification allocation and never keep the cell alive.

The raw-pointer dereference in Harness is shared by its header/core/trailer
accessors. No mutable reference to the entire cell is created. The original
allocation pointer is retained for Box::from_raw. No cell reference may be used
after the final release. The header access in RawTask has the same owner-liveness
requirement. Construction installs the vtable for the matching future type; private unsafe
vtable entry points require that callers preserve that pairing. JoinHandle<T> is constructed only with the RawTask for a future
whose Output is T; its erased stack output slot is restored to that exact type
by the vtable. These are the remaining raw dispatch and ownership obligations.

## Pinned future storage

The future is stored inline in the cell's private stage, with no additional
future allocation. The cell stays at one heap address until deallocation.
There are only two unsafe operations in core.rs:

- `with_stage` converts the UnsafeCell pointer to an exclusive reference for
  the duration of a private closure. The reference cannot escape the closure.
- `poll` pins the Running future at that stable address.

Only the owner accesses the stage. Polling, cancellation and future destruction
happen while RUNNING; reentrant join detachment in that state only removes join
interest and its waker. It cannot borrow or drop the stage. A wake from any
thread only updates notification state and may enqueue another turn.

A Running stage is never moved out. Assignment drops its future in place, then
installs the replacement even if Drop unwinds. In particular, replacing this
assignment with mem::replace or UnsafeCell::get().read() would violate pinning.
Output extraction verifies Finished before moving it. Finished outputs are not
pinned and are moved out before their destructors run. Panicking future polls
and drops are contained by the existing harness catch_unwind boundaries.

## Join waker storage

The trailer uses Cell<Option<Waker>>. Registration replaces the slot before
dropping the old waker; completion/detachment take it before invoking user code.
Matching takes and restores the slot around Waker::will_wake, which does not
invoke user callbacks. These operations need neither raw references nor a
runtime borrow check. The existing notification state protocol and atomic
orderings are retained.

## Verification

Integration tests cover local non-Send values and outputs, pinned poll/drop
addresses, panic cleanup, self-detachment, repeated/same/replaced join wakers,
a panicking waker destructor, foreign wake coalescing, wakes during polling,
wakes racing with cancellation and wakes after deallocation. Compile-fail docs
check that Task and JoinHandle are neither Send nor Sync. Run these under Miri
as well as the native test runner; Miri samples executions rather than proving
all possible atomic interleavings. Benchmarks and measured alternatives are in
benches/README.md and benches/RESULTS.md.
