# Owned io_uring operation boundary

The crate root denies unsafe code. `src/driver.rs` and `src/fd.rs` allow it; `buffer`,
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
stable across operation moves and slab growth. Rc<Fd> retains input
file descriptors. Buffer storage is initialized, so short reads and errors
never expose uninitialized memory. Lengths fit the kernel's signed result size;
positional offsets reject the current-position sentinel and signed overflow.

Push performs no submit, reap, wait, or callback. If validation, admission or
SQ push fails, the unsubmitted operation is returned with its owned inputs.
After successful publication, an enter error does not justify freeing inputs.
The ring uses no SQPOLL. Submit and reap are separate explicit phases.

`Io::push_with_guard` additionally retains adapter-owned resources independently
of the submitter claim. Rejected publication drops the guard immediately. The
terminal CQE moves an accepted guard out of the slab into `Progress`; `wake`
releases it outside all driver borrows, before notifying operation waiters.
Dropping that Progress also releases its guards. Cancellation identity pins and
unconsumed results do not retain these guards after terminal completion. During
driver destruction guard release is deferred with the other notifications until
kernel retirement and ring destruction. Guards must not panic in Drop; wake
isolates a panicking guard so subsequent notifications can still run. The fs2
adapter uses guards to keep file/domain activity counted after waiter abandonment.

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

## Unsafe boundaries

1. SQ push publishes pointers into already-owned, stable operation data. Slab
   ownership and lifecycle references retain it until terminal completion.
2. Successful OpenAt/Socket/Accept results adopt fresh descriptors using
   OwnedFd::from_raw_fd and Fd::from_owned_fd_unchecked. Adoption occurs even
   for ignored operations, so the owned
   result is closed when its last lifecycle reference is released.
3. Teardown publishes pointer-free cancellation SQEs directly. Admission has
   failed, so no target identity can be reused. Previously queued SQEs are
   submitted first, and only one internal cancellation is outstanding at a time.
   Its reserved zero user_data is handled without a slab lookup. The original
   requests retain their records and inputs until their own terminal CQEs.

4. Fd imports call fstat with writable struct storage, reading it only after
   success. The public unchecked constructor transfers the non-ring invariant
   to its caller; no validation occurs on that path.

Fd owns a live descriptor that does not refer to an io_uring instance. Generic
imports (including std File and socket wrappers) validate this once with fstat,
then inspect /proc/self/fd for anonymous inodes. Ring descriptors are rejected
and dropped; eventfd and epoll remain supported. If inspection fails, import
fails closed. The unchecked constructor requires the caller to guarantee this
invariant. No mutable File/OwnedFd reference is exposed that could replace the
descriptor. Duplicating an Fd preserves its classification without revalidation.
Successful open/socket/accept CQEs return fresh non-ring descriptors, so the
unsafe adoption boundary constructs Fd directly without a validation syscall.
All descriptor-bearing operations take Fd (shared through Rc where needed).

Fd is !Send and !Sync through a PhantomData<Rc<()>> marker. It has no driver
reference or deferred cleanup queue. Its File field closes synchronously on drop,
including abandoned outputs and final input references retired by the driver.
This is a fallback; callers can explicitly await Fd::close(&io) to use io_uring.
That future publishes an ordinary Close operation and releases its handle if
canceled. Before publication, dropping the future or rejecting admission drops
the still-owned Fd synchronously. After publication, only the kernel closes it;
canceling the future abandons its result but does not cancel the CLOSE.

Close accepts a unique Fd and transfers it with into_raw_fd after successful
SQ publication, suppressing its destructor. The driver retains no raw fd to
reclaim: even ECANCELED may be a filesystem flush error after the descriptor
has been removed and reused. Public cancellation of an unconsumed Close is
rejected with Unsupported, and teardown skips Close when choosing cancellation
targets, waiting for its natural completion instead. No linked SQEs or other
cancellation mechanisms are exposed. Close errors are reported without retrying.

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
without allocating an identity vector, skipping completed, cancellation, and Close
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
Safe rustix eventfd creation needs no unsafe block here. Weak wake handles retain a standard File during racing writes. The reactor
creates a distinct local Fd duplicate for its poll watch, which retains that
descriptor through its CQE; cancellation pins that watch's slab identity.

## Verification limits

Native tests cover split push/submit/reap phases, batching, immediate rejection,
owned buffers and descriptors, typed fd outputs, positional file operations,
socket operations, cancellation at capacity, stale cancellation identity,
reentrant and panicking wakers, result retention, and parking races.
Pure lifecycle and buffer tests run under Miri; Miri cannot validate kernel
accesses. Kernel ABI and io-uring's SQ/CQ implementation remain trusted.
