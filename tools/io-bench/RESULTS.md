# Initial real-file comparison — 2026-10-23

Shep's new appender achieved higher throughput with eight concurrent durable
files in this VM. Redpanda led the native-polling streaming case and had lower
mixed-workload read latency. These results identify useful strengths and gaps;
they do not establish production WAL or Kafka performance parity.

The later [streaming investigation](STREAMING.md) shows substantial variability
in longer disk runs and identifies a scheduling contribution to append p99.
The short streaming result below should be read with that follow-up.

## Setup

Both release executables used one CPU (CPU 2), strict io_uring, O_DIRECT,
4096-byte alignment, 64-KiB chunks, a shared 30-MiB chunk budget, and 2-MiB
preallocation increments. Each file had one producer, 64 warmup appends, and
1000 measured appends. Durable/mixed cases synchronized every append; streaming
cases included one final synchronization per file in their throughput timing.
Mixed cases added four direct 64-KiB readers over a separate 32-MiB file.

The host was an exe.dev VM exposing an AMD EPYC 9554P, Linux 6.12.113, and an
ext4 filesystem on `/dev/vda`. All files used that same filesystem. No device
or hypervisor caches were flushed. Both builds were optimized without PGO.
Redpanda source was `4254992f6115e92c1dbd86f90bb0aee0b869f15e`; Shep was the
working tree based on `6f21953d736e801a710d45854517ee7acfa2e2c2`, including the
new I/O architecture. Exact executable hashes and dirty status are saved with
each experiment.

This compares `shep-append` on the scheduled async file stack with Redpanda's
production `storage::segment_appender`. Record framing, checksums, recovery,
replication, and Kafka are outside the measured layer. See the
[workload contract](README.md#common-workload-contract).

## Native Redpanda polling

Medians of three separate process runs per engine/case; p99 is the median of
each run's p99, rather than a pooled percentile. CPU time includes every process
thread and, in mixed mode, the readers.

| Mode | Files | Append bytes | Shep MiB/s | Redpanda MiB/s | Shep p99 ms | Redpanda p99 ms | Shep CPU µs/append | Redpanda CPU µs/append |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Durable | 1 | 97 | 0.16 | 0.18 | 2.004 | 2.414 | 94.79 | 349.23 |
| Durable | 1 | 4096 | 3.76 | 4.06 | 3.147 | 3.018 | 111.78 | 368.57 |
| Durable | 8 | 4096 | 16.29 | 9.62 | 3.975 | 7.440 | 37.51 | 275.13 |
| Durable | 8 | 4128 | 16.19 | 9.56 | 4.368 | 7.321 | 45.38 | 218.42 |
| Mixed | 8 | 4096 | 16.80 | 10.51 | 5.949 | 9.252 | 89.29 | 351.17 |
| Streaming | 8 | 4096 | 581.20 | 691.17 | 0.079 | 0.023 | 3.55 | 3.10 |

Eight-file durable throughput was about 1.69× Redpanda's, with a lower median
p99. Single-file durable throughput was slightly lower. Native streaming
throughput was about 16% lower, and its append-acceptance p99 was higher.
Streaming acceptance latency excludes the final synchronization, which is
included in throughput.

Mixed write throughput was about 1.60× Redpanda's, but read p99 was 0.570 ms
versus Redpanda's 0.452 ms. This is a scheduling tradeoff worth investigating,
not an across-the-board mixed-I/O win. The engines also completed different
numbers of reads because the producer windows and read service rates differed;
raw counts and samples are retained.

Physical write amplification matched in every paired case: 1.0× for aligned
4096-byte appends, 1.984× for 4128-byte durable appends, and 43.24× for 97-byte
durable appends. Both stacks therefore paid the same partial-page rewrite cost.

## Parked-reactor diagnostic

An additional experiment set Redpanda's `--idle-poll-time-us=0 --poll-aio=0`.
Shep's normal eventfd/Tokio completion path was unchanged. This helps expose
sensitivity to reactor polling; it is a separate experiment, not a replacement
for the native comparison.

| Mode | Files | Bytes | Shep MiB/s | Redpanda MiB/s | Shep p99 ms | Redpanda p99 ms | Shep CPU % | Redpanda CPU % |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Durable | 1 | 4096 | 2.65 | 3.75 | 4.155 | 3.264 | 10.1 | 9.5 |
| Durable | 8 | 4096 | 14.47 | 8.49 | 6.718 | 9.426 | 18.1 | 11.4 |
| Mixed | 8 | 4096 | 16.07 | 10.31 | 5.717 | 9.832 | 36.2 | 36.2 |
| Streaming | 8 | 4096 | 713.03 | 589.54 | 0.055 | 0.023 | 40.7 | 56.2 |

The eight-file durable advantage persisted. Native polling accounts for much
of Redpanda's CPU consumption in the first experiment. Streaming throughput
was variable: parked-run ranges were 513–763 MiB/s for Shep and 560–662 MiB/s
for Redpanda, so those medians do not establish a reliable streaming advantage.
Native-run streaming ranges were 532–648 and 664–700 MiB/s, respectively.

## Reproduce and inspect

```sh
python3 tools/io-bench/compare.py \
  --output target/io-compare-native --cpu 2 \
  --iterations 1000 --warmup 64 --repetitions 3

python3 tools/io-bench/compare.py \
  --output target/io-compare-parked --cpu 2 \
  --iterations 1000 --warmup 64 --repetitions 3 \
  --redpanda-polling parked \
  --case durable:1:4096 --case durable:8:4096 \
  --case stream:8:4096 --case mixed:8:4096
```

Use a fresh output directory when rerunning. The original local evidence is in
[`target/io-compare-native`](../../target/io-compare-native/summary.md) and
[`target/io-compare-parked`](../../target/io-compare-parked/summary.md). These
generated directories are ignored by Git; this report preserves their main
findings. Each contains all raw latencies, process output, commands, machine
metadata, executable hashes, and CSV summaries with trial ranges.

All 60 measured processes (30 engine pairs) passed byte/length verification,
exact synchronization-count assertions, and independent percentile checks
against the raw samples. This remains a short, closed-loop VM experiment;
longer runs on the intended production disk are needed to characterize steady
state, open-loop overload, and multi-core device contention.
