# Shep / Redpanda real-file I/O comparison

This compares the new Shep byte-appender stack with Redpanda's production
`storage::segment_appender` from a local checkout. It is a component benchmark,
not a Kafka, Raft, record-format, or recovery benchmark. Redpanda's existing
`segment_appender_rpbench` uses a fake in-memory file, so it cannot answer the
real-disk question.

The [initial measured results](RESULTS.md) include native-polling and
parked-reactor runs, latency/CPU tradeoffs, and the exact reproduction commands.
The [streaming investigation](STREAMING.md) adds longer runs, a CPU-bound
diagnostic, and an isolated test of cooperative scheduling.

## Run

Install Bazelisk and Redpanda's system build prerequisites using the instructions
in the checkout's README. The runner uses its pinned Bazel, LLVM, and Seastar
versions. Build and run from the Shep checkout:

```sh
python3 tools/io-bench/compare.py \
  --redpanda ~/redpanda \
  --output target/io-compare-native \
  --cpu 2 --iterations 1000 --warmup 64 --repetitions 3
```

Choose a CPU in the process's allowed affinity set. Both executables and their
user-space helper threads inherit the same single-CPU affinity. The native
Seastar reactor also receives an explicit matching `--cpuset` and `--smp 1`.
Builds finish before measurements begin; cases run sequentially, and engine
order alternates between trials/cases. Do not rebuild the benchmark executables
concurrently: their hashes are checked before and after the experiment.

The runner installs `shep_io_bench.cc` and a marked BUILD target into
`~/redpanda/src/v/storage/tests`. It changes no Redpanda production source.
`prepare_redpanda.py` performs this installation idempotently and refuses to
replace an unrelated source file. These two checkout changes are deliberate;
the harness's canonical source lives alongside this README.

The default suite covers:

- Durable 97-byte and 4096-byte appends, one file.
- Durable 4096-byte and 4128-byte appends, eight concurrent files.
- Streaming 4096-byte appends, eight files.
- Durable 4096-byte appends plus four direct range readers, eight files.

`--suite full` adds 1/8/32-file cases with 97-byte, 4096-byte, and 128-KiB appends
in both durable and streaming modes. Supply repeated `--case MODE:FILES:BYTES`
arguments to select individual cases. For example:

```sh
python3 tools/io-bench/compare.py --skip-build \
  --output target/io-compare-parked --cpu 2 \
  --case durable:8:4096 --case stream:8:4096 --case mixed:8:4096 \
  --redpanda-polling parked --iterations 1000 --repetitions 3
```

Native Redpanda polling is the default comparison. `parked` additionally sets
`--idle-poll-time-us=0 --poll-aio=0`, to distinguish reactor polling behavior
from appender behavior. Shep uses its normal Tokio/eventfd completion path in
both experiments. CPU consumption is reported so latency/throughput gains do
not hide a larger CPU budget.

## Common workload contract

| Setting | Both implementations |
| --- | --- |
| Measured layer | Buffered byte appender on a real file |
| Backend | Explicit io_uring; no fallback |
| Data | The same repeated `Z` byte buffer; no framing or checksums |
| Files | One sequential producer per file, all producers concurrent |
| Data I/O | O_DIRECT |
| Minimum write unit | 4096 bytes |
| Chunk size | 64 KiB |
| Shared chunk budget | 30 MiB |
| Preallocation increment | 2 MiB |
| Idle write timeout | 1000 ms |
| Durable/mixed | Await append, then await synchronization, for every operation |
| Streaming | Await each append; synchronize every file before stopping the timer |
| Warmup | 64 appends per file, synchronized before timing |
| Readers in mixed mode | Four 64-KiB reads over a separate 32-MiB file, same LCG offsets |
| Verification | All appended bytes and final logical file sizes, outside timing |

Redpanda's chunk cache uses a 4096-byte alignment. Shep's native filesystem
minimum may be smaller, so the Shep benchmark's `Metered` file adapter explicitly
raises its alignment to 4096. This removes geometry as a confounder; production
Shep's lower-alignment optimization is deliberately not used here.

Redpanda's shared chunk budget is selected through its public memory-group
constructor: with optional subsystems and reservations off, the chunk cache has
15 of 87 shares. The harness asserts the resulting budget rather than silently
assuming the ratio still holds after an upstream change. The process gets a
1-GiB Seastar memory arena; this is distinct from the common 30-MiB chunk budget.
Shep uses its normal allocator. This experiment does not compare memory arenas
or total resident-memory budgets.

All files live in separate fresh directories under the same `--data-root`
(default: `target/io-bench-data`). Names and setup are synchronized before the
shared start signal. Directory creation, warmup, fixture creation, appender
close/trimming, verification, CSV writing, and cleanup are outside timing.
Preallocation extensions needed during measurement remain inside timing.

Direct reads bypass the engines' caches and the guest kernel page cache. The
experiment does not flush hypervisor or device caches. There is no filesystem
I/O calibration file or application-level rate limit; each stack's normal
scheduling implementation remains part of what is measured.

## Evidence and interpretation

Each trial checks its measured synchronization count: `files * iterations` for
durable/mixed, and exactly one per file for streaming. Both appenders count
successful physical writes and bytes, allowing write amplification to be
compared directly. Percentiles use identical integer-rank selection. The runner
recomputes every percentile and checks operation counts against the saved CSV,
rejecting mismatched geometry, durability, backend, or verification results.

The output directory contains:

- `environment.json`: CPU/affinity/quota, kernel, filesystem, source revisions
  and dirty status, executable hashes, common settings, and arguments.
- `results.jsonl`: every trial's result and complete execution command.
- `*.latencies.csv`: individual operation samples, before sorting.
- `*.stdout.txt` / `*.stderr.txt`: original process outputs.
- `summary.csv`: medians and min/max ranges for throughput, latency, CPU,
  physical write amplification, and mixed read latency.
- `summary.md`: the primary comparison table.
- `build-*.log`: build outputs when not using `--skip-build`.

The reported p99/p99.9 are medians of individual trials' percentiles, not pooled
percentiles. Streaming operation latency measures buffer acceptance, while
streaming throughput includes the final synchronization. CPU time includes all
process threads; mixed-mode CPU includes readers. Context-switch counts are
also recorded. Read samples may include the final in-flight reads completing
immediately after the producer window closes, identically on both sides.

This is a closed-loop workload. It measures achievable service under a bounded
number of concurrent producers; it does not measure open-loop overload latency
or coordinated-omission-corrected response time. Initial runs use release
builds without PGO. Results from a VM's virtual disk describe that VM, not a
production NVMe deployment.

Successful trials remove only their own temporary data directories. Failed
trials retain data and logs for diagnosis. Use `--keep-data` to retain successful
files too, and always use a fresh results directory for a new experiment.
