# Findings from the consistency planner model

`Planner.tla` checks the consistency planner (§7.6), discipline enforcement (§4.1.2), leader-resolved field ops (§8, §10.3) and how reads are served under the authorization horizon (§6.3).

## What the model covers

- **Partitions.** There are two, each with one leader and no failover. Appends are pipelined.
- **Reads.** A version is a vector of committed prefixes and is causally closed. A read may be served at any such version, so readers can lag without bound.
- **Schema.**
  - `Commit c` lives in P1. It is `#[permanent]`, and its owner is `#[frozen]`.
  - `Item i` lives in P2. It has a mutable lifecycle, a frozen owner, an op-eligible `stock` constrained by `self >= 0`, and a mutable `watchers` edge.
- **Discipline enforcement.** Front ends enforce disciplines under their registry, and the leader's registry barrier check gates their proposals.
- **Leader ops.**
  - The leader keeps op records, value records, `{key, absent}` records for removals, and a last-value map.
  - An op resolves against the newest write not in S; if there is none, against its `base_value`.
  - The result is `too_old` iff S is below the floor.
  - Each op includes a lifecycle read.
  - The context includes the previous write to the key, or uses the self-prefix flag.

## Invariants

- **OutcomeSerializable:** a serial order consistent with both logs reproduces every committed mutation's guard outcome, bound outcome and writes.
- **FreshFalse:** a `require` reported false is false at a fresh version.
- **CauseVisible:** a closed version that contains an entry also contains what it depended on.
- **DeltaOldExact:** an op's recorded old value is exact.
- **CtxRule:** contexts follow the context rule, in its closure form.
- **KeyOrder:** writes to one key are causally ordered.
- **BoundNeverCrossed:** replaying the ops, rather than the recorded values, never crosses a bound.
- **CompiledAsValidated:** anything compiled as validated is validated.
- **DisciplinesHold:** no frozen fact or permanent lifecycle is ever changed.
- **RelyNeedsBarrier:** a plan relies on a discipline only once the enforcement barrier is in its version.
- **WeakenAfterAck:** a weaker discipline takes effect only after every partition has committed the barrier.
- **ReadHorizon:** a read never over-grants.

## Results

| Config | Expected | Distinct states | TLC time | What it checks |
|---|---|---|---|---|
| Planner_guards | pass | 32,075 | 1s | A Stable guard skipped across partitions, plus the retry after a stale false; a guard over a mutable lifecycle is validated; delete, unlink; reads |
| Planner_guards_deep | pass | 501,102 | 10s | MaxExec 3, with floors advancing |
| Planner_ops | pass | 314,139 | 6s | Pipelined decrements; an op on a field another consumer reads is compiled as validated; removal; delete |
| Planner_ops_deep | pass | 1,140,423 | 22s | MaxExec 3 |
| Planner_weaken | pass | 3,130 | 1s | The two-step weakening against the C2 race |
| Planner_weaken_deep | pass | 5,881 | <1s | MaxExec 3 |
| Planner_strengthen | pass | 82,028 | 2s | The rely rule and the barrier check |
| Planner_strengthen_deep | pass | 232,920 | 5s | MaxExec 3 |
| Planner | pass | 378,792 | 9s | Combined: accept, purge and a decrement under a weakening |
| Planner_xpart | pass | 1,074 | <1s | The self-prefix flag expanded across partitions |
| Planner_pipeline | fail | 20 | <1s | BoundNeverCrossed: an op resolved against `base_value` only |
| Planner_noremoval | fail | 28 | <1s | BoundNeverCrossed: a removal missing from the last-value map |
| Planner_c2 | fail | 3,569 | <1s | OutcomeSerializable: a weakening in force before every partition acknowledged it (review C2) |
| Planner_novisible | fail | 4 | <1s | ReadHorizon: the RevocationSensitive classification ignores `visible()` |
| Planner_mutlife | fail | 22 | <1s | OutcomeSerializable: a guard skipped over a mutable lifecycle |
| Planner_opread | fail | 30 | <1s | OutcomeSerializable: an op on a field another consumer reads, left unvalidated |
| Planner_stalereg | fail | 69 | <1s | DisciplinesHold: a stale registry and no barrier check let a frozen field be written |
| Planner_nolife | fail | 22 | <1s | OutcomeSerializable: an op without a lifecycle read |
| Planner_norely / _norely_ser | fail | 210 / 1,661 | <1s | ReadHorizon / OutcomeSerializable: a plan relies on a discipline without its enforcement barrier |
| Planner_noprev | fail | 22 | <1s | KeyOrder: a context without the previous write |
| Planner_staleretry | fail | 9 | <1s | FreshFalse: the retry version S′ taken from a materializer |
| Planner_guardonly | fail | 13 | <1s | CauseVisible: S′ not joined into the context |
| Planner_reach_* (11 configs) | reached | 6–3,815 | <1s | Reachability: a skip, a retry, a purge, a validated accept, a reassign, the self-prefix flag (including across partitions), an `invalid` rejection, and resolution inside the window all happen |

