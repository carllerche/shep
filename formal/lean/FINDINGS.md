# Findings from the Lean formalization of Shep's Z-set calculus

Theorem names refer to files in `ShepZSet/`. The project builds with no
`sorry`, and `#print axioms` on the headline theorems shows only `propext`,
`Classical.choice`, and `Quot.sound`.

**Scope change.** The original property 6, list element IDs and fractional
order keys, was dropped when the design made ordered lists atomic leaf values
(§10.2). Property 6 now covers the nested-structure model: field IDs and map
keys as path segments, set members as facts, and lists as atomic values.

Findings are ordered by importance. The design changes they led to are folded
into `docs/design.md` (§9.5, §10.1, §10.2, §10.4, §11.1, §11.5, §12).

## 1. A causally closed version's state is valid only under three conditions

Commutativity (`applyList_perm`, `applyList_eq_stateAt`) shows that the final
state doesn't depend on application order. Validity is a separate question: a
state is valid when it is a set, has one value per field, and is tree-shaped.

What the proofs establish:

- `noncausal_intermediate_not_set`: applying a delete before its create gives
  weight −1, so causal order matters for intermediate states.
- `causalOrder_prefix_closed`: every prefix of a causal order is causally
  closed.
- `causallyClosed_union`: merging tokens preserves causal closure.
- `wf_stateAt_closed`: every causally closed version's state is well-formed,
  given these three conditions:
  1. Causality is a strict order: `d ∉ ctx d`, and every context is causally
     closed.
  2. Each delta is a valid transition from the state at its own context.
     §9.5's serializability argument provides this, but only if every fact a
     delta writes is owned by that delta's PG node.
  3. Concurrent dots never touch prefix-comparable paths of the same node.
     Row conflicts provide this for node facts. They don't provide it for
     edge facts stored as their own rows, which need a unique key on
     `(edge type, src, dst)` or an equivalent row conflict.
- `concurrent_same_field_not_set`: condition 3 can't be dropped.

## 2. Widening is exact with additive column patching; three details need stating

`widen_correct` holds for any `S` and `Δ`: `widen S Δ = proj(S+Δ) − proj S`.
Related results:

- `widen_window`
- `widen_window_filtered`: the filtered window isn't the state at V1, but it
  agrees with it on every projected path.
- `widen_telescope`
- `widen_linear_query`
- `widen_join`

An implementation that stores Option-valued columns and takes the `+` value is
exact only when both states are valid (`leaf_patch`). So a delta must carry
`−old` along with `+new`, and the old state must be causally closed.

The three details:

1. **Skipping by footprint** (`footprint_skip_sound`). An entry can be skipped
   only when none of its footprint prefixes is comparable with a projected
   prefix: equal to it, above it, or below it.
2. **What must be projected** (`proj_skip`). `@deleted_at`, and every path the
   read policies read, must be projected. Otherwise deletes and policy changes
   are silently skipped.
3. **Consolidating over the window.** Sum `ΔI` over the whole window and widen
   once. Widening entry by entry exposes intermediate rows.

## 3. Projection and union are linear only with bag semantics

What the proofs establish:

- `zmap_linear`, `plus_linear`: projection and union are linear.
- `zmap_not_preserves_set`, `isSet_not_closed_add`: projection and union can
  produce weight 2, so results stop being sets.
- `distinct_not_linear`.
- `distinct_delta`, `distinctH_eq`, `distinctInc_support`: incremental
  `distinct` needs the old integrated weight, but only at the keys in `Δ`'s
  support.
- `zjoin_isSet`: a join of two sets is a set.

Change results are sets, so collapsing projections and `UNION` must end in
`distinct`.

## 4. Empty containers can't be represented as leaf facts (a correctness bug)

`encodeDoc_empty_containers`: `{}`, `{a: {}}`, and an absent field all encode
to no facts, so replay can't distinguish `Some(empty)` from `None`. The fix is
a presence fact `(node, path ++ [@], ())` for every container that exists.

## 5. Nested deltas preserve well-formedness, each under a precondition

