# shep-util-slab

Reusable integer-keyed storage for the timer arena and runtime components.
The implementation adapts the safe core of slab 0.4.12, has no normal
dependencies, and enforces `#![forbid(unsafe_code)]`. See [FORK.md](FORK.md) for
the exact source, intentionally small API, and ported tests.

`Slab::with_capacity(n)` reserves storage without inserting entries. Insertion
reuses vacant slots before growing; existing keys survive growth, but addresses
may change. Removal returns ownership of the value and releases its key for
reuse. Keys have no generation check. The slab retains capacity until dropped.

The utility has no default preallocation count. The timer proposal selects
1,024 entries per core; callers can choose their own capacity.

```sh
cargo test -p shep-util-slab
cargo test -p shep-util-slab --bench slab
cargo clippy -p shep-util-slab --all-targets -- -D warnings
```

Hegel tests run in the ordinary test suite; its native engine is not interpreted
by Miri. Run Miri against the deterministic ported/ownership tests. If Hegel's
build script tries to build its native engine under Miri, set `HEGEL_C_LIB_DIR`
to the `out/lib` directory containing an engine built by ordinary `cargo test`.

```sh
cargo +nightly miri test -p shep-util-slab --lib --test upstream --test ownership
```

## Performance comparison

The benchmark compares upstream slab and this implementation in the same
binary, using the same checked APIs, shuffled access/removal order, 64-byte
payloads, and populations of 1,024 and 32,768 values. It has 16 cases:

- Growth from zero, including allocation and final destruction.
- Churn in a preallocated slab, including insertion and shuffled removal.
- Checked lookup of a populated slab.
- Checked mutable lookup plus updating a payload word.

Churn reuses backing storage; the growth case includes allocation. Lookup cases
exclude population. Key/shuffle preparation is outside the measured work.
There are no custom timing traits, allocators, or unsafe benchmark helpers.
Container metadata size is printed, separately from allocated entry storage.

```sh
taskset -c 0 cargo bench -p shep-util-slab --bench slab -- \
  --save-baseline initial-safe-core --noplot
```

Criterion uses 50 samples, a one-second warm-up, and a three-second target
measurement per case. Results are under `target/criterion`. Keep benchmark
source, compiler, CPU affinity, and settings fixed for before/after changes.
The shared VM has substantial elapsed-time repeat drift; a single comparison
does not establish a small performance advantage or regression. Matching core
algorithms avoids a deliberate representation tradeoff, but timing still needs
measurement rather than assuming equivalence.
