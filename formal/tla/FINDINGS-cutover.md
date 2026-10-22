# Findings from the schema cutover model

## The model

`Cutover.tla` models the four-state cutover in design §4.2: Prepared → DualWrite → Active, or
Abandoned. It also covers:

- adapters and backfill (§4.3–§4.4)
- registry barriers (§10.3)
- splits, with barrier re-delivery and cursor remap (§9.6)
- freshness checks and live topics that count registry dots (§11.7, §13.2)
- test bases (§14.2)

**Schema.** One field, with a tightening step whose write adapter is lossy:

- values range over 0..2
- the old head accepts 0..2; the new head accepts 0..1
- `put` truncates
- the new head adds a local `#[unique]`

**Data.** Three rows across two partitions. One partition can split.

**Writers.** Front ends whose registry caches are stale by any amount. Each write is checked
against the leader's barrier.

**Driver.** The meta leader. It runs registration, the barriers, backfill with a cursor per
partition, the activation guard, abandon, and the next version.

**Versions.** Lineage prefixes plus a registry prefix.

**Readers.** Old-version readers are modelled by their lens. `Inv_Lens` checks that every
single-entry step between closed versions that changes an old reader's view is either a stamped
write to that row or a counted registry dot.

`Cutover.cfg` turns on every fix below and passes all 15 invariants (8,353,648 states, 6m21s).
`Cutover_live` passes `Live_Backfill` and `Live_Done` under weak fairness. Configs that fail are
kept failing on purpose.

## Findings

| # | Severity | Finding | Config |
|---|---|---|---|
| CU1 | high | Enforce the new unique from the Prepared (index) barrier on. A one-time check lets a collision arrive after it and deadlocks the cutover past the point of no return | `_dualunique`, `_dualunique_live` |
| CU2 | high | DualWrite writes must enforce the new set-level constraints. A guard at one snapshot can't | `_noenforce`, `_noactunique` (`_noactunique_idx` passes with the CU1 fix) |
| CU3 | high | If the lens switches at the barrier, the abandon dot changes old readers' views and must be counted | `_abandoncount` |
| CU4 | medium-high | Choose the lens by the registry at V, not by each partition's barrier entry | `_lenskey` |
| CU5 | info | Switching the lens at activation is sound and needs no dot counting. Its cost is one stamped change per lossy row | `_lensact` (passes), `_lensact_quiet` |
| CU6 | high | Before Active, a patch must be valid for both heads, or it's rejected. Truncating it through `put` loses data on abandon | `_putlossy` |
| CU7 | high | Backfill reads at a snapshot holding its partition's DualWrite barrier. Skip decisions are validated too | `_bfsnap` |
| CU8 | medium | The activation entry's context is the guard snapshot | `_actctx` |
| CU9 | high | Barriers reach split children before DualWrite is entered, and cursors are remapped | `_noredeliver`, `_cursorlost` |
| CU10 | medium | Bases are never pinned while any version is Prepared or in DualWrite, including with an explicit `--base-version` | `_base` |
| CU11 | high | Backfill writes are validated | `_blindbf` |
| CU12 | high | Abandon isn't allowed after the first backfill write | `_abandonbf` |
| CU13 | low | "Stored data is valid for both heads during the cutover" is wrong before backfill finishes | text |

## Results

| Config | Expected | Result | Distinct states | Time |
|---|---|---|---|---|
| `Cutover` | pass | pass | 8,353,648 | 6m21s |
| `Cutover_live` | pass | pass | 58,681 | 2s |
| `Cutover_lensact` | negative requested | passes (CU5) | 2,181,793 | 1m48s |
| `Cutover_lensact_quiet` | fail | `Inv_BackfillQuiet` | 238,260 | 1s |
| `Cutover_blindbf` | fail | `Inv_NoLostUpdate` | 373,416 | 1s |
| `Cutover_abandonbf` | fail | `Inv_AbandonLossless` | 349,813 | 2s |
| `Cutover_noredeliver` | fail | `Inv_DualWrite` | 316,013 | 1s |
| `Cutover_noactunique` | fail | `Inv_ActivateGuard` | 1,439,344 | 4s |
| `Cutover_noactunique_idx` | pass | pass | 2,181,793 | 35s |
| `Cutover_base` | fail | `Inv_BaseAgree` | 164 | 0s |
| `Cutover_abandoncount` | fail | `Inv_Lens` | 21,419 | 1s |
| `Cutover_dualunique` | fail | `Inv_SetFeasible` | 19,272 | 0s |
| `Cutover_dualunique_live` | fail | `Live_Backfill` | 65,332 | 3s |
| `Cutover_noenforce` | fail | `Inv_Shape` | 1,522,079 | 18s |
| `Cutover_bfsnap` | fail | `Inv_Cursor` | 512,799 | 2s |
| `Cutover_putlossy` | fail | `Inv_AbandonLossless` | 59,655 | 0s |
| `Cutover_actctx` | fail | `Inv_Shape` | 725,580 | 8s |
| `Cutover_lenskey` | fail | `Inv_Lens` | 20,140 | 0s |
| `Cutover_cursorlost` | fail | `Inv_Cursor` | 226,515 | 1s |

## Not modelled

- **Write paths:** policies deployed during DualWrite, the representability guard, multi-step
  and cross-field adapters, and 2PC parts across barriers.
- **Failure and contention:** driver crash recovery (covered only implicitly), and liveness under
  unbounded contention.
- **Topology:** multiple splits and merges.
- **Readers:** front-end read paths are assumed to use the registry at V.
- **Bounds:** 2 user writes, 3 rows, 3 values, 1 split.
