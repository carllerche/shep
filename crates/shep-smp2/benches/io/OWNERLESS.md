# Packed operation handles

OperationHandle now contains one optional NonZeroU64, shrinking from 24 bytes
to 8 bytes and removing the per-operation Io Rc clone/drop. The high 24 bits
identify its allocating Io; the low 40 bits hold its slab index. Owner IDs are
allocated once per driver, never reused, and permanently stop at exhaustion.

Raw handles must be consumed or explicitly release(&Io). The file adapter
uses a borrowed cleanup guard, so dropped async operations still release their
claim without cloning Io. CancelTarget retains Io only while its extra pin is
active and releases that reference on retirement, preventing a completed
cancellation record from retaining its own shared slab.

Both versions were measured on CPU 2 using the same compiler/kernel and
workspace dependency lock, 40 samples, 200 ms warmup, and 1 s measurement. The
before source was saved before editing and rebuilt in an isolated workspace.
Source hashes and full historical estimates are available in Git history.

| Driver case | Before mean (ns) | After mean (ns) | Change |
|---|---:|---:|---:|
| driver_cancel_identity_pin_release | 4.37 | 4.65 | +6.4% |
| driver_ignored_nop_batch_256 | 28777.59 | 26370.10 | -8.4% |
| driver_nop_batch/1 | 212.27 | 210.56 | -0.8% |
| driver_nop_batch/256 | 26350.43 | 26111.85 | -0.9% |
| driver_nop_batch/32 | 3267.26 | 3227.10 | -1.2% |
| driver_write_4k_batch/1 | 1964.40 | 2143.64 | +9.1% |
| driver_write_4k_batch/256 | 243824.07 | 242331.33 | -0.6% |
| driver_write_4k_batch/32 | 27335.82 | 26881.19 | -1.7% |

These shared-VM timings are measurements, not a blanket performance guarantee.
Cancel pin/release with owner validation measures about 0.28 ns more in this run.
The isolated single-write case flags a slowdown; larger write and Nop batches
remain close to baseline. Ignored completion retirement is faster in this run.
A longer single-write follow-up (80 samples, 3 seconds, same CPU) gives -2.9%,
within Criterion's noise threshold. Both the original flagged result and the
follow-up are retained in the JSON; there is no reproducible single-write
slowdown established by these runs.

Validation covers wrong-Io rejection for every handle method with coincident
slab indices, stale tokens after driver destruction, explicit/idempotent
release, forgotten completed cancellation cleanup, full ID bit boundaries, and
permanent owner-ID exhaustion. The SMP suite passed 213 tests on rerun after an
unchanged shep-smp test initially panicked when repolling a completed async fn.
13 pure lifecycle/identity tests and 2 buffer tests passed under Miri. Clippy,
rustdoc with warnings denied, and the 13 runtime benchmark smoke cases pass.
