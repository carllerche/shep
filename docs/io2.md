# Native io_uring runtime foundation

The parallel *2 crates keep existing Raft/WAL/storage/node consumers on their
current runtime. Linux io_uring with extended enter arguments is required
(Linux 5.11+ for waiting; individual opcodes can require newer kernels).
Unsupported initialization and opcode errors are returned without a fallback.

| Component | Responsibility |
|---|---|
| shep-smp-io2 | Safe owned operations, ring push/submit/reap, completion lifecycle and cancellation |
| shep-smp-fs2 | Positional owned-buffer file adapter and operation-completion futures |
| shep-smp-net | TCP listener/stream adapter, bounded transfers, serialized directions and socket shutdown |
| shep-smp2 | Local executor/timer integration, batching policy and eventfd parking |

The io2 crate root denies unsafe code; only its private driver module opts in.
Io, arena, lifecycle, operations and buffers each forbid unsafe code. Both higher layers
forbid unsafe code too. The four unsafe blocks publish operation/cancellation SQEs and adopt owned
file descriptors. See [the safety boundary](../crates/shep-smp-io2/SAFETY.md).
Production dependencies have no Tokio runtime. Existing task internals retain
their separate unsafe implementation and upstream attribution.

## Driver phases and operations

Io::push(Operation) immediately admits an owned request and publishes its SQE,
returning an OperationHandle. It performs no submit syscall, wait or reap.
Full admission/SQ capacity returns Rejected { error: WouldBlock, operation };
validation and closed admission also return the original inputs. There is no
admission-waiter map or second queue behind a full queue.

Driver::submit explicitly submits the current batch. Driver::reap_into processes
available CQEs, returns the number processed, and appends waiter notifications
to a caller-owned Progress. The caller calls Progress::wake after the batch's
lifecycle transitions. Progress retains its allocation across wake calls, and
the driver reuses storage for discarded records. Driver::submit_and_wait
is an explicit combined submission/wait primitive for an idle runtime.

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
the slab, its records, admission counters, and packed OperationId keys.
Lifecycle defines the per-operation Submitted/Waiting/Ignored/Completed states;
the arena combines their transitions with kernel-reference counts to decide
slot retirement.

OperationHandle is an eight-byte opaque ID, with no Io reference and no Future
implementation. Use handle.try_complete(&io), handle.poll_complete(&io, cx), or
poll_fn(|cx| handle.poll_complete(&io, cx)).await. Pass the allocating Io to
handle.release(&io) when abandoning a result. File adapters use borrowed guards
to release claims on dropped futures; raw-token callers own that responsibility.
A bare token drop retains its slab claim until Io destruction. The registered
waker belongs to the operation's slab record; no per-request Rc clone or
Rc/RefCell mailbox is needed for ordinary operations.

A static atomic allocates nonzero 24-bit DriverIds once per driver, never
recycling or wrapping them. Io clones share their driver's identity. The CQE's u64 packs that owner ID above a 40-bit slab index
(24 + 40 = 64). Passing a token to the wrong Io panics before accessing the
slab. Cancellation tokens retain their allocating Io while their pin is active;
retirement releases that reference.

The shared Submitted/Waiting/Ignored/Completed lifecycle follows Monoio's model:
slots remain occupied until both the submitter and kernel release their claims.
Releasing a handle marks its request Ignored without freeing kernel-owned data.
Completed results do not consume kernel admission capacity and remain available
after driver destruction when the caller retains its Io. Retained completed results may grow the slab.

A CancelTarget pins a live identity until its token is dropped unsubmitted or
its cancellation CQE arrives. The original operation may complete and its
values may be consumed first; its slot still cannot be reused for another
request. One cancellation can be admitted beyond the ordinary in-flight limit.
Cancellation is explicit and does not promise that the target had no effect.

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

