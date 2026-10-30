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

Startup, shutdown and metadata opens are outside measurement. Criterion writes
raw samples under `target/criterion`. Shared VM timings need repeated runs before
attributing small differences to code changes.

Component suites:

- [Driver](io/README.md)
- [Tasks](task/README.md)
- [Timers](time/README.md)
