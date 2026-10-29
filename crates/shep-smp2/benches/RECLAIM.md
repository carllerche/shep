# Reactor-owned slot reclamation

Futures take completed values and clear their tokens, but leave their slab entry
occupied as Consumed. Only the reactor removes entries after dispatching the
entire notification batch or before starting the next turn. An intrusive list
links consumed entries; its head is a Cell beside the arena RefCell. This avoids
an extra queue allocation, a slab scan, and an arena borrow when nothing needs
reclaiming. Data tokens are now plain slab indices, with no generations or
retirement tombstones. Kernel resources stay owned until the terminal CQE.

Native regression tests cover consuming and cancelling B inside A's wake
callback, submitting C, and then processing B's old notification. C cannot reuse
B's index during that batch and receives no incorrect notification. Arena tests
also check that reuse starts only after reactor reclamation; Miri covers those
safe state transitions.

The baseline was captured immediately before this change. All 13 final cases use
the same 40-sample, 200 ms warmup, 1 s measurement settings as that baseline.
The first prototype used a separate reclamation queue and borrowed the arena on
every empty reclamation attempt. A longer follow-up (1 s warmup, 3 s measurement)
still flagged a small ready-root slowdown. The final intrusive list avoids
those empty borrows; Criterion detected no ready-root change in the final run
(change estimate +0.93%, 95% interval -0.14% to +2.10%, p=0.11).

File I/O and foreign handoff remain too variable on this unpinned shared VM to
establish performance parity. The final comparison flagged foreign handoff,
4 KiB write/read and 64 KiB write/read as slower, while all three batch-write
cases were faster. The prototype's 64 KiB write/read timing changed from about
62 us to 27 us during its longer follow-up without a code change. These
measurements do not establish either a portable speedup or absence of regression.

| Case | Before median (ns) | Final median (ns) | Change |
|---|---:|---:|---:|
| io/batch_write_4k/1 | 11504.9 | 9659.0 | -16.0% |
| io/batch_write_4k/256 | 766041.3 | 413285.2 | -46.0% |
| io/batch_write_4k/32 | 134231.7 | 67975.3 | -49.4% |
| io/write_read/4096 | 10341.3 | 19247.2 | +86.1% |
| io/write_read/64 | 10101.8 | 10335.2 | +2.3% |
| io/write_read/65536 | 28388.4 | 43029.4 | +51.6% |
| park/foreign_parking_handoff | 14066.3 | 8521.9 | -39.4% |
| park/foreign_running_burst_1024 | 23063.9 | 22852.3 | -0.9% |
| park/owner_notification | 137.6 | 138.2 | +0.4% |
| runtime/foreign_handoff | 10351.4 | 12893.0 | +24.6% |
| runtime/ready_root | 231.5 | 231.6 | +0.0% |
| runtime/self_wake_128_turns | 6138.6 | 6184.7 | +0.8% |
| runtime/timer_idle_1ms | 1999764.8 | 1999733.1 | -0.0% |

Detailed estimates and source hashes are available in Git history. The baseline
compiler/kernel metadata is shared with the final run. Raw samples were saved
under target/criterion/**/reactor-reclaim-before and **/new.

Reproduce the comparison with:

```sh
cargo bench -p shep-smp2 --bench runtime -- --baseline reactor-reclaim-before
```