| Operation | Theorem | Precondition |
|---|---|---|
| set a scalar or list leaf (`−old +new`) | `wf_set_leaf` | `old` is the live value |
| set an absent leaf, or add a map entry with a leaf value | `wf_add_leaf` | nothing live at, above, or below the path |
| create a node, or add a map entry with a sub-document | `wf_add_fresh` | the new facts are well-formed, and their paths are unrelated to the node's live paths |
| add a set member | `wf_add_member` | not already a member, and no live leaf strictly above or below |
| remove a set member, or unset a leaf | `wf_remove_member`, `wf_remove_fact` | the fact is live |
| remove a subtree or a map entry | `wf_remove_subtree`, `wf_remove_map_entry`, `removeSubtreeDelta_apply` | none |
| change a `Document` value's shape | `wf_replace_subtree_with_leaf` | none beyond those of the two steps |

The resulting rule: operations are diffed against the snapshot, and idempotent
operations log nothing.

## 6. The change-query algebra holds as stated

- `changes_chain`, `changes_apply`, `holder_invariant`
- `state_sub_of_subset`
- `state_diff_general`: without `V1 ⊆ V2`, the input delta is
  `Σ(V2∖V1) − Σ(V1∖V2)`.

## 7. The DBSP formulas hold as stated

- `linear_delta`
- `join_delta`
- `bilinear_delta_new`: `ΔA⋈B₂ + A₂⋈ΔB − ΔA⋈ΔB`, which avoids materializing
  `A₁`.
- The stream lemmas: `diff_integ`, `integ_diff`, `incr_linear`, `incr_comp`,
  `integ_incr`.

## 8. Order independence and replay hold as stated

`applyList_perm` and `applyList_eq_stateAt`. Finding 1 explains why causal
order still matters.

## 9. Edge facts need a key; edge sort keys need a gap invariant (not machine-checked)

- **Edge key.** At most one live fact per `(edge type, src, dst)`. Changing a
  sort key is `−old`, `+new`.
- **Generated sort keys.** If Shep ever generates a sort key between two
  neighbors, no lexicographic digit string lies strictly between `s` and `s`
  followed by the minimum digit. Keys must never end in the minimum digit, and
  equal keys leave no room at all. This was argued by hand only.

## 10. Not proven

- the Postgres guarantees behind conditions 2 and 3 of finding 1, which are
  hypotheses here
- DVV encoding, footprint encoding, the log format, and policy semantics
- fractional order keys (descoped)

## Aggregates and recursion

The proofs for this section are in `ShepZSet/Aggregates.lean` and
`ShepZSet/Recursion.lean`. Recursion is modeled with semi-naive evaluation
over a linear step, not with DBSP's nested-stream construction.

### A1. Recursive queries break the §11.5 cost bound

What the proofs establish:

- **The baseline is correct.**
  - `seminaive_correct`, `seminaive_terminates`, `lfp_fixed`, `lfp_least`:
    semi-naive evaluation reaches the least fixed point.
  - `mem_reach`: for nested groups, that fixed point is the transitive
    closure of the membership edges.
- **Insertions** (`incr_insert`, `tc_insert`, `tc_insert_seed`, `lfp_mono`):
  continuing semi-naive evaluation from the old fixed point reaches the new
  one.
- **Naive deletion is wrong** (`naive_delete_wrong`). Delete `0→2` from the
  edges `{0→1, 1→2, 0→2}`: naive deletion removes `(0,2)`, which is still
  reachable through `1`.
- **DRed is correct** (`dred_sound`, `dred_survivors_prefixed`,
  `dred_correct`).

The cost is sensitive to how much output and how many derivations change,
not to the size of the change. So recursion is materialized (§12) or
depth-bounded. The design changes are in §11.5 and §6.8.

### A2. Recursion through a deny rule has no fixed point

`nonmonotone_no_fixpoint`. A recursive `delegate` may appear only in allow
rules, and the checker enforces this through stratification (§6.8). The
monotonicity argument is on paper; only the counterexample is
machine-checked.

### A3. Recursive relations must be sets (not machine-checked)

Cyclic memberships give infinitely many derivations, so a recursive relation
needs `distinct` inside the loop.

### A4. Grouped aggregates hold as stated, with one catch

What the proofs establish:

