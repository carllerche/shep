# Intrusive timer baseline

Measured on 2026-10-23 against runtime commit `b0a64f3`, using the benchmark
harness introduced alongside this report. No timer implementation was changed.

- x86_64 exe.dev KVM VM, AMD EPYC 9554P; pinned to VM CPU 0 with `taskset`.
- `rustc 1.97.1 (8bab26f4f 2026-07-14)`, LLVM 22.1.6.
- Cargo's default bench profile; no `RUSTFLAGS` or encoded Rust flags set.
- Criterion 0.8.2, 50 samples, 1 second warm-up, 3 second target measurement.
- All 46 cases saved under `target/criterion/**/intrusive-b0a64f3/`.
- Sleep: 120 bytes inline. Timer: 24 bytes inline. Heap storage is additional.

Representative estimates below divide the measured batch time by the timer
count. The intervals are Criterion's 95% confidence intervals for those
estimates. They are not timer latency percentiles or performance budgets.

| Case | Timers | ns/timer | 95% interval, ns/timer |
|---|---:|---:|---:|
| Inline construct/register/cancel | 1 | 31.53 | 31.27–31.82 |
| Boxed construct/register/cancel | 1 | 54.51 | 53.88–55.02 |
| Register | 1,024 | 14.38 | 14.13–14.69 |
| Cancel, including Box free | 1,024 | 15.74 | 15.47–16.03 |
| Lazy extension | 1,024 | 8.76 | 8.59–8.96 |
| Shorten and relink | 1,024 | 15.58 | 15.33–15.85 |
| Expire, one shared deadline | 1,024 | 5.38 | 5.16–5.65 |
| Expire, spread deadlines | 1,024 | 13.02 | 12.81–13.25 |
| Expire, six levels | 1,024 | 9.79 | 9.68–9.93 |
| Expire, counted Arc waker | 1,024 | 13.00 | 12.81–13.25 |
| Allocation-inclusive churn | 1,024 | 67.77 | 66.59–69.11 |
| Expire and complete executor tasks | 1,024 | 80.68 | 80.06–81.48 |
| Repoll pending sleep | 1,024 | 2.69 | 2.63–2.76 |
| Read `is_elapsed` | 1,024 | 0.73 | 0.72–0.75 |

The absolute values are specific to this shared VM and workload. Use the
[before/after procedure](README.md#running) on the same machine to evaluate
candidate implementations; preserve both the inline and allocation-inclusive
cases so a storage change is not hidden by benchmark fixture allocation.

## Repeatability check

A subsequent run with the **same final benchmark binary**, timer implementation,
CPU affinity, and sampling parameters produced the following changes. Negative
means faster. There was no runtime change to explain any improvement/regression.

| Case | Reported mean time change | 95% interval |
|---|---:|---:|
| Inline register/cancel | +28.97% | +21.74% to +36.64% |
| Churn, 1,024 timers | −9.74% | −12.14% to −7.63% |
| Churn, 32,768 timers | +26.29% | +22.54% to +30.27% |
| Executor expiration, 1,024 tasks | +6.76% | +3.95% to +9.65% |

These results make small performance claims on this VM inconclusive. Narrow
within-run confidence intervals do not account for between-run drift or changes
in the surrounding benchmark workload. The saved baseline is useful for running
comparisons, but not a reliable regression gate here. Confirm candidate changes
on a sufficiently stable machine; instruction/cache profiling can provide
additional evidence when elapsed-time measurements remain this variable.

## Outer-cell probe

An isolated copy replaced `TimerEntry.inner: UnsafeCell<TimerShared>` with
`TimerShared`, constructed it directly, and replaced `unsafe { &*self.inner.get() }`
with `&self.inner`. The now-unused `UnsafeCell` import was removed. The wheel,
list links, state fields, registration/cancellation behavior, and pinning were
unchanged. Sleep and Timer still measured 120 and 24 bytes.

All 42 unit/integration tests passed in that copy under
`nightly-2026-10-22` Miri, including the model and reentrant cancellation tests.
The same benchmark source and sampling settings were used for nine cases,
sharing the original baseline via `CRITERION_HOME` while keeping the probe's
compiled targets separate.

| Case | Reported mean time change vs baseline |
|---|---:|
| Inline register/cancel | +7.46% |
| Boxed register/cancel | +4.86% |
| Lazy extension, 1,024 timers | −4.79% |
| Spread expiration, 1,024 timers | +1.61% |
| Churn, 1,024 timers | +1.04% |
| Churn, 32,768 timers | +4.07% |
| Repoll pending, 1,024 timers | −9.95% |
| Read readiness, 1,024 timers | −1.21% |
| Executor expiration, 1,024 tasks | −4.12% |

Given the repeatability results above, these measurements establish neither a
speedup nor a regression. The probe is a candidate for a small cleanup with no
algorithm/allocation change, subject to a source-level aliasing review; Miri
passing alone does not prove soundness. At the time of this probe it was not
applied; the following cleanup now includes it and the list specialization.

## Applied outer-cell removal and list specialization

The workspace now stores TimerShared directly and uses a concrete EntryList
with direct next/previous cells. The generic unsafe Link trait, its conversion
hooks, Pointers wrapper, and ManuallyDrop/type bookkeeping are gone. The four
upstream list tests now use real pinned timer entries and retain their original
FIFO, neighbor-link, repeated-removal, and absent-node checks.

Production unsafe functions decreased from 16 to 13 and the custom unsafe trait
was removed. There are still 24 unsafe blocks: removing two dereferences is
balanced by two explicit insertion call sites. Insertion now states the
non-owning handle's liveness/pinning obligation directly rather than relying
on the removed generic trait. Memory ownership still requires pinning and
unlink-before-drop until an arena replaces the non-owning pointers.

All 42 tests pass under Miri, all 24 doctests pass, all 46 benchmark cases pass
their smoke checks, and Clippy/workspace checks pass. Sleep remains 120 bytes
and Timer 24 bytes on x86_64; no allocation strategy or wheel algorithm changed.

All 46 cases were measured with the same CPU affinity and sampling settings and
saved as `specialized-entry-list`. The table uses Criterion's time estimates
divided by batch size, in ns/timer, as in the original baseline above.

| Case | Original | Applied cleanup |
|---|---:|---:|
| Inline register/cancel | 31.53 | 32.18 |
| Boxed register/cancel | 54.51 | 51.69 |
| Register, 1,024 timers | 14.38 | 14.12 |
| Cancel, 1,024 timers | 15.74 | 16.16 |
| Lazy extension, 1,024 timers | 8.76 | 9.29 |
| Shorten/relink, 1,024 timers | 15.58 | 14.48 |
| Spread expiration, 1,024 timers | 13.02 | 13.98 |
| Six-level expiration, 1,024 timers | 9.79 | 10.70 |
| Churn, 1,024 timers | 67.77 | 62.52 |
| Churn, 32,768 timers | 66.74 | 75.89 |
| Repoll pending, 1,024 timers | 2.69 | 2.46 |
| Read readiness, 1,024 timers | 0.73 | 0.72 |
| Executor expiration, 1,024 tasks | 80.68 | 88.25 |

Results move in both directions. Given the unchanged-binary repeat drift
documented above, these timings establish neither a speedup nor a regression.
They do preserve measurements for the next experiment. To replay the comparison
without collecting new timings:

```sh
cargo bench -p shep-smp2 --bench timers -- \
  --load-baseline specialized-entry-list --baseline intrusive-b0a64f3 --noplot
```
