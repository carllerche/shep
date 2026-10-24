# Native io_uring runtime foundation

This parallel implementation leaves shep-smp, shep-smp-io, the existing I/O
scheduler, and all Raft/WAL/storage/node consumers on their current runtime.
It requires Linux io_uring with extended enter arguments (Linux 5.11+), and
returns initialization errors when the environment does not support it.

| Component | Responsibility |
|---|---|
| shep-smp-io2/src/core.rs + core/ | Private ring/SQ/CQ ownership, kernel-visible resources, operation arena and eventfd parking |
| shep-smp-io2/src/io.rs | Safe owned-buffer file futures, bounded admission and completion delivery |
| shep-smp2 | Single-core Runtime combining the existing executor/timer with the native reactor |

The I/O implementation is one crate. Its root denies unsafe code by default;
only the private core module opts in. The io, arena, buffer and park_state
modules each enforce forbid(unsafe_code), as does the runtime crate. All four
unsafe blocks remain in core.rs and core/park.rs; see
[the safety review](../crates/shep-smp-io2/SAFETY.md). Arena internals use
parent-module visibility, so the safe I/O module cannot retire a submitted
operation or access its kernel-owned buffer through implementation helpers.
Production dependencies contain no Tokio runtime. Tests and benchmarks use
Tokio channels only as runtime-independent notification primitives. The
existing task crate retains its separately reviewed unsafe implementation.

## Current functionality

Runtime::block_on runs a local 'static root future. Runtime::spawn and the
scoped spawn function schedule local child/background tasks. Futures and outputs
need not be Send. Each turn services I/O and timers around a bounded executor
turn. Ready work prevents parking; idle work waits in io_uring until a CQE,
foreign notification or next timer-wheel deadline. A root completing does not
cancel background tasks: they remain owned until completion or shutdown.

Io::file binds an already-open std::fs::File. File provides positional owned-
buffer read_at/write_at and explicit sync_data/sync_all. Reads/writes may be
short. Await prerequisite writes before syncing; independently submitted
operations have no implicit ordering. Offset u64::MAX is rejected rather than
silently selecting io_uring's current-file-position sentinel. Aligned buffers
support descriptors opened with O_DIRECT, but alignment discovery/open policy
is not implemented by this layer yet.

Operations are lazy until first polled and wait for bounded admission. Once
accepted, the slab owns the operation, buffer, descriptor and lifecycle. The
safe core immediately publishes its SQE, carrying just the slab index; the
reactor batches submission syscalls. There is no software submission queue,
per-operation Rc/RefCell mailbox, or reply lookup table.

The lifecycle follows Monoio's Submitted/Waiting/Ignored/Completed model. A
terminal CQE releases driver interest, and result consumption or future drop
releases consumer interest. Whichever finishes last removes the entry. Dropped
in-flight futures become Ignored and keep their buffers/descriptors until the
real CQE. Completed results stay available until consumed, including after the
reactor is destroyed, and do not hold kernel admission capacity.

Completion delivery extracts owned wakers before invoking callbacks; it keeps
no notification indices. A callback can consume another completed result and
immediately reuse its slot safely. There are no generations, tombstones,
Consumed placeholders or deferred reclamation lists. Ring/arena borrows never
survive waker callbacks or operation destruction.

Fatal driver failures are terminal and may retain unresolved resources; see
SAFETY.md. Normal shutdown cancels task consumers, drains accepted I/O, retires
the wake watch and closes reactor resources.

A runnable example opens its descriptor before entering the reactor, writes,
syncs, waits on a timer, and reads on the new runtime:

```sh
cargo run -p shep-smp2 --example files
```

There is no background Tokio runtime, polling helper thread, periodic I/O retry
timer or epoll completion bridge. Each reactor is explicitly local; its weak
waker alone crosses threads.

## Idle protocol

The core exposes prepare_park, which arms the eventfd PollAdd, drains old
counter values and publishes PARKING before submitting. The runtime rechecks
ready work and deadlines, then either drops the guard or calls wait(deadline).
Every guard exit restores RUNNING. A retained NOTIFIED permit makes the attempt
skip waiting; CLOSED is terminal. This four-state protocol uses SeqCst CASes.

