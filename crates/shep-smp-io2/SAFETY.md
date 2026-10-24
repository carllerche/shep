# Native reactor safety boundary

All four unsafe blocks in the new I/O/runtime stack live in the private core
module of shep-smp-io2: two in core.rs and two in core/park.rs. The crate root
denies unsafe code by default and core explicitly opts in. The io, arena, buffer
and park_state modules each forbid unsafe code; shep-smp2 also forbids it.

The safe boundary is now a module boundary within one I/O crate. Callers supply
no raw pointers, SQEs, raw descriptors, borrowed buffers or unsafe buffer-trait
implementations. Arena mutation/retirement helpers are visible only inside
core, preserving the former crate boundary: safe sibling modules cannot
manufacture CQE retirement or retrieve a submitted operation's memory. The
public crate reexports only the buffer, wake handle, wait outcome and safe I/O
facade. Existing task internals remain in shep-smp-task; this document covers
the new kernel boundary, not transitive dependencies.

## Ownership through completion

A data SQE is constructed only from an owned Operation. Its initialized Buffer
has one stable backing allocation; moving the Buffer or Operation never moves
those bytes. The operation owns an Arc<File>, retaining its descriptor. Before
publishing an SQE, admission stores the operation and its lifecycle in a single
Slab<Request>. Only the safe core submission method can construct/publish SQEs.
The future keeps a shared driver handle and slab index, without a per-operation
allocation or mailbox. No public method exposes submitted buffer bytes.

Admission immediately publishes the SQE. The reactor later batches the actual
submission syscall. There is no software pending-submission queue. If SQ push
fails, no kernel reference was published; the fresh entry is removed and all
owned values are returned to the caller. A later enter error never justifies
freeing already-published memory. Reads expose initialized memory on short/error
results; no uninitialized length manipulation is needed.

The default ring enables no SQPOLL, multishot, zero-copy send notifications,
registered buffers, or buffer selection. Each data operation has one terminal
CQE. SQE user_data is its slab index; data indices exclude the two control
tokens. Indices are scoped to their Operations handle, never exposed externally.

The slab entry is shared state between the driver and operation future. Its
lifecycle tracks which party still needs it:

| State | Future action | Terminal CQE action |
|---|---|---|
| Submitted | Poll installs Waiting; drop sets Ignored | Store Completed |
| Waiting(waker) | Repoll replaces waker; drop sets Ignored and releases waker | Store Completed and extract waker |
| Ignored | Future has already released its index | Remove entry and release operation |
| Completed(result) | Poll takes values and removes entry; drop removes entry | Driver has already released its index |

Removal requires both driver retirement and consumer release. The last party
to finish removes the entry. There are no Consumed placeholders, deferred
reclamation lists, generations or permanent tombstones. A future clears its
index before running any destructor after consuming or abandoning the entry.
Submitted includes SQEs published but not yet submitted with io_uring_enter;
dropping such a future still retains its operation until its real CQE.

The reactor processes its CQ snapshot and extracts all result wakers before
invoking any callbacks. Notification batches contain owned Wakers, never slab
indices. A callback may consume another completed result and immediately reuse
its slot; the driver no longer looks up the old index. Replaced/released wakers
and discarded operations are destroyed after arena/ring RefCell borrows end.
One Rc owns the shared arena and optional ring; there is no unsafe shared cell.
The reactor drains or retains unresolved resources and closes the ring on Drop.
Completed results stay in the shared slab for futures that outlive the reactor,
and those futures can remove their entries without a subsequent driver turn.

Only published operations awaiting CQEs consume kernel admission. Completed
results may grow the slab beyond the kernel in-flight bound without blocking
new admission. This preserves progress while consumers retain completed values.

The lifecycle follows Monoio's Submitted/Waiting/Ignored/Completed model
(reviewed at commit 8675e9406ea7943c441d969e38c3f5fc198ef6d1). Monoio normally
keeps typed operation data in the future and transfers it to boxed ignored state
on cancellation. Here the owned Operation stays in the slab throughout, avoiding
that type-erased allocation and an unsafe operation/buffer trait boundary.
Extensions with additional CQEs or cancel requests must account for every
remaining kernel reference before allowing index reuse.

## Unsafe sites

1. `park::Event::new`: eventfd creation has no pointer arguments. The negative
   error result is checked before a successful fresh descriptor is adopted.
2. `File::from_raw_fd`: adopts that fresh eventfd exactly once. All subsequent
   reads/writes and closure use standard safe Rust file ownership/I/O.
3. Data SQ push: the private slab already owns the buffer and descriptor, and
   retains them through the terminal CQE or the conservative failure cleanup.
4. Control SQ push: only private one-shot PollAdd/PollRemove builders reach it.
   Neither contains a userspace memory pointer. The eventfd owner remains alive
   until the watch/removal completions have retired (or ring destruction).

There are no unsafe Send/Sync impls, unchecked projections, custom allocation
operations, raw wakers, or mutable pointers returned through the public API.
The initialized, aligned Buffer itself forbids unsafe code. It overallocates
by at most alignment - 1 bytes and selects an aligned initialized slice using
safe indexing; it cannot reallocate after construction.

## Capacity and CQ ownership

At most capacity data operations await terminal CQEs; completed results may
remain in additional slab slots without consuming kernel capacity. The SQ reserves two additional
entries for one wake watch and one watch-removal operation; default CQ capacity
is at least sufficient for all these outstanding single-shot completions.
Control tokens occupy reserved u64 values disjoint from every valid data index.
Only one watch and one removal can be outstanding. Removing an already-ready
watch may return ENOENT; the original watch CQE must still be reaped.

