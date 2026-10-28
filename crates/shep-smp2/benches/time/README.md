# Timer performance comparisons

The timer now uses slab-owned entries and forbids unsafe code. The benchmark
uses only public APIs, explicit advancement, and Criterion 0.8 with plotting
and parallel analysis disabled. It performs no I/O or wall-clock sleeps.
Historical measurements are in [BASELINE.md](BASELINE.md); the ownership
migration's timing and heap measurements are in [ARENA.md](ARENA.md).
Instruction, memory-reference, inlining, and page-fault attribution is in
[ATTRIBUTION.md](ATTRIBUTION.md), with a fixed-work probe and gated perf runner.
Applied optimizations and their measurements are in
[OPPORTUNITIES.md](OPPORTUNITIES.md).

## Running

Save a fresh baseline before another implementation change:

```sh
taskset -c 0 cargo bench -p shep-smp2 --bench timers -- \
  --save-baseline before-next-change --noplot
```

Then compare without replacing it:

```sh
taskset -c 0 cargo bench -p shep-smp2 --bench timers -- \
  --baseline before-next-change --noplot
```

There are 56 cases, with 50 samples, one second of warm-up, and three seconds
target measurement per case. Results and samples live under `target/criterion`.
Keep source, compiler, flags, CPU affinity, sample settings, and background load
fixed. Affinity within this VM does not isolate the host CPU. Unchanged-binary
repeats previously drifted substantially; confirm small differences on a stable
machine or with additional instruction/cache evidence.

Preserve named historical baselines: `intrusive-b0a64f3` predates the entry
cleanup; `specialized-entry-list` captures the pointer-based implementation
after that cleanup; `slab-owned-arena` captures the initial ownership change;
`slab-owned-optimized` captures the subsequent three hot-path optimizations.
The new rearm cases and selected hot paths also have a focused
`before-arena-matched` baseline from an isolated copy of commit b5b9a22.

To replay a comparison without new timing, filter to cases present in both:

```sh
cargo bench -p shep-smp2 --bench timers -- \
  '^(register|cancel|reset_extend|reset_shorten|churn|poll_pending|is_elapsed)/' \
  --load-baseline slab-owned-arena --baseline specialized-entry-list --noplot
```

[Criterion's guide](https://criterion-rs.github.io/book/user_guide/command_line_options.html#baselines)
describes baseline options. Separate worktrees should keep compiled Cargo targets
separate and set `CRITERION_HOME` to the same absolute results directory.

Useful shorter checks:

```sh
cargo test -p shep-smp2 --bench timers
taskset -c 0 cargo bench -p shep-smp2 --bench timers -- \
  '^(churn|reset_extend|expire_spread)/1024$' --quick --noplot
```

Quick mode is exploratory. Reported time is per batch; divide by the count for
average time per timer. Confidence intervals are not individual p99 latencies.

## Timing boundaries

The common scaled cases use 1, 1,024, and 32,768 consumers. Pinned boxes keep
the historical fixtures comparable; inline lifecycles and executor tasks also
exercise sleeps without a per-sleep Box.

| Cases | Measured | Excluded |
|---|---|---|
| `lifecycle/timer_create_drop` | Allocate/initialize/drop a wheel and its default slot reservation | Clock read |
| `lifecycle/lazy_inline` | Construct/drop an unpolled inline sleep | Wheel and scope setup |
| `lifecycle/register_cancel_inline` | Stack-pin, register, cancel | Wheel and scope setup; no per-sleep heap allocation |
| `lifecycle/register_cancel_boxed` | Same lifecycle with Box allocation/free | Wheel and scope setup |
| `register/N` | First pending poll, including arena growth if needed | Sleep/Box/fixture setup and cleanup |
| `cancel/N` | Shuffled cancellation, including Box frees | Registration, shuffle, remaining cleanup |
| `reset_extend/N` | Lazy extension | Setup, later cascading, cleanup |
| `reset_shorten/N` | Unlink and reinsert an earlier deadline | Setup/cleanup |
| `expire_dense/N` | Advance through one shared deadline | Registration, subsequent polls, Box frees |
| `expire_spread/N` | Advance through deadlines over 4,096 ticks | Same |
| `expire_levels/N` | Advance through all levels at larger counts | Same |
| `expire_extended/N` | Visit cached buckets and extended deadlines | Registration and initial resets |
| `expire_waker/N` | Spread expiration plus Arc-backed wake/drop | Waker construction, registration, cleanup |
| `churn/N` | Box allocation, registration, extension, cancellation | Reused wheel and Vec capacity; prepared deadlines |
| `next_wake/N` | One query of a populated wheel | Setup/cleanup |
| `poll_pending/N` | Repeated pending polls with the same waker | Setup/cleanup |
| `is_elapsed/N` | Read readiness | Setup/cleanup |
| `executor_expire/N` | Expire, deliver task wakes, finish/drop tasks | Spawning and first pending poll |
| `rearm_elapsed/N` | Reset already elapsed/observed sleeps to future deadlines | Registration, initial expiration/observation, cleanup |
| `arena_registration/C/N` | First registration at or above reserved capacity | C-slot reservation, Sleep/Box construction, cleanup |

Reservation cases use capacities 0, 256, 1,024, and 4,096, filling the reservation
and crossing it by one (zero has one registration case). The other cases use
the default reservation. Cold registration and steady churn must both be
considered: the latter reuses arena slots after warm-up.

Wheel-only cases use a noop waker. The Arc counter case measures real wake
lifetime work separately; executor cases use the task module's actual wakers and
queue. This is component integration without reactor I/O or OS wake latency.
Preflight assertions and benchmark smoke runs check fixture behavior.

## Retained heap snapshots

The source-safe example `arena_memory` prepares an empty, live, expired, or
freed-consumer phase. It deliberately forgets the selected owners so DHAT's
end-of-process live heap represents that phase instead of normal teardown.
The leak is confined to this one-shot profiling fixture.

```sh
cargo build -p shep-smp2 --release --example arena_memory
valgrind --tool=dhat --dhat-out-file=/tmp/timer-live.json \
  target/release/examples/arena_memory live 32768 1024
valgrind --tool=dhat --dhat-out-file=/tmp/timer-freed.json \
  target/release/examples/arena_memory freed 32768 1024
```

Arguments are phase, consumer count, and initial capacity. The fixture uses
boxed sleeps plus a Vec of owners; it is not an estimate of every executor's
task layout. Sum DHAT point fields `eb` and `ebk` for retained requested bytes
and allocation blocks. Figures include common process bookkeeping, not RSS,
allocator rounding, or transient growth peaks. The before/after run used the
same example against the old source, substituting its capacity-less constructor.

`size_of::<Sleep>()` is now 48 bytes, while each reserved slot uses 56 bytes on
this x86_64 build. The full arena can reduce total live bytes, but reservations
and the high-water capacity remain allocated after consumers disappear. See
[the measured table](ARENA.md) before interpreting future size as total savings.

## Correctness

Run the integration tests, docs, and Miri in addition to timing. The library,
benchmarks, and profiling example forbid unsafe code. The integration test for
custom waker Clone reentrancy contains one documented raw-waker construction;
the runtime does not. Production-sized reservation boundaries run natively,
with smaller equivalent transitions under Miri to bound slice-retagging costs.
