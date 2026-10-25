# Implementation bugs found by formal modeling

Checked against `759122e` on 2026-10-22. These are protocol counterexamples
using the existing implementation model plus a source review, not executions
of the Rust binary. Design-only findings remain under `formal/`.

## MODEL-1 — High: stale HELLO replies allow two clusters from overlapping seeds

**Status: open.** Resumes BF1 in `formal/tla/FINDINGS-live-bootstrap.md`.

`seeds_empty` checks each peer separately, without reserving the peer or
persisting a founding identity. `found` later initializes from those answers.
See [controller.rs](crates/shep-wal/src/controller.rs#L138), especially
`seeds_empty` at line 180. Comparing seed lists in each answer does not make
the answers remain true until initialization.

Counterexample:

1. Nodes 1, 2 and 3 configure `{1,2,3}`; node 4 configures `{3,4}`.
2. Node 1 queries nodes 2 then 3. Both report empty with `{1,2,3}`.
3. Node 3 restarts with its seed list corrected to `{3,4}`; node 4 reports
   empty with that list.
4. Nodes 1 and 3 each initialize their controller. Messages from node 1 to
   node 3 can be delayed.
5. Majority `{1,2}` commits one `Found`; majority `{3,4}` commits another.
   Both decisions apply, despite the overlapping configured memberships.

Reproduce from the repository root:

```sh
formal/tla/run.sh BootstrapFoundTrace BootstrapFoundTrace_typo.cfg
```

TLC violates **OneCluster** after 22 transitions (23 distinct states).
The script uses only guarded actions from `BootstrapFound.tla` and respects
the implementation's peer-query order. Ordinary message delay suffices;
there is no disk wipe in this trace.

Founding needs agreement on an identity before independent Raft memberships
can initialize, backed by stable reservations or an external conditional
claim. The proposed alternatives are only checked within the bounds in
`formal/tla/FINDINGS-founding-resume.md`; they are not implemented fixes.

## MODEL-2 — High: reusing a wiped seed's identity loses a committed decision

**Status: open.** Resumes BF2/B2; the new trace demonstrates committed-state
loss, rather than only violating the rule that wiped seeds must stay out.

`start_controller` unconditionally creates/opens the controller replica
([controller.rs](crates/shep-wal/src/controller.rs#L108)). The Raft RPC handlers
dispatch votes and appends to any existing group without an incarnation or
join gate ([node.rs](crates/shep-raft/src/node.rs#L1298), vote at line 1305).
After a wipe, the same configured node ID therefore participates with no
previous log or vote. Waiting for HELLO before locally initializing does
not prevent participation in someone else's election.

Counterexample with unchanged seeds `{1,2,3}`:

1. Node 1 initializes; node 3 receives the initial membership only.
2. Nodes 1 and 2 commit founding identity A. Node 3 has not received A.
3. Wipe node 2 and restart it under the same node ID.
4. Node 3 wins a later term using votes `{2,3}`: node 2 has forgotten A and
   accepts node 3's older log.
5. Nodes 2 and 3 commit founding identity B. Node 1 retains committed A.

Reproduce:

```sh
formal/tla/run.sh BootstrapFoundTrace BootstrapFoundTrace_amnesia.cfg
formal/tla/run.sh BootstrapFoundTrace BootstrapFoundTrace_wipe.cfg
```

The first violates **OneCluster** after 23 transitions (24 distinct states),
with exactly one wipe, no seed-list change and no second initialization.
The second violates **WipedStaysOut** after 14 transitions (15 distinct states).
The first trace omits Raft's automatic leader no-ops, as the parent model
does; those add entries but do not repair the forgotten majority decision.

Reject participation from an empty replacement under the old incarnation.
Require an explicit join with a fresh identity and a safe membership change,
or restoration of the old durable Raft state. A cluster UUID alone is not
enough for an empty disk that has forgotten the UUID too.

Lean's `Shep.Founding.wipe_allows_conflicting_vote` separately checks the
vote/log amnesia premise. It is an abstract protocol lemma, not a proof that
the Rust implementation refines the model.

# Implementation gaps found by testing

Found while adding property tests and decode limits on top of `16667da`
(2026-10-22), in the uncommitted codec and RPC changes that add `Decoder`,
`Limits` and `MAX_PAYLOAD`.

## TEST-1 — High: snapshots are sent as one RPC frame

**Status: open.**

`SnapshotRequest` carries the whole snapshot as one `data: Bytes`
([wire.rs](crates/shep-raft/src/wire.rs#L101), sent from
[network.rs](crates/shep-raft/src/network.rs#L521)). RPC frames are now
capped at `MAX_PAYLOAD` (32 MiB), so a snapshot larger than that fails with
`RpcError::PayloadTooLarge`. A follower that needs it can never catch up once
the leader has purged the log behind the snapshot.

Before the cap, a large snapshot went out as one unbounded frame: a memory
risk on the receiver, and head-of-line blocking for heartbeats sharing the
connection.

Send snapshots in chunks, each well under `MAX_PAYLOAD`, as openraft's
chunked snapshot protocol does.

## TEST-2 — Medium: decode memory accounting is within a factor, not exact

**Status: open.**

`Decoder::charge` ([decoder.rs](crates/shep-codec/src/decoder.rs#L87)) is
charged each element's size, at least one byte
([impls.rs](crates/shep-codec/src/impls.rs#L32)), but not the collections'
own overhead: B-tree nodes allocated for eleven entries at a time, hash table
control bytes and spare buckets, `Vec` growth slack. The memory a decode
holds is bounded, but by about 8× `Limits::max_memory` rather than by the
limit itself; [memory_limit.rs](crates/shep-codec/tests/memory_limit.rs#L23)
asserts `FACTOR = 8`.

With the 64 MiB default, one hostile frame can hold roughly 512 MiB, and
the RPC server's `max_memory` (256 MiB) bounds only request bodies, not what
decoding them allocates.

Charge collection overhead (a B-tree node per eleven entries, hash table
capacity rather than length) so the factor approaches 1, then tighten
`FACTOR` in the test.

## TEST-3 — Low: the WAL chaos test is flaky

**Status: open.** Fails without the codec and RPC changes too.

`chaos_keeps_the_log_serializable` asserts at least three rounds of chaos
([chaos.rs](crates/shep-wal/tests/chaos.rs#L95)), but the workers often finish
their 120 increments before the third round starts, so the test fails on
`only 1 rounds of chaos` before checking serializability. In five alternating
runs it failed four times at `16667da` and three times with the changes.

Run chaos for a fixed number of rounds and let the workers keep going until
it ends, rather than racing the two.