- **Linear forms.** `groupAgg_add`, `groupAgg_delta`, `groupCount_eq_zmap`.
- **The rows form.**
  - `aggRows_delta`, `groupRows_delta`, `countRows_delta`, `sumRows_delta`:
    the incremental formula.
  - `groupAgg_support`, `aggRows_delta_local`: the cost stays within the
    touched groups.
  - `aggRow_appear`, `aggRow_disappear`, `aggRow_change`: a group appearing,
    disappearing, or changing.
  - `countRows_not_linear`: the rows form isn't linear.
- **The catch.** A group's presence comes from its `COUNT`, not its `SUM`, so
  `SUM` and `AVG` are maintained as a `(COUNT, SUM)` pair.

### A5. `MIN` and `MAX` aren't incremental under deletions

What the proofs establish:

- `max_not_incremental`: `{3,5}` and `{1,5}` both have max 5, but deleting 5
  leaves different answers.
- `groupMax_restrict`, `groupMax_untouched`: a group's max depends only on
  that group's rows.
- `groupMax_insert`: insertions alone are incremental.

Deletions need the group's rows at V1, through an index (§11.5).

### Still not proven

- that depth-bounded unrolling equals the bounded fixed point
- non-linear recursion, where the step joins the recursive relation with
  itself

## Adapters

The proofs for this section are in four files: `ShepZSet/Adapters.lean`,
`AdapterLibrary.lean`, `AdapterCounterexamples.lean`, and
`AdapterSystem.lean`.

**The model.**

- A row maps field IDs to values, and a patch maps field IDs to optional
  values.
- A lens has two parts:
  - `get : Row → Option Row`, where `none` means `hide`;
  - `put : Patch → Row → Option Patch`, where `none` means `reject`.
- `A ⨾ B` places `A` on the view side and `B` on the storage side.

### AD1. Laws 4 and 5 don't survive a lossy write below a cross-field step

What the proofs establish:

- **What always survives composition.** `comp_totalRead`,
  `comp_totalWrite`, `comp_locality`, `comp_noHiddenWrite`, and
  `chain_weakLaws`.
- **When every lower step writes exactly, every law survives.** `comp_laws`
  and `chain_laws`.
- **Same-field chains of built-ins stay lawful.** `fieldLaws_comp` and
  `fieldLens_comp_laws`.
- **The counterexample.** The view is `total = sub + tax`, above a clamp
  that writes `sub` lossily. Each step satisfies every law on its own
  (`A1_laws`, `A2_laws`, `B3_laws`), but the chain doesn't:
  - `chain_putGet_no_norm`: the same write reads back differently on two
    rows that look identical to v1.
  - `chain_putPut_fails`, `chain_not_lawful`: applying two writes in turn
    differs from applying their merge.
- **What registration must check** (`registration_sufficient`):
  - **Exact new step.** Checking its laws plus the backfill conditions is
    enough.
  - **Lossy new step.** Laws 4 and 5 must be re-proven for chains with a
    cross-field step above it, or the step must use `reject` instead.

### AD2. `add` for edge writes is unlawful

`firstAdd_putGet_fails` and `firstAdd_putPut_fails` show `add` breaks the
laws. `replace` is lawful (`firstEdge_fieldLaws`), and its representable set
is "at most one edge" (`firstEdge_rep`).

### AD3. The name split is lossy and has a restricted representable set

What the proofs establish:

- `split_getPut_fails`: GetPut fails when `first` contains a space.
- `split_putGet_fails`: PutGet fails when a name's first space is its last
  character.
- `split_plainJoin_fails`: PutGet fails for `"cher"` under the plain join,
  so the join has to handle an empty `last`.

### AD4. `default(...)` on a read is lossy

`defaultRead_rep`: a missing value isn't representable.

### AD5. The composed representable set is per field

What the proofs establish:

- `rep_comp_iff`: the set is exact when the storage-side step writes
  exactly.
- `rep_comp_of`: the per-field formula is always sound.
- `rep_comp_strict`: when the storage-side step normalizes, the formula is
  a strict under-approximation.
- `rep_union`, `rep_of_each`: representability can be checked field by
  field.

The whole-row formula from §4.4 was too coarse.

### AD6. The write guard is exactly "the writer could undo this"

What the proofs establish:

