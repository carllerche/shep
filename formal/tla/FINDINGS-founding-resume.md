# Resumed controller-founding investigation (2026-10-22)

Investigates controller founding against implementation `759122e`.
Open implementation bugs are in root `BUGS.md`: MODEL-1 (BF1) and MODEL-2
(BF2/B2). This report distinguishes implementation failures from proposed
fixes. No implementation fix was made.

## Reproducible implementation failures

`BootstrapFoundTrace.tla` extends `BootstrapFound` and selects a sequence
of its existing guarded actions, recording an extra step counter. It adds
no protocol behavior. Each configuration also checks `TraceIncomplete`;
the intended safety invariant must fail, rather than the script blocking
or simply running to its end without demonstrating the bug.

| Configuration | Required violation | Transitions | Distinct states |
|---|---|---:|---:|
| `BootstrapFoundTrace_typo` | `OneCluster` | 22 | 23 |
| `BootstrapFoundTrace_amnesia` | `OneCluster` | 23 | 24 |
| `BootstrapFoundTrace_wipe` | `WipedStaysOut` | 14 | 15 |

All three completed in under one second, with TLC exit code 12 and the
intended violation. The typo trace follows the Rust implementation's
ascending HELLO peer-query order. It changes node 3's configuration only
after node 1 received its answer, then commits separate identities using
quorums `{1,2}` and `{3,4}`.

The amnesia trace strengthens the earlier report: `{1,2}` first commits A,
while node 3 has only the original membership. Wipe node 2; `{2,3}` can then
elect the stale node 3 and commit B. The final state's `applied` contains
two IDs. This requires no seed-list change, second initialization, message
reordering within a replication stream, or simultaneous disk wipes.

Source mapping: `shep-wal/src/controller.rs` opens the controller before
founding, collects unreserved HELLO observations, and later initializes.
`shep-raft/src/node.rs` forwards votes/appends for an existing group without
an incarnation check. Its wire requests carry group/vote/log data, without
a persisted founding reservation or empty-replica join authorization.

Limits: the inherited model abstracts whole-log replication, persistence
and elections, and omits automatic leader no-ops and pre-vote rounds.
Extra no-ops and successful pre-vote rounds do not restore a wiped seed's
forgotten log. These are model counterexamples plus source review, not
Rust integration tests or machine-checked source refinements.

## Proposed fixes

Both proposed alternatives remain unlanded. A passing finite model is
evidence only within its bounds; it is not a general correctness proof.

| Configuration | Result | Distinct states | Time |
|---|---|---:|---:|
| `BootstrapFound_typo_promise_t1` | pass | 1,454 | 1s |
| `BootstrapFound_same3_promise_t1` | pass | 5,725 | 1s |
| `BootstrapFound_same3_claim_t1` | pass | 914,089 | 16s |
| `BootstrapFound_typo_claim_t1` | pass | 1,544,000 | 34s |
| `BootstrapFound_typo_connpromise` | pass | 20,798,540 | 7m29s |

These inherited reduced configurations use one term and at most one
message in flight. The identical-seed cases allow one crash and one wipe;
the typo-claim case additionally allows one configuration change. The
typo-pledge case allows one crash/configuration change and no wipe.
The one-term bound cannot exercise MODEL-2's later-term election.

The completed, larger connection-pledge case uses two terms, at most two
messages in flight, one crash and one seed-list change, with no disk wipe.
It checks `OneCluster`, `NoMix`, `NoCross` and `NoReinit`, to search depth 48.
It does not establish the safety of reusing an identity after a disk wipe,
or liveness after a stranded initializer. It exercises connection adoption
and closure rules as well as durable pledges.

The larger `BootstrapFound_same3_promise` run was deliberately stopped
after about three minutes to obtain short completed regressions first.
Its last progress report showed 9,244,655 distinct states and an unfinished
queue. **This is not a pass.**

Founding is reachable under both candidates even with the reduced bounds:
`BootstrapFound_promise_found` violates `NeverFounded` (107 distinct states),
as does `BootstrapFound_claim_found` (379 distinct states). Both complete
in under one second. These are reachability witnesses, not liveness proofs;
they rule out an entirely disabled founding path in the passing cases.

## Lean obligations

`../lean/ShepZSet/Founding.lean` adds:

- `overlapping_pledges_unique`: if two founding sets overlap and their
  common seed retains the same pledge, they must choose the same ID.
  No axioms.
- `wipe_allows_conflicting_vote`: a previously voting seed with the newer
  log refuses an older candidate, but clearing its persistent state allows
  that candidate's same-term request. Uses standard `propext`, `Quot.sound`.
- `restart_strands_pledged_initializer`: the candidate's first HELLO leaves
  `pledge=1`, `prop=1`, `nextId=2`. Restart clears `prop`; both its HELLO guard
  and initialize guard then fail. Uses standard `propext`.
- `restore_proposal_enables_founding`: reloading any nonzero reserved ID
  enables both local founding guards, regardless of `nextId`. This is a
  local recovery lemma, not a global liveness or safety proof. Uses standard
  `propext`.

The third result concerns the unlanded pledge proposal. Recover the
proposal ID from the durable pledge, or deliberately require intervention;
do not describe the current candidate as automatically crash-recoverable.
It may still receive replication from another member with the reserved ID;
the lemma concerns its own founding actions, not universal deadlock.

The full pinned Lean v4.35.0 / Mathlib build passes (1079 jobs). `#print
axioms` confirms the listed dependencies; there are no admitted proofs.

## Reproduction and remaining work

```sh
cd formal/tla
TLC_WORKERS=8 ./check_founding.sh
cd ../lean
"$HOME/.elan/bin/lake" build
```

TLC jar SHA-256 is unchanged from the handoff:
`5bd7b64db99ae4c6c35f290de5e809c0ee41af43f8699cf696099ffb6452b1e2`.
`run.sh` retains the machine-wide advisory lock and state-directory cleanup;
the Linux VM uses `flock` rather than the old hardcoded BSD `lockf` path.
The focused runner completed all nine checks with their expected results.

Remaining: complete the larger per-message and identical-seed connection
pledge checks,
test later terms for the external claim, and model a recovery mechanism
for the pledge's lost volatile proposal.
