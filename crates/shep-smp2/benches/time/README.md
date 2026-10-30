# Timer performance comparisons

The timer now uses slab-owned entries and forbids unsafe code. The benchmark
uses only public APIs, explicit advancement, and Criterion 0.8 with plotting
and parallel analysis disabled. It performs no I/O or wall-clock sleeps.

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

Useful shorter checks:

```sh
cargo nextest run -p shep-smp2 --bench timers
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

## Correctness

Run the integration tests, docs, and Miri in addition to timing. The library
and benchmarks forbid unsafe code. The integration test for
custom waker Clone reentrancy contains one documented raw-waker construction;
the runtime does not. Production-sized reservation boundaries run natively,
with smaller equivalent transitions under Miri to bound slice-retagging costs.