- `guard_undo`: a write that passes the guard can be undone by its writer.
- `undo_implies_rep`: with exact writes, a write its writer can undo was
  representable.
- `no_silent_loss`: an accepted write changes only its footprint, reads
  back normalized, leaves untouched fields alone, and can be undone.
- `rep_after_write`: with exact writes, a version can always edit what it
  wrote.
- `run_valid`: under any interleaving of versions, storage stays in
  `S(head)`.
- `read_valid`: every read is valid for its version, or is a declared
  `hide`.

### AD7. Law 4's normalization must be per field

`write_readback`: the law holds only with a per-field `norm`. A whole-view
`norm` could change fields the write never touched.

### AD8. Left identity requires rejecting writes to hidden rows

`comp_id_left_iff`: left identity holds exactly when writes to hidden rows
are rejected. `comp_id_right` and `comp_assoc` hold unconditionally.

### AD9. Moving the head forward

What the proofs establish:

- `chain_snoc`: appending a new head step extends every chain.
- `head_forward_read`, `head_forward_read_lossless`: after a head change,
  older versions read the old row, or its declared lossy image.
- `head_forward_many`: by induction, that holds across any number of head
  changes.
- `truncBackfill_spec`: the `truncate` backfill satisfies these conditions.

The backfill obligations are that every backfilled row is valid for the new
head, and that `get(b(x))` equals the declared image.

### AD10. A write through any lens is a valid fact transition

`rowFacts_patch`, `patchDelta_old_live`, `wf_write_transition`. This
discharges condition 2 of `wf_stateAt_closed` for writes from every version.
Only single-valued leaves were modelled.

### AD11. Built-in instances and fusion

Each built-in is proven lawful, with an exact representable set:

| Adapter | Theorems | Representable set |
|---|---|---|
| rename (identity) | `id_laws` | every row |
| `truncate(n)` read | `truncRead_*` | length ≤ n |
| `clamp` / `saturate` | `clampRead_*` | values in range |
| `map(a → b)` | `enumMap_*` | values other than `a` |
| `first` with `replace` | `firstEdge_*` | at most one edge |
| `default` read | `defaultRead_*` | value present |
| tightening `truncate(m)` write | `truncWrite_*`, `truncReject_fieldLaws` | every valid row |

Fusion: `truncRead_fuse`, `truncRead_fuse_comm`, and `truncRead_200_500`.

### Not proven

- read-footprint locality for change queries: an old version's projection
  must include its lens's read footprint
- `redact`
- hoist/plunge and wrap/unwrap
- custom CEL adapters checked by SMT
- composition across different fields
- a general condition under which cross-field steps can sit above lossy
  writes

## Freshness, placeholders, walks, search privacy, viewer mappings

Files: `ShepZSet/Freshness.lean`, `Placeholders.lean`, `Walks.lean`, `SearchPrivacy.lean`,
`ViewerMapping.lean`. Every theorem uses only `propext`, `Classical.choice`, and `Quot.sound`.

- **Freshness.**
  - The check is sound given a sound read set (`fresh_sound`, `freshness_sound`).
  - The concrete scan model satisfies the read-set condition (`Scan.eval_readSetSound`).
  - Compaction is safe only if it is covering (`compaction_refines`, `compaction_sound`).
  - Counterexamples:
    - Checking only returned rows is unsound (`returnedOnly_unsound`).
    - Skipping ghosts is unsound (`noGhost_unsound`).
    - Compacting to the max counter is unsound under concurrent dots on one node (`counterMax_compaction_unsound`). With per-partition prefixes, compare against `V`'s prefix.
- **Placeholders.**
  - Every query, for every viewer, treats a placeholder exactly as an absent key (`query_placeholder_eq_absent`).
  - A fill's change equals a fresh create's (`fill_changes_eq_create`).
  - Creating over equal data is a no-op; over different data it is a conflict (`create_equal`, `create_conflict`).
- **Walks.**
  - A walk equals a walk over the visible subgraph, with the same depths (`walk_restrict`, `walk_congr_visible`).
  - A budget that counts hidden nodes leaks them through `truncated` (`budget_counting_hidden_leaks`). Count visible work only (`visibleWork_congr`).
