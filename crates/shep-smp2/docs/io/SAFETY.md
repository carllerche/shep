# Owned io_uring operation boundary

The `io` module denies unsafe code. `src/io/driver.rs` and `src/io/fd.rs` allow it;
`buffer`, `producer`, `operation`, `arena`, and `lifecycle` each forbid it. The fs and
net modules and the runtime/eventfd protocol in shep-smp2 forbid unsafe code.
Dependencies (including io-uring and shep-sys) retain their own unsafe internals.

Callers supply owned operations, never raw SQEs, pointers, borrowed buffers,
raw descriptors, or unsafe buffer traits. The driver and Io are local, backed
by one shared Rc. Typed operation owners retain their originating Io and concrete inputs. A submitter cannot inspect or mutate a buffer while the kernel
owns it. Only the private driver constructs SQEs and processes
CQEs. The driver owns shared ring storage and DriverId allocation. The io module
provides the safe producer and Rejected admission errors; operation owns typed owners,
cancellation tokens, completion values, and request inputs. Arena owns slab
records, packed OperationIds, and retirement helpers; lifecycle defines their
submitter/completion states. These implementation modules are crate-private.

## Publication and memory ownership

`Io::submit(T)` takes sealed concrete operation data; `Io::push(Operation)` is
its enum convenience API. Publication temporarily dispatches through Operation,
then infallibly recovers T without allocating or invoking user code. The returned
Op<T> owns inputs and an Io clone. The arena stores only lifecycle/result metadata,
completion kind, cancellation dependencies and identity counts during normal use.

Buffer backing allocations, CString paths and boxed socket addresses remain
stable across owner moves and slab growth. No SQE points into the movable owner.
Rc<Fd> retains input descriptors. Initialized Buffer storage prevents uninitialized
reads after short/error completions. Offsets and lengths retain their validation.

Push normally only publishes. A full SQ triggers at most two nonblocking
submit/reap attempts, retrying publication after each. Transient errors and zero
progress cannot spin indefinitely: an unpublished request returns WouldBlock
with its owned inputs. Fatal submission errors close admission and notify waiters,
but neither partial submissions nor errors roll back already-published requests.
The kernel SQ head determines free space; an enter result is not a list of
requests safe to free or republish. No software pending queue is added.
The ring uses no SQPOLL. Explicit submit/reap/wait phases remain available.
Pressure callbacks and destructors run only after ring and arena borrows end.
Like Monoio's submit_with_data ordering, pressure is handled before creating and
publishing the new operation. After callbacks, publication reacquires borrows and
rechecks admission and SQ space. No callbacks run after successful publication,
so the returned owner exists before any completion callback. Ring, arena and
shutdown-cancellation state share one RefCell. Normal completion effects are
stack-owned, one CQE at a time. There is no activity accounting or generic
completion guard API; higher layers coordinate resource shutdown.

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
low 40 bits. NonZeroU64 makes the optional identity eight bytes; the owner also stores Io and data. Slab
index overflow rejects admission before publication. All handle methods that
access the slab validate the supplied Io before indexing, including release and
cancel_target; a mismatch panics without consuming the original claim. CQE
identities are checked too. Zero is reserved for the driver's single internal
teardown cancellation; it never indexes the slab. The index remains protected by the lifecycle and
cancellation pins; owner bits are not per-slot generations.

The owner holds concrete inputs while Submitted, Waiting and Completed. Dropping
an unfinished owner moves kernel-needed data into Ignored(Some(Box<dyn Any>)).
Nop, Socket, published Close and Cancel require no input retention after publication
and use Ignored(None). Their metadata still tracks terminal completion.

| State | Owner action | Terminal CQE action |
|---|---|---|
| Submitted | Poll installs Waiting; drop transfers needed data to Ignored | Store Completed |
| Waiting | Replace waker, or abandon inputs and extract waker | Extract waker and store Completed |
| Ignored | Owner has released its claim | Extract inputs and owned result for destruction |
| Completed | Consume or destroy result and owner inputs | Already retired |
| VacantPinned | No resources, only identity pins | Original CQE already retired |

