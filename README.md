# Shep

A versioned graph data service for internal apps, on its own Raft-replicated
log. See [docs/design.md](docs/design.md) for the design.

The implementation is being built from the WAL up (milestone 2, §18). The
WAL is modeled on Redpanda: thread-per-core, Redpanda's RPC framing and
serialization, segmented logs, and a controller group that every node
replicates.

## Layout

| Crate | What it does |
|---|---|
| `crates/shep-version` | Dots and dotted version vectors (§9) |
| `crates/shep-codec` (+ `-derive`) | Binary serialization with versioned envelopes, after Redpanda's `serde` |
| `crates/shep-futures` | The few future and stream utilities used in place of the `futures` crate, single-threaded: `join_all`, `Shared`, `FuturesOrdered`, `block_on`, `catch_unwind`, `unfold`, `next` |
| `crates/shep-rpc` | General-purpose request/response RPC framing, after Redpanda's: a 32-byte checksummed header, key/value metadata the service defines, frame I/O |
| `crates/shep-rpc-server` | Serving it: concurrent handlers, routing on the header and metadata before the body is read |
| `crates/shep-rpc-client` | Calling it: pipelined calls over one connection, reconnect with backoff, a connection cache |
| `crates/shep-smp` | Thread-per-core: a `LocalRuntime` per core, each with its own state, reached with `submit_to` |
| `crates/shep-smp-task` | Local task ownership and execution, adapted from Monoio; the first component of the future SMP runtime |
| `crates/shep-smp-exec` | Local FIFO executor with scoped `spawn`, bounded task turns, and driver wakeups |
| `crates/shep-smp-time` | Reactor-owned hierarchical timing wheel, sleeps, timeouts, and intervals, adapted from Monoio |
| `crates/shep-util-slab` | Safe reusable indexed storage, with the small API needed by runtime components |
| `crates/shep-smp-io` | Core-local io_uring files, owned aligned buffers, and explicit I/O lifetime management |
| `crates/shep-smp-io-scheduler` | Weighted admission with shared physical-device capacity and per-class metrics |
| `crates/shep-append` | Format-independent buffered append files with explicit written and durable positions |
| `crates/shep-storage` | Per-core storage, after Redpanda's: segmented logs with coalesced fsync and crash recovery, and a key-value store |
| `crates/shep-raft-engine` | openraft 0.10.0-alpha.37, forked (see its `FORK.md`), with its test suite |
| `crates/shep-raft-memstore` | openraft's test-support store, vendored |
| `crates/shep-raft` | Embeddable multi-raft per core. Each group runs the embedder's `Machine`, which admits proposals on the leader and applies them everywhere. The crate handles void entries, rebuilding leader-side state, idempotency keys, commit times and tailing. Also: pipelined appends, heartbeats batched per node pair, logs on `shep-storage`, node-local core placement |
| `crates/shep-wal-api` | The WAL's client protocol |
| `crates/shep-wal` | The WAL, as an embedder of `shep-raft`: a group per partition whose machine validates proposals (§10.3), and the node's side of the controller |
| `crates/shep-wal-controller` | The WAL controller, a `Machine` too: members, log sets and partitions, rebuilt from its log (§9.6). A universe is a log set under Shep's rules |
| `crates/shep-wal-client` | Client: proposals routed to leaders, long-poll log tailing, log sets and universes |
| `crates/shep-node` | The `shep-node` binary: every service in one process |
| `tests/test-node` | End-to-end tests that run `shep-node` processes |
| `formal/lean`, `formal/tla` | Lean proofs and TLA+ models of the design's semantics and protocols |

Each crate's interface is tested from `crates/<crate>/tests/`; there are no
new unit tests in `src` (vendored crates keep their upstream tests). Tests that
span many components go in `tests/test-<aspect>` crates.

## Running

```sh
cargo test --workspace
```

A single server is a complete deployment, without replication:

```sh
cargo run -p shep-node -- --id 1 --listen 127.0.0.1:7001 --data-dir ./data/1
```

A cluster names its founding members on every node:

```sh
shep-node --id 1 --listen 10.0.0.1:7000 --data-dir /var/lib/shep \
  --seed 1=10.0.0.1:7000 --seed 2=10.0.0.2:7000 --seed 3=10.0.0.3:7000
```

## Status

- **Built:** the WAL on its own Raft, validation with read and write sets,
  idempotency, leader-change rebuilds, crash recovery, long-poll tailing,
  the controller (founding from seeds, universes with their meta and data
  partitions, each node reconciling its replicas), and the single binary.
- **Next:** node join after founding, partition moves and splits, the
  feature table, cluster identity checks on connect, lightweight heartbeats,
  follower-admitted recovery, moving replicas between cores, then the
  materializer.

Storage I/O architecture, lifecycle, and benchmark commands: [docs/io.md](docs/io.md).
