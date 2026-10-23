# Investigating streaming throughput

The initial result is not evidence of a fixed Redpanda throughput advantage.
Longer disk runs changed the ordering, while a CPU-bound diagnostic slightly
favored Shep. We did identify a causal contributor to the acceptance-latency
gap: Tokio's cooperative scheduling. Disabling that cooperation changes where
waiting appears in the latency distribution; it is not a general optimization.

## Longer disk runs

The initial streaming case measured only 31.25 MiB, with trial durations of
roughly 45–60 ms. That is too short to characterize sustained disk service.
We repeated the same protocol with 100,000 appends per file: 800,000 appends
and 3.125 GiB per process. Each experiment used three runs per engine, CPU 2,
eight files, 4096-byte appends, and the original common geometry.

| Experiment | Shep MiB/s median (range) | Redpanda MiB/s median (range) | Shep CPU µs/append | Redpanda CPU µs/append |
| --- | ---: | ---: | ---: | ---: |
| Native polling | 929 (850–934) | 1293 (862–1374) | 2.119 | 2.119 |
| Parked reactor | 1400 (1320–1425) | 910 (864–1357) | 2.115 | 2.193 |

Both engines performed exactly 50,000 physical writes of 64 KiB, wrote the same
3,276,800,000 bytes, made 1560 preallocation calls, and completed eight measured
synchronizations per run. Write amplification was 1.0×. Redpanda's result is
therefore not explained by fewer writes, larger writes, or weaker final
durability in this workload.

In the native experiment, Redpanda completed a similar amount of CPU work in
less wall time: median CPU utilization was 70.1%, versus Shep's 49.4%. This is
consistent with different waiting or overlap, rather than a large difference
in CPU cost per append. It does not identify the cause of the waiting.

The parked experiment is particularly important: **Shep's executable and
settings were unchanged, yet its median throughput rose by about 51%.** The
experiments ran sequentially, so changing Redpanda's polling setting cannot by
itself explain that shift. VM/device conditions and other variation remain
confounders. The throughput reversal must not be presented as a measured causal
effect of disabling Redpanda's polling.

## Removing the block device from the comparison

We ran the same long workload on `/dev/shm`, a tmpfs filesystem, to exercise
the appenders, kernel file operations, runtime scheduling, and copying without
the virtual block device. This is a diagnostic, not a durability benchmark;
tmpfs synchronization does not establish persistence on a disk.

| Engine | MiB/s median (range) | CPU utilization | CPU µs/append | Append p99 µs |
| --- | ---: | ---: | ---: | ---: |
| Shep | 2921 (2838–3153) | 100% | 1.337 | 17.45 |
| Redpanda, native polling | 2873 (2737–2894) | 100% | 1.359 | 2.61 |

Shep was slightly ahead when both were CPU-bound. This does not prove its CPU
path is always better, but it provides no support for a large inherent
throughput penalty from the current layering. Redpanda still had a substantially
lower append-acceptance p99, so that difference warranted a separate test.

## Controlled scheduling ablations

An isolated copy under `target/io-stream-snapshot` retained the same production
code. The copied benchmark added a switch around its measured append call:

```rust
tokio::task::unconstrained(appender.append(&payload)).await?
```

The baseline used the ordinary `appender.append(&payload).await?`. Both branches
were in the same executable, selected once per producer before timing. Engine
order alternated over three trials. The copied harness also recorded the raw
driver's submission/completion batch counters and scheduler timing totals
outside the timed CPU/wall window. Production sources were not changed.

| Filesystem | Variant | MiB/s median | Append p99 µs | Append p99.9 ms |
| --- | --- | ---: | ---: | ---: |
| tmpfs | Normal cooperation | 3014 | 16.04 | 3.288 |
| tmpfs | Unconstrained append | 2900 | 3.59 | 4.633 |
| ext4 on `/dev/vda` | Normal cooperation | 969 | 77.76 | 6.052 |
| ext4 on `/dev/vda` | Unconstrained append | 1449 | 27.90 | 9.539 |

Changing cooperation alone greatly reduced p99 in both environments. This
establishes that runtime cooperation contributes to the latency gap. It did not
eliminate the disk p99 gap, and p99.9 became worse in both diagnostics.

The tmpfs throughput declined by about 4%; the disk throughput rose, with wide
trial ranges. Disabling cooperation allows a producer to do more work before
other tasks run, changing batching and where delays occur. A lower acceptance
p99 consequently does not by itself imply better I/O service, fairness, or
overall application latency. Removing cooperation wholesale is not justified.

A second isolated ablation removed only the raw driver's explicit
`yield_now()` after a wakeup. On tmpfs, baseline throughput was 3079 MiB/s
(3004–3156), versus 2852 MiB/s (2826–3192) without the yield. Median p99 was
22.20 versus 24.48 µs. This test did not establish a benefit from removing that
yield, so the explicit yield alone is not a supported explanation for the gap.

## What the source says, and what remains unresolved

Shep's append path takes a per-file Tokio mutex and can yield through Tokio's
cooperative budget. Dispatch crosses a detached writeback task, the scheduler's
detached execution task, and a raw-driver completion channel. Its ring is
integrated through eventfd/AsyncFd. Redpanda uses Seastar continuations and a
reactor-integrated completion path. These are plausible places to investigate
pipeline latency and batching; the measurements above do not assign a
throughput cost to each one.

Several tempting explanations are not unique to Shep. Both appenders copy into
chunks and clear recycled chunk memory. Both exclude conflicting writes during
space reservation, then allocate and adjust file length. Redpanda's normal
append path also cancels and rearms an idle timer. Those operations need
measurement before being blamed for a throughput difference.

The useful next investigation is a phase-specific trace on the intended disk:
enqueue/admission, kernel submission, completion observation, and producer
resumption, alongside actual queue occupancy. It should interleave polling
variants, run long enough to reveal device variability, and preserve other
tasks' latency when evaluating scheduling changes. The current evidence
supports a scheduling contribution to p99; it does **not** establish one root
cause for the original throughput gap.

## Reproduction and saved evidence

Run from the Shep checkout, choosing fresh output directories:

```sh
python3 tools/io-bench/compare.py --skip-build \
  --output target/io-compare-stream-long-native --cpu 2 \
  --iterations 100000 --warmup 64 --repetitions 3 --case stream:8:4096

python3 tools/io-bench/compare.py --skip-build \
  --output target/io-compare-stream-long-parked --cpu 2 \
  --iterations 100000 --warmup 64 --repetitions 3 --case stream:8:4096 \
  --redpanda-polling parked

python3 tools/io-bench/compare.py --skip-build \
  --output target/io-compare-stream-tmpfs-native --cpu 2 \
  --data-root /dev/shm/shep-io-investigation \
  --iterations 100000 --warmup 64 --repetitions 3 --case stream:8:4096
```

The three standard experiment directories contain environment metadata, binary
hashes, commands, process output, raw samples, and summaries. Additional local
ablation evidence is in `target/io-stream-unconstrained-tmpfs`,
`target/io-stream-unconstrained-disk`, and
`target/io-stream-no-driver-yield-tmpfs`. Their `results.jsonl` files retain
every result and driver diagnostic; raw sample files were independently audited.
The isolated harness and its runner are retained under
`target/io-stream-snapshot`. These generated directories are ignored by Git.