Completed abandonment never boxes inputs, including when pins remain. Retirement
extracts ignored inputs/results immediately even with pins; only VacantPinned
remains. A slot is reusable only after both owner interest and every kernel/identity
reference end. Forgetting an Op leaks its inputs and Io clone, but cannot free
kernel resources early. Normal Drop automatically releases interest.

CancelTarget is an owned, same-ring identity reference, created from a live
handle. Acquiring it pins the target slot even before cancellation publication.
Its drop releases an unsubmitted reference. Publication transfers that pin into
the cancellation record, disarming the token; a cancellation CQE releases it
independently of the cancellation owner. The redundant Io clone is dropped after
the publication borrow ends, while the caller still retains Shared. If the target completes and is consumed first, the slot
cannot be reused for another SQE that a delayed cancellation could accidentally
match. No generations or publicly reusable integer tokens are needed. Rejected
cancellations return the token, preserving ownership. A retired token cannot
be resubmitted.

`Config::submission_queue_capacity` bounds the physical SQ (rounded up to a
power of two, valid range 1..=32768). The operation slab grows for outstanding
requests, completed results and cancellation pins; `in_flight` is a count, not
an admission limit. Cancellation uses the same SQ-turnover path and has no
separate outstanding cap. After ordinary admission closes, cancellation remains
allowed until terminal failure/teardown. The ring requires IORING_FEAT_NODROP:
CQ overflow retains completions in the kernel rather than losing identities or
owned resources. Reaping frees CQ space and a later enter flushes overflow.
There are no admission waiters or software submission backlogs.

Driver::tick consumes and retires one CQE under a state borrow, then releases
that borrow before destroying extracted inputs/results and invoking its waker. It repeats for at most the CQ length observed on entry.
The CQ iterator is dropped before callbacks, publishing its consumed head so a
nested tick cannot observe the same CQE twice. No raw CQEs or slab IDs remain
queued in a local snapshot across callbacks. Each iteration rechecks the ring:
a callback can drive more completions, reuse retired slots, or destroy the owner.
The return count excludes CQEs retired by nested calls. Later CQEs need not have
updated their records when the first callback runs.

Effects contain at most one waker, one abandoned input box, and one discarded
owned result. Empty records removed by final unpin contain no user resources.
Waker clones, replacement drops, input/result destruction and wakes occur outside
state borrows. Wakes isolate panics. Teardown defers waiter wakes until all kernel
access has ended and the ring has been destroyed.

## Unsafe boundaries

1. SQ push publishes pointers into already-owned, stable operation data. The typed
   owner, or an abandonment box, retains it until terminal completion.
2. Successful OpenAt/Socket/Accept results adopt fresh descriptors using
   OwnedFd::from_raw_fd and Fd::from_owned_fd_unchecked. Adoption occurs even
   for ignored operations, so the owned
   result is closed outside the state borrow when abandoned, without waiting for identity pins.
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

Fd is !Send and !Sync through a PhantomData<Rc<()>> marker. It stores no
runtime reference. The final owner takes its optional File and looks up the
currently entered runtime's Io through TLS, releasing the context borrow before
publishing a detached Close. This also applies when an operation owner or abandonment box owns
the last Rc<Fd>, regardless of where the descriptor was created or used.
Converting Fd into File, OwnedFd, or RawFd takes the descriptor out and disables
its destructor cleanup.

Outside an entered runtime, during TLS teardown, or while the context is
mutably borrowed, destruction falls back to synchronous File close. The same
fallback applies when the current driver is gone, closed, failed, or borrowed,
publication errors, or bounded SQ progress cannot free a slot. Ordinary SQ
fullness first submits/reaps existing work and retries detached CLOSE publication.
Close stores the validated descriptor as a plain File, so rejecting or dropping
an unpublished Close cannot recursively resubmit through Fd::drop. Publication
uses a fallible state borrow for reentrant final-owner destruction.
Accepted detached closes release their submitter claim immediately and retain
kernel ownership until completion. CLOSE SQEs use IOSQE_ASYNC to keep potentially
blocking release work (including socket linger) off the submitting thread.
Synchronous fallback and driver teardown can still wait for that work.

