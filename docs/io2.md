# Native io_uring runtime foundation

The parallel *2 crates keep existing Raft/WAL/storage/node consumers on their
current runtime. Linux io_uring with extended enter arguments is required
(Linux 5.11+ for waiting; individual opcodes can require newer kernels).
Unsupported initialization and opcode errors are returned without a fallback.

| Component | Responsibility |
|---|---|
| shep_smp2::io | Safe owned operations, ring push/submit/reap, completion lifecycle and cancellation |
| shep_smp2::fs | Positional owned-buffer file adapter and operation-completion futures |
| shep_smp2::net | TCP listener/stream adapter, bounded transfers, serialized directions and socket shutdown |
| shep_smp2 runtime | Local executor/timer integration, batching policy and eventfd parking |

The io module denies unsafe code; only its private driver and fd modules opt in.
Io, arena, lifecycle, operations and buffers each forbid unsafe code. Both higher layers
forbid unsafe code too. The three unsafe blocks publish operation/cancellation SQEs and adopt owned
file descriptors. See [the safety boundary](../crates/shep-smp2/docs/io/SAFETY.md).
Production dependencies have no Tokio runtime. Existing task internals retain
their separate unsafe implementation and upstream attribution.

## Driver phases and operations

Io::push(Operation) owns a request and publishes its SQE. On a full SQ it makes
at most two nonblocking submit/reap attempts before retrying publication. Stalled
progress returns Rejected { error: WouldBlock, operation }; validation, closure,
and submission errors also preserve unpublished inputs. Already-published requests
remain owned across partial submissions and enter errors. No software pending
queue is added. Completion callbacks run outside driver borrows.

Config::submission_queue_capacity bounds the physical SQ, rounded up to a power
of two (1..=32768). Tracking grows independently for outstanding operations and
retained results. The ring requires lossless CQ overflow (IORING_FEAT_NODROP).

Driver::submit explicitly submits the current batch. Driver::tick retires one
available CQE at a time, releases the shared state borrow, and destroys retired
resources and notifies its waiter before processing the next CQE. There is no
caller-owned notification batch. A tick processes at most the CQ length observed
on entry; reentrant calls can consume remaining CQEs safely. Ring, arena, and
shutdown bookkeeping share one state borrow. Teardown alone defers notifications
until all kernel access has ended. Driver::submit_and_wait is an explicit combined
submission/wait primitive for an idle runtime.

Operation explicitly enumerates supported requests: Nop, ReadAt, WriteAt,
Fsync, Fallocate, Ftruncate, OpenAt, Close, Socket, Accept, Connect, Send, Recv,
Shutdown, PollAdd and Cancel. Every request owns the inputs whose addresses or
file descriptors the kernel needs. Successful completions return Count, File,
Poll or Done; errors preserve the original owned operation and its buffer.
New opcodes require implementing ownership, completion and cancellation rules;
arbitrary raw SQEs and SQE flags are not exposed.

The private driver module owns the ring, shared storage, and DriverId allocator.
The io module exposes the safe producer and admission errors. Operation owns
requests, handles, cancellation tokens, and typed completion values. Arena owns
the slab, its records, outstanding counts, and packed OperationId keys.
Lifecycle defines Submitted/Waiting/Ignored/Completed and empty pinned states;
the arena combines their transitions with kernel-reference counts to decide
slot retirement.

`Io::submit(T)` accepts sealed concrete operation data and returns `Op<T>`, an
owning future retaining the originating Io and inputs. Files use concrete ReadAt,
WriteAt, Fsync, Fallocate and Ftruncate data. `Io::push(Operation)` remains the
enum convenience API and returns `Op<Operation>`. The
try_complete/poll_complete/release methods accept the originating Io and reject
foreign drivers. Drop automatically abandons the
result without canceling the operation.

Normal inputs live in the owner, outside the arena. The arena holds lifecycle,
owned result, waker, completion kind and cancellation identity accounting. Dropping
an unfinished data-bearing owner moves concrete inputs into one abandonment box;
completed owners never need that box. Nop, published Close, Socket and Cancel
need no retained input box. Cancel pins transfer into the cancellation record at
publication and retire at its CQE, independently of the cancellation owner.

A static atomic allocates nonzero 24-bit DriverIds once per driver, never
recycling or wrapping them. Io clones share their driver's identity. The CQE's u64 packs that owner ID above a 40-bit slab index
(24 + 40 = 64). Passing a token to the wrong Io panics before accessing the
slab. Unsubmitted cancellation tokens retain their allocating Io. Publication
transfers their pin to the arena and releases that redundant Io reference; the
cancellation CQE releases the pin.

The shared Submitted/Waiting/Ignored/Completed lifecycle follows Monoio's model:
slots remain occupied until both the submitter and kernel release their claims.
Dropping or releasing an owner marks its request Ignored without freeing kernel-owned data.
Completed results remain available
after driver destruction through their owners. Retained completed results may grow the slab.
Completed resource destruction does not wait for identity pins; only an empty
tombstone remains until the last pin retires. Forgetting an Op leaks its inputs
and Io reference; dropping it performs abandonment.

