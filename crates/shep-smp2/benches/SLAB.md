# Reply context in the operation slab

The core now stores the caller context alongside its operation in one slab
entry. The facade no longer maintains a HashMap or performs a reply lookup.
Completion/rejection/error progress returns the associated context directly.

Comparison with the initial native-foundation baseline, using the same 11
cases and timing settings. These sequential shared-VM runs are exploratory;
small changes and timer/handoff variation are not evidence of a speedup or
regression. The new samples were saved as slab-context.

| Case | Before median (ns) | Slab median (ns) | Change |
|---|---:|---:|---:|
| io/batch_write_4k/1 | 10672.5 | 10199.9 | -4.4% |
| io/batch_write_4k/256 | 705109.6 | 671212.1 | -4.8% |
| io/batch_write_4k/32 | 99268.1 | 127203.6 | +28.1% |
| io/write_read/4096 | 10985.4 | 11788.3 | +7.3% |
| io/write_read/64 | 10290.1 | 10214.2 | -0.7% |
| io/write_read/65536 | 29469.1 | 29227.6 | -0.8% |
| park/retained_wake | 603.5 | 635.4 | +5.3% |
| runtime/foreign_handoff | 8374.9 | 9372.1 | +11.9% |
| runtime/ready_root | 242.0 | 248.4 | +2.6% |
| runtime/self_wake_128_turns | 6064.7 | 6122.5 | +1.0% |
| runtime/timer_idle_1ms | 1999829.0 | 2000105.7 | +0.0% |

A longer confirmation run (1 s warmup, 3 s measurement) also showed
substantial VM variance. Its medians are below.
No speedup or regression is established from these unpaired runs.

| Case | Confirmation median (ns) |
|---|---:|
| io/batch_write_4k/256 | 599100.4 |
| io/batch_write_4k/32 | 133658.9 |
| io/write_read/4096 | 11521.4 |
