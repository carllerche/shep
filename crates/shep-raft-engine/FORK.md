# Fork of openraft

This crate is forked from [openraft](https://github.com/databendlabs/openraft)
`v0.10.0-alpha.37` (commit `728e66bba05c4305dd37016535d1c247a35d2059`),
the `openraft/` crate, under its MIT or Apache-2.0 license (copies in this
directory).

Shep keeps openraft's engine (elections, vote rules, log matching and
truncation, commit, joint-consensus membership) and replaces its per-group
runtime with a per-core multi-group driver modeled on Redpanda (`shep-raft`).

The fork keeps openraft's unit tests inside `src` as an exception to Shep's
no-unit-tests rule, because they guard the engine; they move to integration
tests over time.

Changes from upstream are listed below.

- `tests/` holds openraft's integration tests (`tests/tests` upstream), with
  one line added to each test target's root: `extern crate shep_raft_engine
  as openraft;`. Their support crate `memstore` is vendored as
  `shep-raft-memstore`.
- openraft's `legacy` crate (the v1 network API, which sends snapshots in
  chunks) is not vendored: Shep installs snapshots with the full-snapshot
  API. Removed with it: memstore's `SnapshotReceiverFactory` impl, the
  `InstallSnapshot` variants of the test fixtures' `RpcRequest` and
  `RpcResponse`, the `legacy` exports in `tests/public_api_test.rs`, the
  tests `snapshot_streaming/t10_api_install_snapshot.rs` and
  `t10_api_install_snapshot_with_lower_vote.rs`, and the v1 request case of
  `snapshot_streaming/t10_wire_compat_09.rs`.
- `tests/replication/t52_reset_backoff.rs`: `transfer_leader_resets_target_backoff_by_default`
  waits for node-2's applied index to reach `log_index`, not to equal it.
  Under the `single-threaded` feature, which Shep uses, the new leader's own
  blank entry can apply before the exact index is observed.
- `src/lib.rs` allows `clippy::for_kv_map` and `clippy::arc_with_non_send_sync`,
  which fire only under `single-threaded`.
- `rustfmt.toml` in each vendored crate disables formatting: upstream formats
  with nightly-only options, and the sources stay as upstream wrote them.
