# Owned io_uring operation boundary

The crate root denies unsafe code. Only `src/driver.rs` allows it; `buffer`,
`io`, `operation`, `arena`, and `lifecycle` each forbid it. The file adapter in
shep-smp-fs2 and runtime/eventfd protocol in shep-smp2 also forbid unsafe code.
Dependencies (including io-uring and rustix) retain their own unsafe internals.

Callers supply owned operations, never raw SQEs, pointers, borrowed buffers,
raw descriptors, or unsafe buffer traits. The driver and Io are local, backed
by one shared Rc. Operation handles contain only an opaque packed ID and do not
retain the Io. A submitter cannot inspect or mutate a buffer while the kernel
owns it. Only the private driver constructs SQEs and processes
CQEs. The driver owns shared ring storage and DriverId allocation. The io module
provides the safe producer and Rejected admission errors; operation owns handles,
cancellation tokens, completion values, and request inputs. Arena owns slab
records, packed OperationIds, and retirement helpers; lifecycle defines their
submitter/completion states. These implementation modules are crate-private.

## Publication and memory ownership

`Io::push` stores an owned operation in the slab before publishing its SQE.
Buffer backing allocations, CString paths and boxed socket addresses remain
stable across operation moves and slab growth. Arc<File> retains input
file descriptors. Buffer storage is initialized, so short reads and errors
never expose uninitialized memory. Lengths fit the kernel's signed result size;
positional offsets reject the current-position sentinel and signed overflow.

Push performs no submit, reap, wait, or callback. If validation, admission or
SQ push fails, the unsubmitted operation is returned with its owned inputs.
After successful publication, an enter error does not justify freeing inputs.
The ring uses no SQPOLL. Submit and reap are separate explicit phases.

The supported operations are Nop, positional ReadAt/WriteAt, Fsync, Fallocate,
Ftruncate, OpenAt, Close, Socket, Accept, Connect, Send, Recv, Shutdown, PollAdd,
and Cancel. They produce one terminal CQE each. No public flags enable
multishot, zero-copy sends, registered buffers, buffer selection, linked SQEs,
CQE skipping, or arbitrary kernel commands. Adding such features requires
implementing their complete lifetime protocol first; a CQE with MORE is rejected
as an internal invariant violation.

## Shared lifecycle and cancellation identities

Each driver receives a process-unique nonzero 24-bit DriverId from a static
atomic counter; its Io clones share that identity.
Allocation stops permanently at exhaustion; IDs never wrap or recycle. The
OperationId packs that owner into the high 24 bits and the slab index into the
low 40 bits. NonZeroU64 makes the optional handle exactly eight bytes. Slab
index overflow rejects admission before publication. All handle methods that
access the slab validate the supplied Io before indexing, including release and
cancel_target; a mismatch panics without consuming the original claim. CQE
identities are checked too. Zero is reserved for the driver's single internal
teardown cancellation; it never indexes the slab. The index remains protected by the lifecycle and
cancellation pins; owner bits are not per-slot generations.

Each slab record owns its operation, lifecycle state, and count of remaining
kernel identity references. An accepted SQE contributes one reference until
its terminal CQE. Its submitter holds a separate claim until consumption or
explicit release(&Io). A bare handle has no Drop access to the slab. File
adapters install borrowed-Io cleanup guards before awaiting, releasing their
claim if the future is dropped. Forgetting to release a raw handle retains its
record until the Io is destroyed; it cannot free kernel-owned data early.

| State | Submitter action | Terminal CQE action |
|---|---|---|
| Submitted | Poll installs Waiting; release sets Ignored | Store Completed |
| Waiting(waker) | Replace waker, or release and set Ignored | Extract waker and store Completed |
| Ignored | Submitter has released its claim | Retain any owned result until kernel references are gone |
| Completed(result) | Consume owned values or abandon them | Original CQE already retired |

A slot is removed only after the submitter releases its claim AND every kernel
identity reference retires. Completion consumption can take inner values while
an outstanding cancellation still pins the record. This is why the inner
operation is optional: the slot remains occupied for that cancellation's
identity after its original operation's values have been returned.

CancelTarget is an owned, same-ring identity reference, created from a live
handle. Acquiring it pins the target slot even before cancellation publication.
Its drop releases an unsubmitted reference; a cancellation CQE releases a
submitted reference. If the target completes and is consumed first, the slot
cannot be reused for another SQE that a delayed cancellation could accidentally
match. No generations or publicly reusable integer tokens are needed. Rejected
cancellations return the token, preserving ownership. A retired token cannot
be resubmitted.

