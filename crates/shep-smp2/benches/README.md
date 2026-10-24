# Native runtime foundation benchmarks

```sh
cargo bench -p shep-smp2 --bench runtime -- --save-baseline native-foundation
```

The initial 11 cases establish the first native-runtime baseline, not a Tokio/Monoio
performance comparison. Defaults: 40 samples, 200 ms warmup, 1 s measurement.
Keep compiler, CPU and filesystem identical for comparisons. Increase warmup
and measurement times on a dedicated machine before interpreting small changes.

- Ready root: allocate a task, run it, consume its result; runtime reused.
- Self-wake: 128 yields plus completion; includes task registry and scheduling.
- Timer idle: a 1 ms sleep with actual kernel parking; includes millisecond
  timer rounding, so the elapsed time can approach 2 ms.
- Foreign handoff: persistent producer, synchronous channel plus Tokio oneshot
  used only as a runtime-independent notification primitive. Excludes thread
  startup. The producer may finish before the root first polls; this measures
  the complete handoff, not exclusively the blocked-reactor wake latency.
- Write/read: buffered positional file I/O at 64/4096/65536 bytes, stable reused
  aligned buffers, reused pre-sized file, fresh root task per iteration.
  Throughput counts both directions; page cache and completion state access
  are included. No durability claim: no fsync in these cases.
- Batch writes: 1/32/256 concurrent 4 KiB writes, allocating task and buffer per
  operation. Runtime capacity 256, task-turn budget 64. Throughput is ops/s.
- Owner notification: prepare, notify from the owner, skip the kernel wait and
  poll. There is no eventfd write. This replaces the historical retained_wake
  case, which wrote the fd from the owner; their timings are not equivalent.
- Foreign running burst: a persistent producer sends 1024 notifications while
  the owner is running; no eventfd write is needed. Includes command/ack channel
  handoff, amortized across the burst. Reports whole-burst duration.
- Foreign parking handoff: clear an old permit using an expired wait attempt,
  prepare again, request one notification from a persistent producer, wait,
  acknowledge and poll. Includes channel/syscall overhead and possible spurious
  wake completion; excludes thread startup. It is not isolated kernel wake latency.

Startup, shutdown and metadata opens are outside measurement. Criterion raw
samples remain in target/criterion; BASELINE.md and baseline.json retain the
initial environment and estimates. The tests separately force wake timing
around the final readiness check and verify that waits report Ready rather
than merely timing out.

The original native-foundation and subsequent slab-context versions allocated a
per-operation reply mailbox. The current arena-token version stores that state
in the reactor's slab and does not allocate a separate reply box.
See ARENA.md and arena.json for that comparison; earlier archives remain intact.
The SQE carries the slab index. Arena
storage is reserved at startup and grows only when retained completed results
require more entries, rather than allocating a mailbox for every operation.

The current scoped-wake run has 13 cases. Matching owner-thread task wakes use
scoped TLS throughout runtime driving, while foreign notifications use the
RUNNING/PARKING/NOTIFIED/CLOSED handshake. WAKE.md and wake.json retain this
measurement; historical baseline/slab/arena archives are left unchanged.

The I/O core is now a private module within shep-smp-io2. Earlier benchmark
source hashes retain their historical shep-smp-io2-core paths; those archives
are measurement records, not current workspace dependencies. Some archives
also record src/core/mod.rs, which has since moved to src/core.rs.

The current implementation uses the Submitted/Waiting/Ignored/Completed shared
lifecycle. The last party (driver or future) to finish removes the entry. SQEs
are published directly on admission; the reactor batches submission syscalls.
There are no deferred reclamation lists or software submission queues.
LIFECYCLE.md and lifecycle.json retain the comparison with the prior version.
RECLAIM.md and reclaim.json describe the preceding implementation at measurement
time; the older arena/wake archives likewise remain historical records.

The current driver split moves positional file adapters into shep-smp-fs2 and
parking into shep-smp2. shep-smp-io2 exposes owned operations and separate
push/submit/reap phases. Saturation returns the rejected operation immediately;
there is no admission-waiter map. Cancellation targets pin their slab identities
through the cancellation CQE. DRIVER.md and driver-split.json retain the
before/after runtime comparison. Eight additional driver-only cases live in
[io2/benches](../../shep-smp-io2/benches/README.md).

Operation handles now take &Io when polling/releasing and no longer own an Io
clone. The current packed identity has 24 owner bits and 40 slab-index bits.
The [driver comparison](../../shep-smp-io2/benches/OWNERLESS.md) records this
change; previous source hashes and API descriptions remain historical records.
