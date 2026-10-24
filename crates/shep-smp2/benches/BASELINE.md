# Native runtime initial baseline

rustc 1.97.1 (8bab26f4f 2026-07-14); Linux 6.12.113; AMD EPYC 9554P, 16-vCPU KVM VM.
Criterion 0.8.2, default release profile, no CPU pinning, 40 samples,
200 ms warmup and 1 s measurement per case. Other validation jobs were
finished before the benchmark run. Shared VM and buffered-file results are
exploratory measurements, not portable latency/throughput guarantees.

| Case | Median |
|---|---:|
| io/batch_write_4k/1 | 10.67 µs |
| io/batch_write_4k/256 | 705.11 µs |
| io/batch_write_4k/32 | 99.27 µs |
| io/write_read/4096 | 10.99 µs |
| io/write_read/64 | 10.29 µs |
| io/write_read/65536 | 29.47 µs |
| park/retained_wake | 603.5 ns |
| runtime/foreign_handoff | 8.37 µs |
| runtime/ready_root | 242.0 ns |
| runtime/self_wake_128_turns | 6.06 µs |
| runtime/timer_idle_1ms | 2.00 ms |

Batch cases report whole-batch duration; write/read cases include both
operations. Timing boundaries are described in README.md. Mean/median
estimates and confidence intervals are archived in baseline.json; raw samples
remain under target/criterion/**/native-foundation.
