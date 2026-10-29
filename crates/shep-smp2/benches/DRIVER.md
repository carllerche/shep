# Owned-operation driver split measurements

Baseline source: `2781fd96cb18504a925ab93a29a7baa44c62b1c7`. Before binaries were compiled before the
refactor; the repeat baseline reran that same binary. Rustc 1.97.1 (8bab26f4f 2026-07-14),
Linux 6.12.113, shared exe.dev VM. The primary comparison pins CPU 2:
producer threads inherit this affinity, so foreign handoffs here cross threads
but do not measure inter-core notification latency. Default Criterion settings:
40 samples, 200 ms warmup, 1 s measurement. Startup/shutdown excluded.

The final CPU-2 full-run mean times below retain every case, including apparent regressions.
Earlier candidate runs and focused follow-ups are available in Git history.
These end-to-end I/O and cross-thread timings vary substantially between runs;
they do not establish performance equivalence or a reliable regression percentage.

| Case | Before mean | After mean | Change |
|---|---:|---:|---:|
| io_batch_write_4k/1 | 3.107 µs | 3.665 µs | +18.0% |
| io_batch_write_4k/256 | 769.306 µs | 712.527 µs | -7.4% |
| io_batch_write_4k/32 | 32.487 µs | 32.632 µs | +0.4% |
| io_write_read/4096 | 3.648 µs | 3.630 µs | -0.5% |
| io_write_read/64 | 3.568 µs | 3.753 µs | +5.2% |
| io_write_read/65536 | 9.169 µs | 8.792 µs | -4.1% |
| park_foreign_parking_handoff | 9.713 µs | 9.933 µs | +2.3% |
| park_foreign_running_burst_1024 | 15.558 µs | 15.527 µs | -0.2% |
| park_owner_notification | 0.136 µs | 0.127 µs | -6.5% |
| runtime_foreign_handoff | 6.532 µs | 6.522 µs | -0.2% |
| runtime_ready_root | 0.222 µs | 0.227 µs | +2.1% |
| runtime_self_wake_128_turns | 6.172 µs | 6.321 µs | +2.4% |
| runtime_timer_idle_1ms | 2000.653 µs | 1999.587 µs | -0.1% |

The redundant watch checks introduced in the first split were removed: parking
preparation relies on the watch armed by new/poll, and empty CQ polls avoid an
arena borrow. Owner notification improves in the final pinned comparison;
ready-root performance has no statistically detected change. No eventfd write was added to owner or running-thread wakes.

The 64 KiB write/read case illustrates the uncertainty: candidate reported
central times across the three full runs were 31.41 µs, 27.99 µs, 69.33 µs.
The one-write batch also changes direction between repeats. The focused 64-byte
80-sample, 3-second comparison reverses the first run's apparent slowdown.
On CPU 2, the focused 80-sample 64 KiB run reports 9.00 µs before and 9.12 µs
after, with no statistically detected change (p=0.18). A traced smoke operation
has the same submission/wait syscall sequence; the additional ring syscall is
for the separate, unmeasured parking fixture at shutdown. Dedicated-machine
repetitions remain necessary before claiming that performance is unchanged.
The pinned full run flags the one-write batch (+18%) and foreign parking
(+2.3%) as regressions; larger batches improve or remain close to baseline.
A longer pinned one-write follow-up (80 samples, 3 seconds) gives +2.1%,
with no statistically detected change (p=0.11). The small parking difference
remains a measured cost of this generic-operation implementation. These results
are retained rather than hidden behind a blanket speedup claim.

Eight [driver-only cases](io/README.md) establish an initial
baseline for explicit batching, buffer reuse, ignored completions and cancel
identity references. They exclude task scheduling and per-iteration buffer
allocation; they are not comparable with the old private driver API.

The split adds no admission backlog or software submission queue. File APIs
move to fs2; runtime eventfd policy moves to smp2. The lifecycle keeps the entry
until submitter release and every kernel identity reference have retired.
The benchmark file-operation workload and capacity stay unchanged.

Validation: 208 SMP tests, 11 lifecycle + 2 buffer + 1 parking-state Miri checks,
Clippy and rustdoc with warnings denied; all eight new benchmark smoke cases pass.