Foreign scheduling publishes the task ID before notifying. RUNNING -> NOTIFIED
retains the notification without a syscall; only the first foreign transition
from PARKING -> NOTIFIED writes the eventfd. Later notifications coalesce. The
post-prepare inbox check and the retained kernel token cover both sides of the
race with entering the wait. Native tests inspect the actual counter, while
threaded regression tests force notifications around preparation and entering
wait, timeout, abandoned waits and shutdown.

The runtime enters the executor's scoped TLS context for its entire loop, not
just task polling. I/O/timer callbacks with a matching executor identity push
directly into its local ready queue. An owner-thread wake with a mismatched or
absent context uses the inbox without calling the driver waker. WakeHandle also
checks the owner thread, so direct owner unpark calls never write the eventfd.
Task-poll recursion is guarded separately from entering a context, allowing
ordinary driver work inside the scope while preserving nested-poll rejection.
No thread-local executor registry or unsafe borrowed TLS references are used.

WakeHandle::unpark returns false after closure; already-started writers retain
the descriptor through their writes. A late write claimed during an earlier
parking attempt may arrive after the owner resumes and is harmless. Park reports
Ready, TimedOut or Interrupted. Absolute deadlines apply to one wait attempt.

This eventfd wakes the native reactor for external work. It is not the existing
shep-smp-io eventfd that forwards kernel completions into Tokio's AsyncFd.

## Design sources

This is a new implementation using the workspace io-uring crate, not copied
Seastar/Monoio implementation code. It uses:

- [Monoio's driver/waker](https://github.com/bytedance/monoio/tree/8675e9406ea7943c441d969e38c3f5fc198ef6d1/monoio/src/driver/uring):
  reactor-owned waiting, retained eventfd wakeups, weak unpark ownership and
  extended wait arguments. Shep uses PollAdd on eventfd instead of a kernel read
  into a reactor-owned buffer, and avoids Rc<UnsafeCell<driver>>.
- [Seastar's reactor](https://github.com/scylladb/seastar/blob/master/src/core/reactor.cc):
  local execution, servicing event sources between task turns, and preparing
  interrupt mode before rechecking for pending work. Shep does not copy the
  systemwide-barrier optimization or its task/I/O schedulers. Its own
  CAS-based parking handshake suppresses eventfd writes while running.

No attribution file claims this Rust implementation is a fork. The existing
executor/task/timer crates retain their own upstream attribution.

## Validation and next steps

Run the two new crates' native tests and Clippy. Buffer and operation arena tests can run under Miri; actual io_uring/eventfd tests require
a native kernel. Compile-fail docs enforce local-only Reactor/Runtime ownership.
The [benchmark suite](../crates/shep-smp2/benches/README.md) establishes
baselines for root tasks, yielding, timers, foreign handoff, file I/O
batches and retained wakeups. It is not an equivalence/performance claim against
Tokio, Monoio or Seastar.

The foundation is deliberately single-core. Still to implement before migrating
production: a bounded blocking metadata helper pool and async open/metadata
policy, thread-per-core startup/shutdown and submit_to in shep-smp2, networking
and RPC integration, and shep-smp-io-scheduler2. Registered buffers/files,
multishot operations and busy-poll tuning require their own
measurements and safety/lifetime review. The old crates stay in place until an
explicit application migration.

The former shep-smp-io2-core package was merged into io2's private core module.
Historical benchmark JSON deliberately retains the original source paths and
hashes for the versions measured. Current validation commands are:

```sh
cargo test -p shep-smp-io2 -p shep-smp2
cargo clippy -p shep-smp-io2 -p shep-smp2 --all-targets -- -D warnings
MIRIFLAGS='-Zmiri-disable-isolation -Zmiri-tree-borrows' cargo +nightly-2026-10-22 miri test -p shep-smp-io2 --lib
```

Native core tests live under src/core/tests; public buffer/file tests remain
integration tests. Miri runs the pure arena/parking-state tests; native kernel
and callback tests are excluded from Miri.
