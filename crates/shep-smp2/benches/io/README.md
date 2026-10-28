# Owned-operation driver benchmarks

```sh
cargo bench -p shep-smp2 --bench driver -- --save-baseline owned-driver
cargo bench -p shep-smp2 --bench driver -- --test
```

Eight cases isolate the driver from task scheduling and file-adapter futures:

- Nop batches at 1/32/256 operations: push a batch, explicitly submit/reap,
  then consume each completion. The handle vector is reserved and reused.
- Positional 4 KiB write batches at 1/32/256 operations: owned aligned buffers,
  handle/buffer vectors and pre-sized page-cache-backed file are reused. Buffer
  allocation, file creation, fsync and task scheduling are outside measurement.
- Ignored Nop batch of 256: explicitly release submitter handles before submission and retire
  the kernel-owned records. Includes abandoned-record destruction during reap.
- Cancel identity pin/release: acquire/drop a CancelTarget for an unconsumed,
  completed operation. Measures safe identity bookkeeping, without a cancel SQE
  or syscall. Actual kernel cancellation has native correctness tests.

The driver is reused; normal drain and destruction happen outside measurement.
Defaults are 40 samples, 200 ms warmup and 1 s measurement. Batch throughput is
operations/sec. Results include submit/wait syscall costs; these are not pure
slab instruction timings. There is no eventfd watch or executor in these cases.

These are initial baselines for the new low-level API. The end-to-end before/
after comparison is in [the runtime suite](../../shep-smp2/benches/DRIVER.md).
Compiler, kernel, source hashes and estimates are retained in driver.json;
Criterion raw samples remain under target/criterion. VM timing variation limits
small performance conclusions; dedicated-machine runs are still required.

Operation handles now contain only an eight-byte packed owner/slot ID. The
caller supplies Io for polling and explicitly releasing raw claims; async file
adapters release through borrowed guards. [OWNERLESS.md](OWNERLESS.md) and
ownerless.json retain the comparison with Io-owning handles, including owner
validation costs and all measured regressions. Earlier driver.json remains a
historical snapshot of the preceding implementation.
