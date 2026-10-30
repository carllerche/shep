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

Criterion writes raw samples under `target/criterion`. Keep compiler, CPU affinity,
profile and benchmark source identical between runs. Repeat measurements on a
stable machine before drawing conclusions from small differences.

Allocation checks reserve arena capacity and construct buffers/resources before
measuring:

```sh
cargo nextest run -p shep-smp2 --features allocation-tests --test io_allocations
```