Only operations awaiting CQEs count against kernel capacity. Retained completed
results and cancellation pins can keep additional slab entries occupied.
One cancellation SQE has reserved capacity so cancellation can make progress at
the ordinary limit. A second concurrent cancellation is rejected immediately.
The physical SQ and default CQ accommodate this single-shot outstanding bound.
During teardown, after pending SQEs have been submitted and admission has
failed, one additional internal cancellation may be outstanding. The default
CQ also accommodates that extra completion.
There are no admission waiters or software submission backlogs.

The driver retires an entire CQ snapshot and extracts owned wakers before
calling callbacks. Driver::reap_into appends notifications to reusable caller-owned
Progress storage and returns a per-call completion count. Progress contains
wakers, never old slab IDs. Reentrant wakes
can consume other completed results and submit new operations safely. Replaced
wakers, abandoned operations, and discarded completion results are destroyed
after ring and arena RefCell borrows end. A panicking wake is contained so other
notifications are still delivered. Applications must call Progress::wake to
notify registered waiters; wake drains notifications while retaining allocation.
Removed records are collected in driver-owned scratch storage and cleared only
after ring and arena borrows end.

## Four unsafe sites

1. SQ push publishes pointers into already-owned, stable operation data. Slab
   ownership and lifecycle references retain it until terminal completion.
2. Successful OpenAt/Socket/Accept results adopt fresh descriptors using
   File::from_raw_fd. Adoption occurs even for ignored operations, so the owned
   result is closed when its last lifecycle reference is released.
3. A canceled Close re-adopts its transferred descriptor exactly once. Close
   accepts a unique File and transfers it with into_raw_fd after successful SQ
   publication, suppressing its destructor. Successful close may let the kernel
   reuse that number before the CQE is reaped, so the completed request must
   never close it again. ECANCELED means CLOSE did not run; that descriptor is
   re-adopted and closed. Other close results follow Linux close ownership
   semantics and are not retried.

4. Teardown publishes pointer-free cancellation SQEs directly. Admission has
   failed, so no target identity can be reused. Previously queued SQEs are
   submitted first, and only one internal cancellation is outstanding at a time.
   Its reserved zero user_data is handled without a slab lookup. The original
   requests retain their records and inputs until their own terminal CQEs.

There are no unsafe Send/Sync implementations, custom allocation operations,
unchecked projections, raw wakers, or public mutable kernel pointers. The
initialized aligned Buffer selects a stable slice using safe allocation and
indexing only.

## Shutdown and failure

Driver::drain closes ordinary admission and explicitly submits/reaps accepted
operations. Higher layers must cancel non-completing receives/watches first;
drain intentionally waits for terminal CQEs. It does not imply fsync, resource
close, or application flush semantics. Runtime parking cancels its own eventfd
watch through ordinary Cancel before draining.

Driver drop fails all admission, including public cancellations, and submits
queued SQEs before issuing targeted cancellations. It scans slab storage once
without allocating an identity vector, skipping completed and cancellation
records. Internal cancellations use their own SQEs even if the public reserve
is occupied. Each cancellation CQE is reaped before issuing the next; ENOENT
and EALREADY are allowed because original completion can race cancellation.
After issuing cancellations, drop blocks until every original terminal CQE has
been reaped, including accepted public cancellation requests. Cancel completion
alone never proves that a target's inputs can be freed.

Only after retirement does drop close the ring and deliver accumulated waiter
notifications outside borrows. Completed results remain consumable through
retained Io clones, and unsubmitted CancelTargets can release their remaining
identity pins afterward. There is no intentional unresolved-payload leak.
Destruction may block indefinitely if an operation cannot finish or be canceled.
A fatal enter/cancel error or panic before retirement aborts the process; it
must not unwind and free kernel-owned inputs. Transient enter errors are retried.
Fatal enter failures are not currently fault-injected in native tests.

The runtime's RUNNING/PARKING/NOTIFIED/CLOSED atomic handshake, owner-thread
notification suppression, and scoped executor TLS live above this boundary.
Safe rustix eventfd creation needs no unsafe block here. Weak wake handles retain
the event descriptor during racing writes. The watch's Arc<File> independently
retains it through its CQE; cancellation pins that watch's slab identity.

## Verification limits

Native tests cover split push/submit/reap phases, batching, immediate rejection,
owned buffers and descriptors, typed fd outputs, positional file operations,
socket operations, cancellation at capacity, stale cancellation identity,
reentrant and panicking wakers, result retention, and parking races.
Pure lifecycle and buffer tests run under Miri; Miri cannot validate kernel
accesses. Kernel ABI and io-uring's SQ/CQ implementation remain trusted.