- **Search privacy.**
  - Hidden-field content changes no row's score or match (`score_invariant`, `ranking_invariant`), provided field policies don't read hideable fields.
  - `fieldPolicy_reading_hidden_leaks` shows that this condition is needed.
  - IDF leaks hidden content into ordering (`idf_outside_model`).
- **Viewer mappings.**
  - `viewer_of` is single-valued for well-formed mappings (`viewerOf_unique`).
  - A deleted actor can do nothing (`canOp_deleted_actor`).
  - The explicit deleted-actor check is load-bearing (`precheck_needed`).

## Storage on dotted version vectors

Files: `ShepZSet/Stamps.lean`, `Validation.lean`, `Closure.lean`, `Backfill.lean`,
`Lineage.lean`. Every theorem uses only `propext`, `Classical.choice`, and `Quot.sound`.

### S1. Stamp visibility generalizes `wf_stateAt_closed`; 2PC is atomic only with the decision dot

- **Visibility.** The state at `V` is the sum of the parts whose stamp is in `V`, and every causally closed `V` is valid (`stateAtS_eq_stateAt`, `wf_stateAtS_closed`).
- **Valid transitions.** A 2PC's merged delta is a valid transition when each part is valid from the decision context's state and the parts touch disjoint nodes (`wf_add_parts`).
- **Atomicity.** Every 2PC is wholly in or wholly out of every version, and a closed version that holds a 2PC holds every one of its prepares (`twoPC_atomic`, `twoPC_state`, `twoPC_closed_prepares`).
- **Counterexample.** Stamping each part with its own release dot breaks atomicity (`localStamp_not_atomic`).

### S2. Validation makes each partition's log serializable in log order

- **Serializability.** Each entry's reads at its read version equal its reads at its log position (`serializable`, `reads_eq_serial`). Old values are true values (`consistent_all`). Validation gives condition 3 and `Fresh.History.WF` (`validated_writes_ordered`, `toHistory_wf`).
- **Hypotheses.** The check covers every earlier entry, committed or not. Written keys are read. Old values are read at `S`. `S` names no later entry. `S ⊆` context, and `S` is closed.
- **Counterexample.** With pipelining, checking only committed entries loses an update (`committedOnly_unsound`).
- **Not modelled.** 2PC intents and late releases (the "no later entry" hypothesis stands in for them), and Raft log matching.

### S3. The closure is the least closed superset; §9.7's formula needs extras' contexts

- **Closure.** It terminates, is closed, is the least such superset, and is monotone (`closure_terminates`, `closure_closed`, `closure_least`, `closure_mono`).
- **Exact rounds.** One `cf(p, prefix)` lookup per partition, plus each extra's context, is an exact round (`stepDVV_eq`).
- **The literal formula.** It is exact only from a covered start, where the extras' contexts are already present (`iterate_stepPre`). Otherwise it stops at a version that isn't closed (`stepPre_unsound`).
- **Frontier.** It is the largest closed subset of the applied entries, and it only advances (`frontier_closed`, `frontier_greatest`, `frontier_mono`).
- **Design changes.**
  - Each round must add every extra's context.
  - Contexts must be causally closed. §9.5's "a context isn't transitively closed" contradicts this.

### S4. Backfill is exact; ghost trust must test the barrier, not the creation dot

- **Backfill.** For a linear index, backfill at the barrier plus per-entry maintenance is exact at every later version (`backfill_correct`).
- **Freshness checks.** They are sound for any `V` that covers the partition's log up to the index's barrier (`backfilled_check_sound`).
- **Counterexample.** A check falsely answers "unchanged" for a causally closed `V` that contains the creation dot but lacks a pre-barrier move (`ghosts_incomplete`).
- **Not proven.** Composite (multi-field) indexes.

### S5. Physical versions must sort by (generation, position) across splits

- **Result.** With that ordering, the newest version stamped in `V` is the causally latest write in `V`, for every `V`, through any number of splits (`winner_correct`, `winner_iff_causalLast`, `winner_stamp`).
- **Counterexample.** Local position alone returns a parent's stale version (`localPos_unsound`).
- **Rules.**
  - Generations increase from parent to child; partition IDs do.
  - One partition writes a key at a time.
  - A split entry's causal past holds everything its partition applied, including the decision dots of released 2PC parts.
  - A child's writes follow the split entry.

