# Shep

A versioned graph data service for internal apps, on its own Raft-replicated
log and SlateDB storage. See [docs/design.md](docs/design.md) for the design.

This is a fresh start: the earlier Postgres prototype (milestone 1) is gone,
and the implementation is being rebuilt from the WAL up (milestone 2, §18).

## Layout

| Crate | What it does |
|---|---|
| `crates/shep-version` | Dots, dotted version vectors (§9) |
| `crates/shep-wal-api` | The WAL's client protocol: gRPC definitions and the types around them |
| `crates/shep-wal` | The WAL: a Raft group per partition (openraft), durable log segments, leader validation (§10.3) |
| `crates/shep-wal-client` | Client for the WAL: proposals routed to leaders, log tailing |
| `formal/lean` | Lean 4 proofs of the semantics the design relies on |
| `formal/tla` | TLA+ models of the protocols; `WalValidate.tla` specifies the WAL |

Each crate's interface is tested from `crates/<crate>/tests/`. There are no
unit tests in `src`. Tests that span many components go in
`tests/test-<aspect>` crates.

## The WAL

A WAL node hosts replicas of any number of partitions and serves three gRPC
services on one address: `shep.wal.v1.Wal` (propose, tail, status),
`shep.wal.v1.WalAdmin` (create and initialize partitions), and
`shep.wal.raft.v1.Raft` between replicas. Protos are compiled in-process
with `protox`, so building needs only Cargo.

- **Proposals** carry a read version `S`, a context, a read set and a write
  set of key ranges, an opaque payload, and a required idempotency key. The
  leader rejects with `contended` if an entry in its validation table that
  `S` doesn't contain wrote inside the read set, and with `too_old` if `S` is
  older than the table. Write sets enter the table when the entry is handed
  to Raft, before it commits.
- **Leader changes.** A new leader rebuilds its validation and idempotency
  tables from its own log, committed or not, before accepting. Each entry
  records the term it was validated in; one appended under any other term is
  void, which closes the race between a leader learning its term and Raft
  appending.
- **Idempotency.** A key's first decided outcome is its only outcome. A
  front end whose re-execution fails records the failure as the key's
  outcome (`Failure` proposals), per finding FR-W3 in `WalValidate.tla`.
- **Readers** stream committed entries from any replica (`Tail`).
- **Storage.** Checksummed records in segment files, written before `append`
  returns and synced by a background flusher that batches; the vote and
  snapshots are replaced atomically. A torn final record is truncated on
  recovery. Nothing is purged yet: archiving comes later (§10.4).

## Running

```sh
cargo test --workspace
```
