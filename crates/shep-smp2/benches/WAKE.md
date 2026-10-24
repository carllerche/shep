# Scoped owner routing and conditional eventfd signaling

The runtime enters its existing safe weak TLS executor context across the whole
loop. Matching owner wakes enter the local ready queue; owner mismatches use
an inbox without a driver wake. Foreign notification CASes a four-state parking
handshake; only a foreign PARKING -> NOTIFIED transition claims an eventfd write.
Native tests check the actual counter and driver-waker calls, rather than using
elapsed benchmark timings as evidence that a syscall was skipped.

The four new tests in park.rs plus its delayed-wake test check suppression,
coalescing and guard restoration; park_state.rs has an ordinary state regression
check. The executor scope/routing tests also pass under Miri.

The 13-case scoped-wake run used the same settings as arena-token. Matching
cases are compared below. These are sequential, unpaired shared-VM measurements;
the ratios do not establish portable performance guarantees. Kernel scheduling
affects the foreign handoff and buffered-file cases. Earlier archives stay intact.

| Case | Arena median (ns) | Scoped wake median (ns) | Change |
|---|---:|---:|---:|
| io/batch_write_4k/1 | 11136.4 | 9086.5 | -18.4% |
| io/batch_write_4k/256 | 599467.9 | 640802.9 | +6.9% |
| io/batch_write_4k/32 | 90941.7 | 122063.6 | +34.2% |
| io/write_read/4096 | 11248.6 | 11258.1 | +0.1% |
| io/write_read/64 | 9902.9 | 15608.7 | +57.6% |
| io/write_read/65536 | 29828.3 | 29598.6 | -0.8% |
| runtime/foreign_handoff | 15647.5 | 15008.1 | -4.1% |
| runtime/ready_root | 227.3 | 236.7 | +4.1% |
| runtime/self_wake_128_turns | 6112.8 | 6341.5 | +3.7% |
| runtime/timer_idle_1ms | 1999980.9 | 1999541.9 | -0.0% |

The historical park/retained_wake case wrote the eventfd from the owner.
It is replaced by park/owner_notification, which deliberately skips that write
and the kernel wait. These are not equivalent timing boundaries. The new
foreign cases separately exercise running suppression and parking handoff.

| New case | Median (ns) |
|---|---:|
| park/foreign_parking_handoff | 13699.7 |
| park/foreign_running_burst_1024 | 16760.6 |
| park/owner_notification | 138.9 |

Timing boundaries are in README.md. wake.json retains mean/median confidence
intervals, compiler/kernel metadata and source hashes. Raw samples remain under
target/criterion/**/scoped-wake.
