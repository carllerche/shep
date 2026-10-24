# Generational operation arena

Accepted operations, result state and wakers now live in one generational slab.
The future retains a reactor handle and token; the SQE user_data is that packed
slot/generation. Per-operation Rc<RefCell<Reply>> mailboxes are gone. The only
Rc/RefCell for entry storage is shared once at reactor scope.

The slab-context run measured the preceding mailbox implementation. The same
11 cases and settings were rerun as arena-token. These are unpaired shared-VM
measurements, not a statistically established speedup/regression. In particular
file batching and foreign handoff vary with scheduling. arena.json retains
estimates, confidence intervals and source hashes; the earlier data remains
unchanged. No extra validation jobs ran during this benchmark run.

| Case | Mailbox median (ns) | Arena median (ns) | Change |
|---|---:|---:|---:|
| io/batch_write_4k/1 | 10199.9 | 11136.4 | +9.2% |
| io/batch_write_4k/256 | 671212.1 | 599467.9 | -10.7% |
| io/batch_write_4k/32 | 127203.6 | 90941.7 | -28.5% |
| io/write_read/4096 | 11788.3 | 11248.6 | -4.6% |
| io/write_read/64 | 10214.2 | 9902.9 | -3.0% |
| io/write_read/65536 | 29227.6 | 29828.3 | +2.1% |
| park/retained_wake | 635.4 | 579.1 | -8.9% |
| runtime/foreign_handoff | 9372.1 | 15647.5 | +67.0% |
| runtime/ready_root | 248.4 | 227.3 | -8.5% |
| runtime/self_wake_128_turns | 6122.5 | 6112.8 | -0.2% |
| runtime/timer_idle_1ms | 2000105.7 | 1999980.9 | -0.0% |