File::from_std binds an already-open std::fs::File to Io. The fs2 adapter
provides eager submit_read_at/submit_write_at handles and async read_at/write_at
helpers returning (Result<usize>, Buffer). Async helpers are lazy until polled;
saturation then returns immediately. Short I/O and partial effects are exposed.
There are no shared file cursors. sync_data/sync_all await their own completion;
callers must establish prerequisite write completion before syncing. The lower
layer's Close takes a uniquely owned descriptor; a high-level resource close
protocol must first account for its outstanding operations.

The driver owns no write backlog, resource-readiness waiter collection,
durability frontier or background error policy. File writers can retain eager
handles to track concurrent completion, contiguous progress and durability above
it. The [shep-smp-net adapter](../crates/shep-smp-net/README.md) provides TCP
serialization through exclusive read/write halves, bounded transfers, flush,
half-close, and cleanup on dropped futures. It has no background write queue:
every write awaits completion. Dropping an admitted socket future shuts down
the resource and requests cancellation; cancellation is terminal for that
listener or connection. Socket shutdown also retires outstanding work when
the cancellation reserve is full. A resource registry can likewise be added
without moving operation waiters out of the operation slab.

Current Send/Recv are ordinary single-shot owned-buffer operations. Provided
buffer pools, buffer selection, multishot and zero-copy notifications remain
extensions requiring separate safe ownership protocols. They are required
building blocks for the eventual buffer-pool TCP API, not implemented by this
refactor.

Runtime::block_on runs a local root task and services I/O/timers around bounded
executor turns. Background tasks survive root completion until finished or
shutdown. Its parking adapter keeps one eventfd PollAdd armed, reserves that
ordinary operation in addition to Config::max_in_flight, and rearms before
calling completion wakers. The driver's cancellation reserve permits shutdown
to retire that watch even at full data capacity. Io::in_flight includes this
runtime-owned watch. Runtime shutdown cancels task consumers and drains accepted
kernel work; users of raw indefinite operations must cancel those operations
before shutdown. No resource-level forced close policy is inferred.

```sh
cargo run -p shep-smp2 --example files
```

## Idle/wakeup protocol

Parking::prepare_park drains stale eventfd tokens and publishes PARKING before
the runtime's final ready-work/deadline check. The Park guard restores RUNNING
on wait, error, timeout, unwind or an abandoned attempt. NOTIFIED retains a
permit that skips waiting; CLOSED is terminal. SeqCst atomic RMWs linearize
publication and state transitions, including already-notified producers.

Foreign scheduling publishes the task ID before notification. RUNNING wakes
retain a permit without writing eventfd. Only the first foreign transition
PARKING -> NOTIFIED writes; later wakes coalesce. The final inbox check and
retained readable kernel token cover both sides of entering the wait. A delayed
write from an earlier parking attempt is harmless and drained on a later turn.

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
cargo nextest run -p shep-smp-io2 -p shep-smp-fs2 -p shep-smp-net -p shep-smp2
cargo clippy -p shep-smp-io2 -p shep-smp-fs2 -p shep-smp-net -p shep-smp2 --all-targets -- -D warnings
MIRIFLAGS='-Zmiri-disable-isolation -Zmiri-tree-borrows' cargo +nightly-2026-10-22 miri test -p shep-smp-io2 --lib
```

Pure lifecycle and buffer tests run under Miri; actual io_uring/eventfd tests
require a native kernel. Compile-fail docs enforce local-only driver/runtime
ownership. The [13-case runtime benchmarks](../crates/shep-smp2/benches/README.md)
cover scheduling, timers, foreign handoff, file batches and parking.
[Eight driver-only cases](../crates/shep-smp-io2/benches/README.md) isolate
batching, owned-buffer reuse, ignored completions and identity pins. Historical
archives retain their original source paths and hashes, including the former
io2-core crate and former core module. They are measurement records.

Remaining steps before production migration include buffer-pool networking,
filesystem resource close/flush APIs, filesystem metadata policy, thread-per-core
startup/shutdown and submit_to, RPC integration, and shep-smp-io-scheduler2.