### S6. Token membership across splits

- **Lineage rule.** "`V` names a descendant and `j ≤ i`" only restates causality (`memL_consistent`).
- **Folding.** Membership and folding are monotone, and folding loses nothing (`memL_mono`, `fold_mono`, `fold_inflationary`).
- **When folding is exact.** Only when the parent's prefix reaches its split entry (`fold_exact`). Otherwise it adds dots (`fold_grows`). So fold only causal (`at_least`) tokens, never `since` versions.

## Consistency planner

Files: `ShepZSet/Planner.lean`, `LeaderOps.lean`, `Polarity.lean`, and `Disciplines.lean`.
Every theorem uses only `propext`, `Classical.choice`, and `Quot.sound`.

### P1. Skipping a Stable guard preserves outcome-serializability

**Result** (`outcome_serializable`, which extends `serializable`). Take a mutation whose
`require p` is Stable and whose `p` reads are left unvalidated. If every log entry passes the
discipline check, that mutation has the same writes and `require` outcome at its log position
as at `S`.

**Why it holds.**
- Every entry respects `≤_D`: frozen facts of created nodes stay equal, and permanent
  lifecycles only grow (`respects_mono`).
- So the state at `S` is `≤_D` the state at the entry's log position (`readAt_le_serial`).
- Stable⁺ guards are upward-closed (`stab_upClosed`). This relies on the implicit lifecycle
  read: reading a field of a node that was never created is an error.
- `!exists` is never Stable⁺, because no lifecycle is frozen.

**Counterexample** (`mutableLifecycle_unsound`). If the guard reads a mutable lifecycle, it
can be true at `S` and false at the log position.

### P2. The context rule gives every condition of `wf_stateAt_closed`

**Leader resolution.** A field op is resolved against the newest write not in `S`, or else
against the base value from `S`. That equals the value at the log position and at the context
(`lv_eq_logpos`, `lv_eq_ctx`). When `S` is at or above the window floor, the in-window search
is complete (`window_complete`).

**Contexts.** These properties hold whether the context names `prevₖ` or uses the self-prefix:
- contexts are strictly earlier (`ctx_lt`)
- contexts are causally closed (`ctx_closed`, `selfPrefix_closed`)
- writes that conflict are ordered (`ctx_ordered`)
- concurrent dots write disjoint paths (`ctx_pathDisjoint`)

From these, `leader_wf` follows. It proves condition 2 rather than assuming it.

**Bounds** (`bounded_leader`). A bounded op stays within its bound under pipelining and loses
no update.

**Prefix-related keys.** When written keys can be strictly prefix-related, the rule must also
consult each conflicting key's last writer.

**Counterexamples.**
- `ctxOmitPrev_not_set`: the context omits `prevₖ`.
- `unrecordedRemoval_not_set`: a removal is not recorded as a `{key, absent}` record.
- `committedResolve_crosses_bound`: the op is resolved against committed state only.

### P3. Polarity never over-grants, with two changes

**Result.** If a decision isn't RevocationSensitive, admitting at `V` implies admitting at
every `V' ⊇ V` (`vis_mono`, `history_dle`, `history_admit_mono`).

**Change 1.** A delegation inside a Deny is always sensitive (`denyDelegate_overgrants`).

**Change 2.** Under errors-as-Deny, CEL's `false && error = false` lets an earlier Allow move
from Skip to Deny (`allowError_overgrants`). The fix is either of:
- every Allow guard is error-free or unchanging, or
- errors in Allow guards count as Skip.

Both fixes are proved.

**Other counterexamples.** `denyMutable_overgrants` and `ignoreVisible_overgrants`.

### P4. Disciplines are preserved, given a stronger frozen-field rule

**Result.**
- Frozen facts keep their creation value (`frozen_eq_creation`).
- Permanent lifecycles never retract (`present_mono`).
- At most one dot writes `@type` (`ty_unique`).

**Rules this needs.**
- A frozen field is written, with `+` or `−`, only by the entry that creates or fills its node.
- `@type` is written only by a create or a fill.
- A permanent node has no `@deleted_at` fact in either direction.

