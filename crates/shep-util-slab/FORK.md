# Safe slab core

Adapted from `slab` 0.4.12, source commit
`a1e4346070a48c936d808de75191dee5d01e433c` in
[tokio-rs/slab](https://github.com/tokio-rs/slab/tree/a1e4346070a48c936d808de75191dee5d01e433c),
using the locally installed crates.io source. The package checksum is
`0c790de23124f9ab44544d7ac05d60440adc586479ce501c1d6d7da3cd8c9cf5`.
The MIT license is preserved in LICENSE-MIT and source/test headers.

The upstream storage core already uses safe Rust: a Vec of occupied values or
vacant-slot next keys, with a free-list head and occupied count. This crate
retains that representation and constant-time checked operations, while
forbidding unsafe code and depending only on std.

## Deliberately small API

Keep only `new`, `with_capacity`, `len`, `is_empty`, `capacity`, `vacant_key`,
`insert`, `get`, `get_mut`, and `remove`; also Default, Debug, Index and IndexMut.
These cover the planned timer arena and the current I/O caller's operations.
There is no normal dependency on upstream slab; its use here is dev-only for
differential tests and benchmarks. Existing callers have not been migrated.

Do not add APIs solely to import more tests. Iterators, draining, clearing,
retaining, cloning, collection builders, vacant-entry handles, capacity
reservation after construction, shrinking, compaction/rekeying, key-of-value,
bulk mutable lookup, unchecked access, and serialization are not included.
Keys are reusable integers, without generation checking or address stability.

Changes from upstream:

- Remove all optional/unchecked APIs and their supporting code. Enforce
  `#![forbid(unsafe_code)]` and keep no normal dependencies.
- Insert directly into an appended or reused slot; increment the occupied
  count after insertion completes. No separate `insert_at` helper is needed.
- Implement checked removal directly without exporting `try_remove`.
- Document lazy occupancy, preallocation, growth, key reuse, and ownership of
  removed values. Preallocation is caller-selected, with no runtime-specific
  default in this utility crate.

## Tests

Port all nine upstream test cases whose operations fit this API:
`insert_get_remove_one`, `insert_get_many`, `insert_get_remove_many`,
`invalid_get_panics`, `invalid_get_mut_panics`, `double_remove_panics`,
`invalid_remove_panics`, `slab_get_mut`, and `const_new`.

Their `contains` assertions use `get(...).is_some()/is_none()` instead of adding
another accessor. The growth test checks sufficient capacity rather than
requiring a particular Vec growth factor. Other behavior is retained.

Additional integration tests cover ownership transfer, exact destruction of
local non-Send payloads, panicking destructors, invalid access/removal leaving
the free list unchanged, zero-sized values, key prediction/reuse, capacity
growth, and Default via `mem::take`.

Two Hegel properties are configured for 1,000 cases each:

- A 150-step state machine compares mixed insert/get/mutate/remove/take/drop
  operations with an independent BTreeMap and upstream slab. Removed values may
  remain owned by the caller while their old slots are reused. Invariants check
  every observed key, vacant-key compatibility, occupied counts, and exact
  payload ownership/destruction; final cleanup must drop every created value.
- Random holes and refills must preserve all surviving keys and stay within
  reserved capacity until that capacity is exceeded.

All tests live under `tests/`. The crate has no source unit-test module.