A CancelTarget pins a live identity until its token is dropped unsubmitted or
its cancellation CQE arrives. The original operation may complete and its
values may be consumed first; its slot still cannot be reused for another
request. Cancellation uses the same SQ turnover path without an outstanding cap.
Cancellation is explicit and does not promise that the target had no effect.
Accepted Close operations run to completion: canceling an unconsumed Close
returns Unsupported, and teardown waits without canceling it. The kernel owns
its descriptor after publication. Even ECANCELED can be a flush error after
removal, so no close error causes the driver to reclaim or retry that fd.

Callers should stop producers and ensure all application operations have reached
terminal completion before runtime shutdown or driver destruction. Dropping a
future only abandons its result. This lifecycle responsibility is enforced safely:
unresolved work must never cause inputs to be freed while the kernel can access them.

Driver::drain stops ordinary admission and waits for accepted operations;
non-completing receives/watches must first be explicitly canceled. Driver drop
instead closes all admission, submits queued work, cancels outstanding requests,
and blocks until their terminal CQEs have been reaped. It then closes the ring
and wakes waiters; completed results remain available through retained Io clones.
Destruction does not intentionally leak unresolved inputs and may wait indefinitely
for operations that cannot finish or be canceled. Fatal teardown errors abort the
process rather than freeing kernel-owned inputs. Fatal driver errors during normal
operation wake waiters; unresolved results panic if polled before retirement.

## Higher layers

File::open and async File::from_std use the current thread-local runtime.
Namespace functions live in fs. Positional read_at/write_at return
(Result<usize>, Buffer); futures are lazy until polled. Short I/O and partial
effects are exposed, and there are no shared file cursors. sync_data/sync_all
await their own completion; callers establish prerequisite write completion.
There is no File::shutdown, activity counter, or generic completion guard API.
Higher layers coordinate admission, outstanding requests, and resource shutdown.
The lower layer's Close takes a uniquely owned descriptor. Detached CLOSEs use
the same SQ progress path before fallback.

The driver owns no write backlog, resource-readiness waiter collection,
durability frontier or background error policy. Higher layers track concurrent completion and durability. The [net adapter](../crates/shep-smp2/docs/net/README.md) provides TCP
serialization through exclusive read/write halves, bounded transfers, flush,
half-close, and cleanup on dropped futures. It has no background write queue:
every write awaits completion. Dropping an admitted socket future shuts down
the resource and requests cancellation; cancellation is terminal for that
listener or connection. Socket shutdown also retires outstanding work when
cancellation publication cannot make progress. A resource registry can likewise be added
without moving operation waiters out of the operation slab.

Current Send/Recv are ordinary single-shot owned-buffer operations. Provided
buffer pools, buffer selection, multishot and zero-copy notifications remain
extensions requiring separate safe ownership protocols. They are required
building blocks for the eventual buffer-pool TCP API, not implemented by this
refactor.

Runtime::block_on runs a local root task and services I/O/timers around bounded
executor turns. Reactor tick coordinates submission, completion delivery, watch
rearming and timer advancement; park_if_idle owns the readiness checks and
parking handshake. Background tasks survive root completion until finished or
shutdown. Its parking adapter keeps one eventfd PollAdd armed and checks it
before parking, including when publication has reaped its CQE. The watch shares
the configured SQ; no extra outstanding-operation reservation is needed.
Io::in_flight includes this runtime-owned watch. Runtime shutdown cancels task consumers and drains accepted
kernel work; users of raw indefinite operations must cancel those operations
before shutdown. No resource-level forced close policy is inferred.

## Idle/wakeup protocol

The runtime driver's prepare_park drains stale eventfd tokens and publishes PARKING before
the runtime's final ready-work/deadline check. The Park guard restores RUNNING
on wait, error, timeout, unwind or an abandoned attempt. NOTIFIED retains a
permit that skips waiting; CLOSED is terminal. SeqCst atomic RMWs linearize
publication and state transitions, including already-notified producers.

Foreign scheduling publishes the task ID before notification. RUNNING wakes
retain a permit without writing eventfd. Only the first foreign transition
PARKING -> NOTIFIED writes; later wakes coalesce. The final inbox check and
retained readable kernel token cover both sides of entering the wait. A delayed
write from an earlier parking attempt is harmless and drained on a later turn.

The runtime context holds I/O and timer handles directly, without a separate
driver-handle wrapper. TLS supplies capabilities for new operations; existing
operations retain their originating driver through their resource adapters.
The runtime enters executor scoped TLS around its entire driving loop. Matching
owner wakes enqueue directly into the local ready queue. A mismatch uses the
target inbox without invoking its driver waker. WakeHandle independently checks
the owner thread and suppresses owner eventfd writes, including after prepare.
No unsafe borrowed TLS lifetime mechanism or global executor registry is needed.