The entire CQ snapshot is retired before any abandoned operation is destroyed
or any result waker is invoked. Returning early after consuming an unprocessed
CQE would strand its request. Progress and ProgressError retain owned wakers
and the retired-operation count, including when a control CQE reports an error.
Completed entries remain accessible only through their futures' live indices.
Test-only helpers also collect owned Completion records; their slab scans are
not part of the production path.

The facade wakes result waiters after releasing both ring and arena borrows,
then wakes capacity waiters even if every retired operation was ignored. The
retirement count comes directly from CQEs, so reentrant submissions do not
change it. There is no separate reply map or second slab. Fatal Drop cleanup
retains unresolved owned operations before closing the ring.

## Parking and cross-thread notifications

The eventfd is nonblocking, without semaphore mode. Every read/write retries
EINTR; EAGAIN on write means the counter already retains a readable token.
A single AtomicU8 implements four states with SeqCst loads and CAS operations:

| State | Meaning |
|---|---|
| RUNNING | The owner is executing; no eventfd write is needed |
| PARKING | The owner has published its intent to block before its final check |
| NOTIFIED | A retained wake permit; further writers coalesce |
| CLOSED | Terminal; no transition may reopen the reactor |

A producer first publishes its task ID, then CASes the state to NOTIFIED. Only
PARKING -> NOTIFIED claims an eventfd write, and only for a foreign thread.
RUNNING -> NOTIFIED retains a permit without a syscall. NOTIFIED -> NOTIFIED
still executes an RMW: publication must linearize against the consumer's state
transitions, rather than relying on an observational load of an old permit.

The owner arms the watch and drains stale eventfd tokens before the parking
transition. It CASes RUNNING -> PARKING, then submits and performs the final
inbox/deadline check. If the previous state was already NOTIFIED, it instead
consumes that permit with NOTIFIED -> RUNNING and marks this attempt to skip
waiting; there is no reason to advertise sleep for an attempt already cancelled.
Park::wait also checks for a notification arriving after preparation. It skips
entering the kernel when notified. Otherwise a racing foreign producer that
observes PARKING must write, so the token either precedes the kernel wait and
is already readable, or interrupts that wait. The descriptor stays watched
through the gap between the final state check and entering the kernel.

Park's Drop guard restores RUNNING with an RMW after a wait, timeout, error,
unwind, or abandoned final check. That acquire observes publications preceding
a notification it consumes; publications after it leave NOTIFIED for the next
prepare. CLOSED is never overwritten by finish. A writer that claimed a signal
before the owner resumed may finish its write afterward; this harmless stale
counter is drained before a later attempt. Thus the claim is conditional on the
observed parking state, not knowledge of the exact instant the owner is blocked.

Owner-thread notifications never write, even if the owner calls unpark between
prepare and wait: their NOTIFIED state makes that subsequent wait skip. They
cannot execute concurrently with a blocking syscall on the same OS thread.
The executor additionally routes same-context wakes directly to its ready queue
through scoped TLS. The runtime enters that context around its entire loop,
including I/O completion delivery, timer advancement and shutdown. The identity
check prevents a nested/different executor from using the wrong local queue.
Same-thread mismatches publish through the target inbox without invoking its
driver waker; the target owner is paused, not concurrently blocked in its ring.

WakeHandle holds Weak<Event>. An upgraded writer owns the descriptor through
its write, protecting against fd reuse during concurrent shutdown. The CLOSED
state prevents new notifications. No handle contains a pointer to local task,
buffer, executor context or reactor state. TLS uses safe Weak references and
RAII scope restoration; it adds no unsafe scoped-reference lifetime machinery.

The final inbox check is a liveness requirement, not an unsafe caller obligation.
Absolute Instant deadlines are recomputed on each wait; interrupted waits return
to service tasks and timers. Extended enter arguments avoid kernel-visible
userspace timeout buffers and accumulating timeout SQEs.

Native tests inspect the actual eventfd counter: running/owner wakes write
zero, concurrent parked wakes coalesce to one, and delayed foreign writes
interrupt a prepared wait. They force notifications before preparation, after
the final check, during parking and across reactor destruction. The runtime test
counts driver-waker calls during local I/O/timer/channel completions and asserts
zero. Scoped-context tests also assert that matching wakes enter the ready queue
directly, without an inbox message. These tests exercise selected race windows;
they are not an exhaustive proof of every concurrent execution.

## Shutdown and fatal errors

Normal runtime shutdown closes task admission and cancels consumers, closes I/O
admission, submits/drains accepted operations, removes/reaps the wake watch,
then closes the eventfd. Application future cancellation never frees a
kernel-visible allocation or releases its admission charge early.

Drop also drains and may block; an operation on a non-completing descriptor can
therefore delay shutdown indefinitely. Data operations are currently file
read/write/fsync. This first version does not provide forced kernel-operation
cancellation, sockets, or arbitrary descriptor polling to callers.

If kernel retirement cannot be established after a fatal enter/control error,
Drop leaks unresolved owned operations before ring destruction rather than
assuming that close makes their buffers safe. Safe facade futures whose buffers
cannot be recovered fail by panic after being woken; the driver reports the
terminal I/O error. Facade shutdown never waits on result counters after that
terminal error; the core independently drains or retains its actual resources.
There is no silent fallback to a different I/O backend.

## Validation limits

Native tests exercise real io_uring/eventfd operations and force notifications
before and after the final check, rearming, timeout and destruction. Capacity,
file-position semantics, short/error/zero-length results, fd retention,
cancellation and shutdown are covered. Safe buffer and lifecycle state transitions also run under Miri. The
facade callback/admission tests run natively because they drive real io_uring. Miri cannot execute io_uring, so it does not validate
the kernel's memory accesses. Kernel ABI and the io-uring crate remain trusted.
Fatal ring failures are reasoned about here; they are not fault-injected by the
current native tests. Unsafe mode extensions require revisiting these proofs.
