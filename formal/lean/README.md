# Shep Z-set calculus in Lean 4

A machine-checked model of the Z-set calculus that `docs/design.md` relies on: deltas
as Z-sets of facts (§10.1), nested documents (§10.2), versions as sets of dots (§9),
change queries (§11), DBSP incrementalization (§11.5), and materialized queries (§12).
The results, and the design changes they suggest, are in [FINDINGS.md](FINDINGS.md).

There is no `sorry` and there are no new axioms. Every theorem depends only on Lean's
standard axioms (`propext`, `Classical.choice`, `Quot.sound`). You can confirm this with
`#print axioms`, as shown below.

## Building

The toolchain is pinned in `lean-toolchain` (Lean `v4.35.0`), and Mathlib is pinned to
tag `v4.35.0` in `lakefile.toml`.

```sh
# Install elan if you don't have it: https://github.com/leanprover/elan
cd formal/lean
lake exe cache get   # download prebuilt Mathlib oleans (don't build Mathlib from source)
lake build
```

The build prints one advisory warning per Mathlib import in `ShepZSet/Basic.lean`, saying
that Mathlib is "designed for use with the module system". It is harmless.

To check for `sorry` and list the axioms each theorem uses:

```sh
grep -rnw 'sorry\|admit' ShepZSet ShepZSet.lean   # expect no output
cat > /tmp/Axioms.lean <<'EOF'
import ShepZSet
#print axioms Shep.widen_correct
#print axioms Shep.wf_stateAt_closed
EOF
lake env lean /tmp/Axioms.lean
```

## Map from design properties to theorems

