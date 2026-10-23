# Initial safe-core comparison

Measured on 2026-10-23, in one benchmark binary, against upstream slab 0.4.12.
The two implementations use the same payloads, access orders, and harness code.

- exe.dev KVM VM, x86_64 AMD EPYC 9554P; pinned to VM CPU 0.
- rustc 1.97.1, default Cargo bench profile.
- Criterion 0.8.2; 50 samples, 1 second warm-up, 3 second measurement target.
- Container metadata: 40 bytes for both implementations. Payload: 64 bytes.
- All 16 cases saved as `initial-safe-core` under
  `target/criterion/slab_{growth,churn,get,get_mut}/`.

The table divides time per batch by the number of values. Brackets are 95%
confidence intervals on those time estimates. A churn element includes both
insertion and removal; growth includes allocation and final destruction. The
lookup cases exclude population; mutable lookup also updates one payload word.

| Case | Values | Shep, ns/element | Upstream, ns/element |
|---|---:|---:|---:|
| Growth | 1,024 | 8.90 [8.79–9.01] | 8.51 [8.41–8.62] |
| Growth | 32,768 | 8.24 [8.10–8.42] | 8.36 [8.21–8.53] |
| Preallocated churn | 1,024 | 12.77 [12.61–12.92] | 12.62 [12.38–12.91] |
| Preallocated churn | 32,768 | 21.62 [21.22–22.05] | 23.05 [22.32–23.92] |
| Lookup | 1,024 | 0.69 [0.68–0.69] | 0.75 [0.72–0.79] |
| Lookup | 32,768 | 1.34 [1.30–1.38] | 1.31 [1.28–1.34] |
| Mutable lookup/update | 1,024 | 0.94 [0.93–0.95] | 0.92 [0.90–0.95] |
| Mutable lookup/update | 32,768 | 1.61 [1.54–1.68] | 1.62 [1.53–1.74] |

Initial timings are close and move in both directions. They do not establish a
small performance advantage or regression: unchanged timer benchmarks on this
same shared VM previously showed substantial between-run drift. These are
component measurements, not timer-arena or I/O integration results.

## Correctness checks

- Nine applicable upstream tests, plus eight ownership/edge-case tests, pass.
- Two Hegel properties, each configured for 1,000 cases, pass. The state machine
  runs up to 150 operations per case, comparing public behavior with an
  independent map and upstream slab while accounting for every payload drop.
- Miri passes all 17 deterministic tests; Hegel's native engine runs outside
  Miri. See the crate README for the native build-directory setting if needed.
- LLVM coverage: 100% of functions, 97.01% of lines. The two uncovered lines
  are the internal invariant-failure arms in insertion and removal.
- Clippy with denied warnings, strict rustdoc, and workspace checks pass.

The library is 160 source lines including documentation and trait implementations,
has no normal dependencies, and forbids unsafe code. Existing runtime callers
are unchanged; the timer-arena proposal now selects this utility.
