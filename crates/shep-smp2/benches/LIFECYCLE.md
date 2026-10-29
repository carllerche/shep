# Shared driver/future lifecycle

The implementation was compared with Monoio at commit
8675e9406ea7943c441d969e38c3f5fc198ef6d1, especially driver/uring/lifecycle.rs,
driver/uring/mod.rs and driver/op.rs. The shared slab entry tracks Submitted,
Waiting(waker), Ignored or Completed(result). Driver retirement and future
consumption/drop are the two release conditions; whichever happens last removes
the entry. The owned Operation stays in our slab throughout, avoiding Monoio's
future-owned typed data and boxed ignored-data allocation.

This replaces the prior deferred Consumed entries and intrusive reclamation
list. SQEs are now published during admission, so the pending VecDeque is gone.
The reactor batches syscalls and extracts owned wakers while draining the CQ;
notification dispatch never looks up old indices. The intermediate CQ snapshot
allocation and per-operation reply context are also gone. The private core
still contains four unsafe blocks; the lifecycle module forbids unsafe code.

Native regression tests consume or drop B in A's callback, immediately reuse
B's slot for C, and verify that dispatching B's previously extracted waker does
not notify C. Other tests cover completion before polling, cancellation before
completion, buffer/fd retention, results consumed after driver destruction,
bounded admission and ownership on rejected publication. Native/release tests,
Clippy with warnings denied, documentation with warnings denied, and eight
Miri lifecycle/parking tests pass. Actual io_uring cannot run under Miri.

The baseline retains the final measurements from the preceding reclamation
implementation. Both complete runs used 40 samples, 200 ms warmup and 1 s
measurement, on the same unpinned shared VM. No ready-root change was detected
(change estimate +0.88%, 95% interval -1.45% to +3.41%, p=0.50).

Results are mixed: buffered write/read improved relative to that baseline,
while the 32/256-operation batches and foreign-parking case were flagged as slower. A
longer follow-up (1 s warmup, 3 s measurement) reduced the 256-operation batch
from about 1.03 ms to 486 us without a code change; it still flagged roughly
20% higher time relative to the baseline. The 32-operation batch also remained
flagged, while foreign parking no longer showed a detectable difference.
These shared-VM runs do not establish performance parity. They are retained
in full rather than selecting only favorable measurements.

To check that signal, the saved pre-change source was built separately and
compared with the final binary on CPU 2, in before/after/before order. Both
versions used 40 samples, 1 s warmup and 3 s measurement. The 32-operation
batch measured about 33.6 / 34.4 / 34.6 us (Criterion slope estimates), with
overlapping intervals. The 256-operation batch measured 678 / 749 / 665 us:
the new version was about 11-13% slower than the adjacent baseline runs.
CPU pinning does not isolate this VM, but this batch-performance signal
warranted further profiling. Detailed historical measurement records are available in Git history.

Profiles of the same two binaries showed allocator/memory operations dominating
user-space samples, without an isolated lifecycle hot spot. To normalize the
amount of work, a temporary fixed-count workload ran 10,000 batches of 256
4 KiB writes, again in before/after/before order on CPU 2. Its user instruction
counts were 10.36 / 10.46 / 10.40 billion (within about 1%). User CPU times were
1.92 / 1.48 / 1.70 s; system CPU times were 5.94 / 2.33 / 4.21 s. Total elapsed
times were 7.89 / 3.82 / 5.92 s. This did not reproduce a slowdown; kernel-time
variation dominated the timing differences. The historical archive in Git history includes the workload source,
perf stat output and binary hashes.

These additional measurements do not justify discarding the Criterion signal
or claiming a speedup. They do show that the slowdown was not consistent across
workloads/runs, and that a performance-parity conclusion needs a more controlled
I/O measurement environment. No speculative optimization was added to the
lifecycle transitions to chase those timing changes.

| Case | Before median (ns) | After median (ns) | Change |
|---|---:|---:|---:|
| io/batch_write_4k/1 | 9659.0 | 9122.5 | -5.6% |
| io/batch_write_4k/256 | 413285.2 | 1056390.1 | +155.6% |
| io/batch_write_4k/32 | 67975.3 | 122979.9 | +80.9% |
| io/write_read/4096 | 19247.2 | 11208.2 | -41.8% |
| io/write_read/64 | 10335.2 | 10590.9 | +2.5% |
| io/write_read/65536 | 43029.4 | 28740.0 | -33.2% |
| park/foreign_parking_handoff | 8521.9 | 12703.7 | +49.1% |
| park/foreign_running_burst_1024 | 22852.3 | 19043.9 | -16.7% |
| park/owner_notification | 138.2 | 134.8 | -2.4% |
| runtime/foreign_handoff | 12893.0 | 9235.0 | -28.4% |
| runtime/ready_root | 231.6 | 229.2 | -1.1% |
| runtime/self_wake_128_turns | 6184.7 | 6373.5 | +3.1% |
| runtime/timer_idle_1ms | 1999733.1 | 1999774.5 | +0.0% |

Raw samples were saved under target/criterion/**/lifecycle-before and **/new.
Detailed estimates and metadata are available in Git history.

```sh
cargo bench -p shep-smp2 --bench runtime -- --baseline lifecycle-before
```