Callers can explicitly await Fd::close(&io). Before the first poll, dropping
that future follows the descriptor's drop policy. Admission rejection closes
synchronously. After publication, canceling the future abandons its result but
does not cancel the CLOSE.

Close accepts a unique Fd and transfers it with into_raw_fd after successful
SQ publication, leaving its destructor with no descriptor to close. The driver retains no raw fd to
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

Soundness takes priority over successful cleanup. Callers should stop producers
and ensure all application operations have reached terminal completion before
runtime shutdown or driver destruction. Indefinite operations must be explicitly
canceled and their original terminal CQEs reaped. Dropping an owner, submitting
cancellation, or receiving only the cancellation CQE is not sufficient. Completed
results and identity-only cancellation pins may remain alive afterward; the slab
does not need to be empty. This is a caller lifecycle responsibility, not an unsafe
precondition: failing to coordinate shutdown must never permit premature freeing
of kernel-accessed inputs.

Driver::drain closes ordinary admission and explicitly submits/reaps accepted
operations. Higher layers must cancel non-completing receives/watches first;
drain intentionally waits for terminal CQEs. It does not imply fsync, resource
close, or application flush semantics. Runtime parking cancels its own eventfd
watch through ordinary Cancel before draining.

Driver drop provides best-effort cancellation followed by terminal retirement as
a memory-safety fallback, not a guarantee of successful application work. It
fails all admission, including public cancellations, and submits
queued SQEs before issuing targeted cancellations. It scans slab storage once
without allocating an identity vector, skipping completed, cancellation, and Close
records. Internal cancellations use their own SQEs after queued work is
submitted, independently of the number of outstanding public requests. Each cancellation CQE is reaped before issuing the next; ENOENT
and EALREADY are allowed because original completion can race cancellation.
After issuing cancellations, drop blocks until every original terminal CQE has
been reaped, including accepted public cancellation requests. Cancel completion
alone never proves that a target's inputs can be freed.

Only after retirement does drop close the ring and deliver accumulated waiter
notifications outside borrows. Completed results remain consumable through
retained Io clones, and unsubmitted CancelTargets can release their remaining
identity pins afterward. Ordinary Drop does not intentionally leak unresolved payloads; explicitly forgetting
an owner leaks its data and Io reference.
Destruction may block indefinitely if an operation cannot finish or be canceled.
A fatal enter/cancel error or panic before retirement aborts the process; it
must not unwind and free kernel-owned inputs. Transient enter errors are retried.
Cancellation failure means an unexpected cancellation error; ENOENT and EALREADY
still require observing the original terminal completion. An operation's own
error completion establishes retirement and is not itself a teardown failure.
Neither closing the ring nor relying on asynchronous kernel ring cleanup is a
substitute for terminal retirement before freeing operation inputs.
Native tests inject zero progress, transient/fatal enter failures and partial
submissions using real SQ head movement; accepted requests remain drainable.
Native subprocess tests verify that fatal teardown errors and pre-retirement
panics terminate with SIGABRT. Run them with
`cargo nextest run -p shep-smp2 --run-ignored all`; the subprocess test is ignored
by default because Weft does not model child processes.

The runtime's RUNNING/PARKING/NOTIFIED/CLOSED atomic handshake, owner-thread
notification suppression, and scoped executor TLS live above this boundary.
Safe shep-sys eventfd creation needs no unsafe block here. Weak wake handles retain a standard File during racing writes. The reactor
creates a distinct local Fd duplicate for its poll watch, which retains that
descriptor through its CQE; cancellation pins that watch's slab identity.

## Verification limits

Native tests cover split push/submit/reap phases, repeated SQ turnover, bounded rejection,
owned buffers and descriptors, typed fd outputs, positional file operations,
socket operations, outstanding reads beyond SQ capacity, cancellation under SQ
pressure, stale cancellation identity, lingering-socket drop under SQ pressure,
reentrant and panicking wakers, result retention, and parking races.
Pure lifecycle and buffer tests run under Miri; Miri cannot validate kernel
accesses. Kernel ABI and io-uring's SQ/CQ implementation remain trusted.