## Findings

| # | Finding | Shown by |
|---|---|---|
| P1 | The two-step weakening is necessary and sufficient. A weaker discipline may take effect only after every partition has **committed** the barrier: an appended barrier can be lost on a leader change. With front-end enforcement, step 2 becomes a second registry change with its own barrier. | `_c2` vs `_weaken` |
| P2 | The rely rule must also cover reads served without the horizon. A read served at a lagging version that lacks the partition's enforcement barrier over-grants. | `_norely` |
| P3 | The barrier check also stops proposals that went stale in flight: the front end's registry was current when it executed, but the barrier reached the leader first. | `_stalereg` |
| P4 | Leader ops are safe with resolution inside the window, removal records and the lifecycle read. Each of the three is necessary. | `_pipeline`, `_noremoval`, `_nolife` |
| P5 | With the self-prefix flag, the context contains the previous write's context only through closure, including across partitions. The literal set formula doesn't hold. | `_xpart` |
| P6 | Barriers don't fence read plans, because reads make no proposals. Read plans are correct only because the registry is inside the actor part of the horizon. This is argued, not modelled: the model's reads use the current registry. | (argued) |
| P7 | Open question: a soft delete keeps field values. If a delete records `{key, absent}`, a restore must record each value again; otherwise later ops on those fields fail with `invalid`. | not modelled |

## Doc text the findings call for

The design doc is frozen at v54 by the user's instruction, so these fixes are recorded here and not applied.

1. **§7.6, "Changing what plans rely on", step 2.** Change it to: "Only after every one of those partitions has *committed* that barrier does the registry commit the weaker discipline itself, as a second change with its own barrier to the partitions whose facts it lets change. Front ends let, for example, a delete of a formerly `shep::Permanent` type through only under a registry that contains that second change."
2. **§7.6, the rely rule.** Extend it to: "…for a read decision served without the horizon, the version it's served at must contain that barrier."
3. **§9.5, after the context formula.** Add: "With the self-prefix flag, `ctx(e)` is `S ∪ {p: i−1}`. That contains `prevₖ` directly, and `ctx(prevₖ)` only once the version is closed, not as a literal subset." In §18, the cross-partition expansion of the flag is now checked by TLC within bounds.
4. **§4.1.2.** Optionally add: "…including a front end whose registry was current when it executed but whose proposal reaches the leader after the barrier."
5. **§7.6, read plans.** Add: "A read decision is classified under the registry at its `S`, which the actor part of the horizon keeps within `B`; so after a weakening, a read planned under the old registry can over-grant a deleted formerly-permanent node for at most `B`."
6. **§8.** Add the restore question from P7.

## Not modelled

- failover
- versions with extras (S is prefix-only)
- real 2PC, and a previous write that is a prepared intent
- time and `B`
- policy deploys and schema heads
- deny, `contained_in` and delegate polarity (Lean covers these)
- `max`, push and extend, and keys that are prefixes of each other
- restore, placeholders, and frozen `Many` edges
- write policies, `claim` and splits
- a combined config with five or more requests, which passed 50M states without finishing