**Counterexample** (`weakFrozen_unsound`). The weaker rule "no `−old`" lets a later entry set
a frozen field that was unset at creation.

### Not proven

- more than one partition
- expanding the self-prefix across partitions
- 2PC intents as `prevₖ`
- the two-step weakening rollout
- `claim`
- a formal link between `Planner.Respects` and `Disc.FrozenOK`/`PermOK`

## ShepQL restatement round (§4.0, §6.1, §8, §18)

The files in this round:
- `ShepZSet/PolicyCombine.lean`
- `Polarity.lean`, restated (the old CEL model is in `PolarityCEL.lean`, superseded)
- `Planner.lean`
- `Totality.lean`
- `Checks.lean`
- `PatchMerge.lean`
- `LeaderOps.lean`, its `FieldOps` section
- `PlaceholdersQL.lean`

Every theorem uses only the axioms `propext`, `Classical.choice` and `Quot.sound`. The per-atom obligation in `Principals.lean` (§18 item 3) is handled separately.

### Q1. Polarity under union minus deny, with every allow Stable

**Result** (`vis_mono`, `history_admit_mono`).
- **Combination model:** guards are two-valued, and a decision is `(∨A) ∧ ¬(∨D)`.
- **Conditions:** the type is permanent; its `contained_in` link is frozen, with a permanent container; every allow is Stable⁺; and every deny is Stable⁻.
- **Then:** a stale read decision never over-grants, whether the viewer is present or absent. The same holds for field decisions (`fieldVis_mono`).
- `ErrMode` and `allowError_overgrants` no longer apply, because ShepQL guards can't error.
- **Counterexamples:** `denyMutable_overgrants`, `allowMutable_overgrants`, `ignoreVisible_overgrants`, `denyDelegate_overgrants`, `fieldDenyMutable_overgrants`.

**Precision** (`port_visible_safe`). Polarity is tracked through `not`, so `not shep::visible(…)` inside a deny is Stable⁻ when the `visible` is Stable⁺.
- §7.6 says "`shep::visible` inside a deny is RevocationSensitive outright". That is sound, but it makes every Ent-ported policy whose earlier allow uses `visible` RevocationSensitive.
- The port actually preserves stability (`port_safe`).

### Q2. Planner guards over `Bool`; a `??` default reads the lifecycle

**Result** (`gstab_sound`, `stab_upClosed`, `stab_neg_closed`).

**Counterexample** (`trueDefault_unsound`). Take `assert((x.f = 1) ?? true)` with `f` readonly.
- At `S`, `x` doesn't exist yet, so the assert is true.
- At the log position after `x` is created with `f = 2`, it is false.

So the default reads the lifecycle of the node on its left side:
- in a Stable⁺ position the default must be `?? false`;
- in a Stable⁻ position it must be `?? true`.

### Q3. Decisions are two-valued

**Result** (`decision_two_valued`, `card_sound`, `total_eval`). A guard with no partial calls whose inferred cardinality is One evaluates to exactly one boolean.

**Finding: principal atoms.**
- Under the per-atom rule, `global viewer in s` is One (`viewerIn_one`).
- Under plain EdgeQL it is AtMostOne (`inViewer_opt`).
- The two readings agree on the atom alone (`viewerIn_eq_coalesce`) but differ under `not` (`viewerIn_not_differs`).

**Finding: placeholders in policies** (`placeholder_hop_not_total`). A hop through a `required` link to a placeholder-capable type is AtMostOne when read in a policy.

### Q4. Field policies are default deny, and never exceed the type

**Result** (`field_default_deny`, `field_le_type`, `field_type_default_deny`).

**Findings.**
- The doc's "a field with allows replaces the type's allows" doesn't match the formula (`field_not_replace`).
- A field's first allow can narrow access (`first_field_allow_narrows`); later allows only widen it.

### Q5. Checks and the SMT option-lifting

**Result** (`enc_agrees`, `check_iff`, `guard_iff`). A check holds iff `¬defined ∨ value`; a guard holds iff `defined ∧ value`.

**Finding** (`ite_naive_disagrees`). `if … else` is defined when its condition is defined and so is the branch it chooses.

### Q6. Patch merging is PutPut