| # | Property | File | Theorems |
|---|---|---|---|
| 1 | Z-sets, group laws, the "is a set" predicate | `Basic.lean` | `ZSet` (= `α →₀ ℤ`), `zset_add_comm`, `zset_add_assoc`, `zset_neg_add_cancel`, …, `IsSet`, `isSet_not_closed_add`, `isSet_not_closed_sub` |
| 2 | The state is the sum of the deltas and doesn't depend on order | `Integrate.lean` | `stateAt`, `applyList_perm`, `applyList_eq_stateAt`, `causalOrder_prefix_closed` |
| 2 | Non-causal intermediate states can be non-sets | `Integrate.lean` | `noncausal_intermediate_not_set` |
| 2/7 | Every causally closed version has a well-formed state, given the side conditions | `Causal.lean` | `wf_stateAt_closed`, `PathDisjoint`, `causallyClosed_union` |
| 2/7 | Concurrent writes to the same key break this | `Facts.lean` | `concurrent_same_field_not_set`, `wf_concurrent` |
| 3 | Change queries: chaining and the holder invariant | `Changes.lean` | `changes`, `changes_chain`, `changes_apply`, `holder_invariant`, `state_sub_of_subset`, `state_diff_general` |
| 4 | Linear operators | `DBSP.lean` | `IsLinear`, `linear_delta`, `linear_delta'`, `zfilter_linear`, `zmap_linear`, `plus_linear`, `comp_linear` |
| 4 | Join is bilinear, plus its incremental formula | `DBSP.lean` | `zjoin_bilinear`, `join_delta`, `bilinear_delta`, `bilinear_delta_new`, `zjoin_isSet` |
| 4 | `distinct` | `DBSP.lean` | `distinct_not_linear`, `distinct_delta`, `distinctH_eq`, `distinctInc_support`, `distinct_idem` |
| 4 | Projection and `UNION` don't preserve sets | `DBSP.lean`, `Basic.lean` | `zmap_not_preserves_set`, `isSet_not_closed_add` |
| 4 | Streams | `DBSP.lean` | `delay`, `integ`, `diff`, `diff_integ` (D∘I = id), `integ_diff` (I∘D = id), `incr_linear`, `incr_comp` (chain rule), `integ_incr` |
| 5/6 | The fact model and well-formedness | `Facts.lean` | `Fact`, `Kind`, `FD`, `TreeShaped`, `WF` |
| 6 | Deltas preserve well-formedness | `Facts.lean` | `wf_set_leaf` (scalar or list leaf, −old +new), `wf_add_leaf`, `wf_add_fresh` (new node or map entry), `wf_add_member`, `wf_remove_member`, `wf_remove_fact`, `wf_remove_subtree`, `wf_remove_map_entry`, `wf_replace_subtree_with_leaf`, `removeSubtreeDelta_apply` |
| 5 | Widening is exact, at any depth, for list leaves, set paths, and maps | `Widen.lean` | `proj`, `widen`, `widen_correct`, `widen_window`, `widen_window_filtered`, `widen_telescope` |
| 5 | Facts outside the projected prefixes contribute nothing | `Widen.lean` | `proj_skip`, `widen_filter`, `footprint_skip_sound` |
| 5 | Queries over widened rows | `Widen.lean` | `widen_linear_query`, `widen_join` |
| 5 | Replace-style (`Option`) leaf widening and its preconditions | `Widen.lean` | `leaf_patch` |
| 7 | Empty containers aren't represented | `Documents.lean` | `encodeDoc_empty_containers` |
| 8 | Grouped aggregates as weights (`COUNT`, `SUM`, the `(COUNT, SUM)` pair for `AVG`) are linear | `Aggregates.lean` | `groupAgg`, `groupAgg_add`, `groupAgg_delta`, `groupCount_linear`, `groupSum_linear`, `groupCountSum_add`, `groupCount_eq_zmap` |
| 8 | Aggregates as rows: the incremental formula, locality, and groups appearing or disappearing | `Aggregates.lean` | `aggRows_delta`, `groupRows_delta`, `countRows_delta`, `sumRows_delta`, `groupAgg_support`, `aggRows_delta_local`, `aggRow_appear`, `aggRow_disappear`, `aggRow_change`, `countRows_not_linear` |
| 8 | `MAX`/`MIN` | `Aggregates.lean` | `max_not_incremental`, `groupMax_restrict`, `groupMax_untouched`, `groupMax_insert` |
| 9 | Recursion: least fixed point and semi-naive evaluation | `Recursion.lean` | `LinStep`, `lfp_fixed`, `lfp_least`, `sn_eq`, `seminaive_correct`, `seminaive_terminates`, `sn_snd_eq_empty_iff` |
| 9 | Transitive closure (nested groups) | `Recursion.lean` | `reach`, `mem_reach` (equals `TransGen`) |
| 9 | Incremental insertion | `Recursion.lean` | `incr_insert`, `lfp_mono`, `tc_insert`, `tc_insert_seed` |
| 9 | Incremental deletion | `Recursion.lean` | `naive_delete_wrong`, `over`, `dred_sound`, `dred_survivors_prefixed`, `dred_correct`, `reach_delete_subset` |
| 9 | Recursion through negation | `Recursion.lean` | `nonmonotone_no_fixpoint` |
| 10 | Adapters as patch lenses; the laws (§4.4) | `Adapters.lean` | `Lens`, `Lens.comp` (`⨾`), `Laws` (laws 1, 2, 4, 5, 6, plus no write to hidden rows), `Rep` (law 3's representable set) |
| 10 | Identity and associativity | `Adapters.lean` | `Lens.comp_assoc`, `Lens.comp_id_right`, `Lens.comp_id_left_iff` (needs no write to hidden rows) |
| 10 | Laws survive composition | `Adapters.lean` | `comp_laws` (lower step exact), `comp_totalRead`, `comp_totalWrite`, `comp_locality` (union of footprints), `comp_noHiddenWrite` |
| 10 | Composed representable set | `Adapters.lean`, `AdapterCounterexamples.lean` | `rep_comp_of`, `rep_comp_iff`, `rep_comp_strict` |
| 10 | Write guard | `Adapters.lean`, `AdapterSystem.lean` | `guard_undo`, `undo_implies_rep`, `rep_union`, `rep_of_each`, `write_readback`, `write_local`, `rep_after_write` |
| 10 | Built-in library | `AdapterLibrary.lean` | `fieldLens_laws`, `fieldLens_rep`, `fieldLens_fuse`, `fieldLaws_comp`, `fieldLens_comp_laws`, `id_laws`, `truncRead_*`, `truncWrite_*`, `truncReject_fieldLaws`, `truncBackfill_spec`, `clampRead_*`, `enumMap_*`, `firstEdge_*`, `defaultRead_*` |
| 10 | Fusion | `AdapterLibrary.lean` | `truncRead_fuse` (`min a b`), `truncRead_fuse_comm`, `truncRead_200_500` |
| 10 | Chains, the running system, moving the head | `AdapterSystem.lean` | `chain_laws`, `chain_laws_top`, `chain_weakLaws`, `chain_snoc`, `run_valid`, `read_valid`, `no_silent_loss`, `head_forward_read`, `head_forward_read_lossless`, `head_forward_many`, `registration_sufficient` |
| 10 | Writes as fact deltas | `AdapterSystem.lean` | `rowFacts_patch`, `wf_rowFacts`, `patchDelta_old_live`, `wf_write_transition`, `lens_write_facts` |
| 10 | Counterexamples | `AdapterCounterexamples.lean` | `chain_putGet_no_norm`, `chain_putPut_fails`, `chain_not_lawful` (with `A1_laws`, `A2_laws`, `B3_laws`), `firstAdd_putGet_fails`, `firstAdd_putPut_fails`, `split_putGet_fails`, `split_getPut_fails`, `split_plainJoin_fails` |
| 11 | Stamp visibility; every causally closed version is valid; 2PC atomic visibility | `Stamps.lean` | `stateAtS_eq_stateAt`, `wf_stateAtS_closed`, `wf_add_parts`, `TwoPC`, `twoPC_atomic`, `twoPC_state`, `twoPC_closed_prepares`, `localStamp_not_atomic` |
| 12 | Validation: serializable in log order | `Validation.lean` | `Validated`, `validated_writes_ordered`, `consistent_all`, `serializable`, `reads_eq_serial`, `toHistory_wf`, `committedOnly_unsound` |
| 13 | Closing versions, the `cf` formula, the frontier | `Closure.lean` | `closure_terminates`, `closure_closed`, `closure_least`, `closure_mono`, `stepDVV_eq`, `iterate_stepPre`, `covered_union`, `stepPre_unsound`, `frontier_closed`, `frontier_greatest`, `frontier_mono`, `closed_biUnion` |
| 14 | Index backfill; ghost completeness | `Backfill.lean` | `backfill_correct`, `actualStamp_some`, `actualStamp_none`, `backfilled_check_sound`, `ghosts_incomplete` |
| 15 | Split lineage: the physical winner; token membership and folding | `Lineage.lean` | `Rules`, `causal_total`, `physLt_of_causal`, `winner_iff_causalLast`, `winner_correct`, `winner_stamp`, `localPos_unsound`, `memL_consistent`, `memL_mono`, `fold_mono`, `fold_inflationary`, `fold_exact`, `fold_grows` |
| 16 | Freshness checks are sound; compaction must be covering | `Freshness.lean` | `History.valAt_eq_of_stamp`, `fresh_sound`, `Scan.eval_readSetSound`, `freshness_sound`, `compaction_refines`, `compaction_sound`, `returnedOnly_unsound`, `noGhost_unsound`, `counterMax_compaction_unsound` |
| 17 | Placeholders read as absent; fill equals create | `Placeholders.lean` | `visible_placeholder`, `visible_placeholder_eq_absent`, `query_placeholder_eq_absent`, `create_placeholder`, `create_equal`, `create_conflict`, `fill_changes_eq_create`, `fill_changes_linear`, `noop_changes` |
| 18 | Walks equal walks over the visible subgraph | `Walks.lean` | `walk_restrict`, `walk_congr_visible`, `mem_walk`, `walk_depth_unique`, `budget_counting_hidden_leaks`, `visibleWork_congr` |
| 19 | Hidden fields don't affect search ranking (IDF excluded) | `SearchPrivacy.lean` | `score_invariant`, `ranking_invariant`, `fieldPolicy_reading_hidden_leaks`, `idf_outside_model` |
| 20 | Viewer mappings; deleted actors can do nothing | `ViewerMapping.lean` | `viewerOf_unique`, `viewerOf_deleted_actor`, `canOp_deleted_actor`, `precheck_needed`, `canOp_deleted_owner` |
| 21 | Skipping a Stable guard preserves outcome-serializability | `Planner.lean` | `DLe`, `respects_mono`, `readAt_le_serial`, `stable_guard_holds`, `outcome_serializable`, `gstab_sound`, `stab_upClosed`, `mutableLifecycle_unsound` |
| 22 | Leader-resolved operations: the context rule, resolution at the log position, bounds | `LeaderOps.lean` | `CtxRule`, `prevK`, `ctxOf_rule`, `ctx_lt`, `ctx_closed`, `selfPrefix_closed`, `ctx_ordered`, `ctx_pathDisjoint`, `leader_hist`, `leader_wf`, `ctx_agrees_logpos`, `lv_eq_logpos`, `lv_eq_ctx`, `window_complete`, `leaderVal_eq_serial`, `bounded_at_position`, `bounded_leader`, `ctxOmitPrev_not_set`, `unrecordedRemoval_not_set`, `committedResolve_crosses_bound` |
| 23 | Polarity never over-grants | `Polarity.lean` | `vis`, `Stab`, `SafeRule`, `NotRS`, `stab_sound`, `unch_sound`, `errFree_sound`, `rules_mono`, `vis_mono`, `notRS_mono`, `vis_not_created`, `history_dle`, `history_admit_mono`, `denyMutable_overgrants`, `ignoreVisible_overgrants`, `denyDelegate_overgrants`, `allowError_overgrants` |
| 24 | Discipline checks preserve frozen facts and permanent lifecycles | `Disciplines.lean` | `FrozenOK`, `PermOK`, `no_ty_in_ctx`, `ty_unique`, `stateAt_frozen`, `frozen_eq_creation`, `created_mono`, `frozen_mono`, `perm_not_deleted`, `present_mono`, `weakFrozen_unsound` |

## Modelling choices

- **Stamps and logs.** Parts (a mutation's delta, or a 2PC participant's part) carry a stamp
  dot; the state at `V` sums the parts stamped in `V`. A partition's log is a sequence of
  entries with a stamp, a read version, a read set (a predicate on facts), and a write set with
  old values. The materializer's state at `S` applies, in log order, the entries stamped in `S`.
- **DVVs.** Dots are `(partition, index)` with indices from `0`; a version's prefix in `p` is
  its first missing index, and dots beyond it are extras.
- **Lineage.** Splits form a forest (`child`, transitive `Anc`), with a generation per
  partition and a split-entry dot per retired partition.
- **Facts.** A fact is `(node, path, value)`. A path is a `List Seg` made of field IDs and
  map keys, plus reserved segments such as `@type`. A schema function `kind : Path → Kind`
  says whether a path is single-valued (a scalar, an atomic list, `@type`, `@deleted_at`)
  or a set path. Values are an arbitrary type, so list values need no special treatment.
- **Well-formed states (`WF`).** A state is well-formed if it is a set (all weights are 0
  or 1), has at most one live value per single-valued `(node, path)`, and is tree-shaped:
  no live leaf sits strictly above another live leaf of the same node.
- **Projections.** A query projects a list of path prefixes of one node type. The column
  for prefix `p` is the node's sub-document under `p`, as a Z-set of `(suffix, value)`.
  For a scalar or list leaf, that is `{([], v)}` or empty. A row's weight is the weight of
  the node's `(n, @type, τ)` fact.
- **Versions.** A version is a `Finset` of dots, each with a delta and a context. Causal
  structure is given by `ctx : Dot → Finset Dot`.
- **Recursion.** A recursive query is the least fixed point of a linear step
  `F(X) = B ∪ G(X)` on finite sets, where `G` distributes over union. Joins with a base
  relation have this form, and so does `delegate`. Sets stand in for Z-sets that are sets.
  The model uses semi-naive evaluation, not DBSP's nested-stream construction.
- **Adapters.** A row is `F → V`, with field IDs shared by every version. A patch is
  `F → Option V`. A lens has `get : Row → Option Row` (`none` is `hide`) and
  `put : Patch → Row → Option Patch` (`none` is `reject`). `put` turns an old version's patch
  into a storage patch, so writes stay patches. Normalization applies per field, to written
  values only. Valid-row sets are arbitrary, except that the library instances use per-field
  types (`Typed`).
- **Not modelled.** DBSP's nested streams, non-linear recursion (a step that joins the
  recursive relation with itself), and edge facts as a separate relation. Edges fit the same
  model as `(src, [edgeType, dst], sortKey)`; see FINDINGS.md.
- **Lists.** Fractional order keys for list elements aren't modelled. The design dropped
  element IDs and order keys, and ordered lists are now atomic leaf values (§10.2).