WakeHandle is weak; upgraded writers retain the event descriptor throughout a
racing write. Late wakes return false after closure. Native tests inspect the
actual eventfd counter and force races before/after the final check, during
wait, timeout, guard abandonment and destruction. This protocol belongs to
smp2, built from io2's ordinary PollAdd/Cancel operations.

## Design sources and validation

This is a Shep implementation using the workspace io-uring crate. It draws on
[Monoio's lifecycle and driver](https://github.com/bytedance/monoio/tree/8675e9406ea7943c441d969e38c3f5fc198ef6d1/monoio/src/driver/uring)
and [Seastar's reactor](https://github.com/scylladb/seastar/blob/master/src/core/reactor.cc):
local execution, explicit submission batches, retained wakeups and preparing
sleep before rechecking pending work. It does not copy their unsafe driver
cell or systemwide memory-barrier mechanisms.

```sh
cargo nextest run -p shep-smp2 --run-ignored all
cargo nextest run -p shep-smp2 --features allocation-tests --test io_allocations
cargo weft test -p shep-smp2
cargo weft test -p shep-smp2 --test rt_park --weft-runs 1000
cargo test -p shep-smp2 --doc
cargo clippy -p shep-smp2 --all-targets -- -D warnings
MIRIFLAGS='-Zmiri-disable-isolation -Zmiri-tree-borrows' cargo +nightly-2026-10-22 miri test -p shep-smp2 --lib io::
```

The ignored teardown-abort test requires native subprocesses and is included by
`--run-ignored all` above. It verifies SIGABRT on fatal teardown errors and panics
before retirement. Weft skips this process-level check; cancellation and terminal
retirement remain covered in simulation.

Pure lifecycle and buffer tests run under Miri; actual io_uring/eventfd tests
run against a native kernel or weft’s Linux simulation. Compile-fail docs enforce local-only driver/runtime
ownership. The [13-case runtime benchmarks](../crates/shep-smp2/benches/README.md)
cover scheduling, timers, foreign handoff, file batches and parking.
[Thirteen driver-only cases](../crates/shep-smp2/benches/io/README.md) isolate
batching, owned-buffer reuse, ignored completions, identity pins and real
cancellation at several abandonment rates. Historical
archives retain their original source paths and hashes, including the former
io2-core crate and former core module. They are measurement records.

Correctness tests synchronize on observable events: first pending polls,
completion retirement, worker-job fences, and producer/consumer acknowledgements.
They do not impose elapsed-time upper bounds on I/O or use sleeps to assume that
another task has started. Native nextest process timeouts and weft execution-step
bounds detect hangs independently of the application's clock.

Weft's default clock jumps stay enabled. Explicit timer tests check deadline
semantics; wake/deadline races allow either event to win and verify that work is
still observed after parking is prepared and readiness is rechecked. The lingering
socket pressure test checks detached-close publication and descriptor retirement,
not a wall-clock latency threshold. Submission latency is a performance property;
these ownership assertions alone do not measure whether kernel submission blocks.

Remaining steps before production migration include buffer-pool networking,
filesystem resource close/flush APIs, filesystem metadata policy, thread-per-core
startup/shutdown and submit_to, RPC integration, and shep-smp-io-scheduler2.

## Operation ownership changes

The Monoio ownership proposal is implemented with these decisions:

- Keep concrete inputs in typed owners during normal execution; box only inputs
  needed after unfinished abandonment. Adopt returned descriptors at retirement.
- Remove file activity accounting, File::shutdown and generic completion guards.
  Application shutdown coordination belongs above smp.
- Keep safe RefCell borrowing, per-CQE effects outside borrows, cancellation pins,
  bounded publication pressure, and owned inputs on rejection.
- Require callers to coordinate completion before shutdown. Keep best-effort
  cancellation and terminal retirement in driver destruction as the memory-safety
  fallback; abort the process if teardown fails to establish safe retirement.
  Unresolved work alone does not trigger immediate abort. Runtime shutdown remains
  an explicit higher-level coordinator; it cancels its watch before drain. Closing
  the ring alone is not proof that kernel access to inputs has ended.
- Use sealed conversions through the existing Operation dispatcher at publication;
  no custom raw-SQE or unsafe buffer interface is added.

On x86_64 with rustc 1.97.1, Record and its slab slot shrink from 128 to 48 bytes.
Op<ReadAt> is 72 bytes (56 bytes of data plus owner bookkeeping), while the enum
owner is 88 bytes. Thus minimal read bookkeeping is 120 bytes, versus the old
128-byte record + 16-byte Pending + 8-byte guard allocation = 152 bytes. These
figures exclude buffer storage, shared resources, allocator overhead and enclosing
future frames; the owner data has moved, not disappeared. See the driver benchmark
README for complete future sizes and validation.

Native allocation tests reserve arena capacity and construct buffers/resources
before measuring. Normal typed read/write submission, retirement and consumption
allocate zero times. Unfinished read abandonment allocates once; completed pinned
abandonment allocates zero times. Explicit cancellation and abandonment of the
cancellation owner allocate zero times. Arena growth remains amortized allocation.