**Result** (`merge_eq_seq`, `mergeAll_perm`). Merging a mutation's patches equals writing them in sequence when every intermediate view is valid.

**Note** (`merge_accepts_invalid_prefix`). The merge is the semantics.

### Q7. Leader field ops equal the evaluator

**Result** (`*_eq`, `resolved_eq_eval_at_logpos`). Each op gives the evaluator's result for every base value, including `{}`.

**Findings.**
- `e` must have cardinality One (`optionalArg_unsets`, `max_optionalArg_unset`).
- Ops of different kinds on one field don't commute (`add_max_not_comm`).

### Q8. Placeholders read as absent in ShepQL

**Result** (`evalQ_placeholder_eq_absent`, `typeFact_eq`). This holds in both read contexts, for key casts, hops, `.__type__`, field reads, `exists` and `shep::visible`. The premise is necessary (`lifecycleExists_distinguishes`).

### Q9. The Ent port is exact

**Result** (`port_exact`, `port_denies_first`).
- Ordered first-match with default deny equals union minus deny once each deny is rewritten as `D'_j = D_j ∧ ¬(∨_{i<j} A_i)`.
- Ent's own default needs a final `allow true` (`port_entDefault`).

### Not proven

- function inlining, and `shep::walk`'s `stop_at`, in the totality model
- guards that read other nodes' fields, in `Polarity.lean`
- the per-atom rule over `UMD`, in `Principals.lean`
- int64 overflow in leader ops

## Principals (`Principals.lean`)

Every theorem uses only `propext`, `Classical.choice` and `Quot.sound`.

- **Pre-checks.** A request is denied on every node, under every policy and app, when its actor or viewer is deleted, or its viewer is of another type (`canOp_deleted_actor`, `canOp_deleted_viewer`, `canOp_wrongtype_viewer`). Declared actors are enforced (`actorsDecl_enforced`).
- **Absent principals, per atom.**
  - With no viewer, a guard equals the same guard with its viewer atoms set to false, under `not`, `and` and `or`; the same holds for the actor (`eval_absent_viewer`, `eval_absent_actor`).
  - This agrees with applying `?? false` to the whole guard on a bare atom, and whenever both principals are present (`viewerAtom_eq_coalesce`, `evalSet_present`). The two differ under `not` (`viewerAtom_not_differs`, `actorAtom_not_differs`).
  - Consequence: `allow (not (global viewer in .blocked))` admits the app acting as itself (`negated_viewer_allow_admits_asApp`).
- **App-only access.** `app = slack and not exists global viewer` admits exactly the app acting as itself (`appOnly_allow_exact`). Without the second atom, an agent acting for a user also gets the grant (`appAllow_without_noviewer_admits_viewer`).
- **`shep::visible` keeps the viewer** (`visible_preserves_viewer`). Into a schema with another viewer type, it denies (`visible_crosstype_denies`).
- **Act-for never amplifies** (`canOp_actfor_no_amplification`). It holds under `ActForOK`: at every node reachable through `visible`,
  - the user passes the actors declaration;
  - actor atoms only move toward the user;
  - `visible` is positive in allows and negative in denies.

  `actForOK_of_actorless` gives a syntactic sufficient condition. With no actors declared, the agent can do exactly what the user can (`canOp_actfor_eq_undeclared`).
- **Counterexamples to act-for.** Each of these amplifies:
  - an allow on the actor (`actorAllow_amplifies`);
  - a deny on the viewer's own type (`actorDeny_on_self_amplifies`);
  - a declaration that leaves out the viewer type (`actorsDecl_excluding_viewer_amplifies`);
  - `visible` in a deny whose target reads the actor (`visibleDeny_amplifies`).

  The checker's flag, "allows on the actor are flagged", must cover all four.
- **Superseded:** the first-match restatement, and the two-party key issuance model, since Shep no longer issues credentials.

**Doc fixes these findings call for.** The design doc is frozen by the user, so they are not applied.
- §6.3, "Actors are opt-in", and §6.8: list the four conditions above, and state both guarantees: equality when no actors are declared, and "no more than the user" under `ActForOK`.
- §6.1, "Absent principals, per atom": give the `not (global viewer in .blocked)` example. The checker should flag an allow that can hold with no viewer unless it mentions `exists global viewer`.
