# Owned-operation driver benchmarks

```sh
cargo bench -p shep-smp2 --bench driver -- --save-baseline owned-driver
cargo bench -p shep-smp2 --bench driver -- --test
```

Thirteen cases isolate the driver from task scheduling and file-adapter futures:

- Nop batches at 1/32/256 operations: push a batch, explicitly submit/tick,
  then consume each completion. The handle vector is reserved and reused.
- Positional 4 KiB write batches at 1/32/256 operations: owned aligned buffers,
  handle/buffer vectors and pre-sized page-cache-backed file are reused. Buffer
  allocation, file creation, fsync and task scheduling are outside measurement.
- Ignored Nop batch of 256: explicitly release submitter handles before submission and retire
  the kernel-owned records. Includes abandoned-record destruction during tick.
- Real cancellation batches of 256 pending PollAdd operations at 0/1/10/50/100%
  target-owner abandonment. Cancellation owners are always dropped immediately;
  target abandonment boxing and kernel cancellation are inside measurement.
- Cancel identity pin/release: acquire/drop a CancelTarget for an unconsumed,
  completed operation. Measures safe identity bookkeeping, without a cancel SQE
  or syscall. Actual kernel cancellation has native correctness tests.

The driver is reused; normal drain and destruction happen outside measurement.
Defaults are 40 samples, 200 ms warmup and 1 s measurement. Batch throughput is
operations/sec. Results include submit/wait syscall costs; these are not pure
slab instruction timings. There is no eventfd watch or executor in these cases.

These are initial baselines for the new low-level API. The end-to-end before/
after comparison is in [the runtime suite](../../shep-smp2/benches/DRIVER.md).
Detailed historical estimates and metadata are available in Git history;
Criterion writes raw samples under target/criterion. VM timing variation limits
small performance conclusions; dedicated-machine runs are still required.

Typed operation owners now retain their Io and concrete data. The arena contains
completion metadata and only receives boxed inputs on unfinished abandonment.
Write batches use concrete WriteAt owners; older archives used enum operations.
[OWNERLESS.md](OWNERLESS.md) describes the historical eight-byte
ID-only API and its comparisons, not the current ownership contract.

## Typed ownership measurements

Measured with rustc 1.97.1 on x86_64-unknown-linux-gnu using
`RUSTC_BOOTSTRAP=1 cargo rustc -p shep-smp2 --lib -- -Zprint-type-sizes`.
The baseline is commit ad4fd4c7ff531826a61b6f3272f0c5c82e837005.

| Layout | Before | After |
|---|---:|---:|
| Record / actual slab slot | 128 | 48 |
| Lifecycle | 24 | 24 |
| Enum owner | 8 (identity only) | 88 (includes data and Io) |
| Concrete ReadAt data | in 72-byte Operation enum | 56 |
| Op<ReadAt> | not present | 72 |
| File read_at / write_at future | 296 | 176 |
| Separately allocated file activity guard payload | 8 | 0 |
| File future + slab slot + guard payload | 432 | 224 |

Sizes are bytes and include data moved out of the arena. Future sizes include
their embedded operation owners; do not add those again. Totals exclude backing
buffer storage, shared objects, spare slab capacity and allocator overhead.
The per-slot reduction is 62.5%; the measured file-future-plus-slot reduction is
48.1%. This is a memory comparison, not evidence of throughput improvement.

Native allocation assertions use preallocated arena capacity and reused resources:

```sh
cargo nextest run -p shep-smp2 --features allocation-tests --test io_allocations
```

They verify zero allocations for normal typed file submission, retirement and
consumption, closed-admission rejection, explicit cancellation and cancellation
owner abandonment. Unfinished read abandonment allocates once; completed pinned
abandonment allocates zero times. The feature enables a test-only System allocator
wrapper; do not enable it for weft, whose simulated allocator is incompatible.
The ordinary `cargo weft test -p shep-smp2` suite covers ownership without that
instrumentation. Driver and runtime benchmark smoke tests use `-- --test`.

## Per-CQE tick comparison

The driver now retires and delivers each CQE before processing the next, with
no normal-path notification vectors. The comparison against the pre-tick working
tree used these commands on the same VM:

```sh
cargo bench -p shep-smp2 --bench driver -- --save-baseline before-tick --sample-size 10 --warm-up-time 0.2 --measurement-time 0.5
cargo bench -p shep-smp2 --bench driver -- --baseline before-tick --sample-size 40 --warm-up-time 0.2 --measurement-time 1
```

The final comparison ran without concurrent builds. Criterion reported:

| Case | Time change | Classification |
|---|---:|---|
| Nop / 1 | +0.4% | No significant change |
| Nop / 32 | +3.2% | Within noise threshold |
| Nop / 256 | +0.6% | No significant change |
| Write 4 KiB / 1 | +5.7% | No significant change |
| Write 4 KiB / 32 | -4.1% | No significant change |
| Write 4 KiB / 256 | -12.5% | Improvement |
| Ignored Nop / 256 | -5.1% | Improvement |
| Cancel identity pin/release | -13.1% | Improvement |

Earlier runs flagged regressions, including a run overlapping compilation.
The final run flagged none, but the short baseline and shared-VM variability
limit causal conclusions; these results do not establish a general speedup.
Criterion samples are in `target/criterion`, with the baseline named `before-tick`.
The runtime benchmark suite also passed in smoke-test mode (`-- --test`).
