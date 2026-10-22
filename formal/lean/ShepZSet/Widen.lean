/-
# Widening field facts into per-query projection rows (design.md §11.5)

A query reads a *projection* of one node type: a row per node holding, for each projected
path prefix, the node's content under that prefix. A prefix can be a scalar leaf, a list
leaf (an atomic value), a set path, or an interior path (a nested struct or a map), at any
depth. The column for prefix `p` is the sub-document under `p`: the Z-set of
`(suffix, value)` pairs of the node's facts at `p ++ suffix`. For a scalar or list leaf that
is `{([], v)}` or empty; for a set path it is the set of members; for a map it is the map's
leaves.

*Widening* turns a fact-level delta `Δ` into projection rows: for each node it touches
(on a projected path or on its `@type` fact), `−old row` and `+new row`, where the old row
comes from the state before `Δ` and the new row is the old row with `Δ`'s facts applied.
-/
import ShepZSet.Facts

noncomputable section
open Classical

namespace Shep

variable {N Seg V : Type*}

/-- A projection row: the node and one column per projected prefix. -/
abbrev Row (N Seg V : Type*) := N × List (ZSet (Path Seg × V))

section Projection

variable (typePath : Path Seg) (τ : V) (Ps : List (Path Seg))

/-- Re-rooting a node's facts under prefix `p`. -/
def under (n : N) (p : Path Seg) : Path Seg × V → Fact N Seg V := fun qv => (n, p ++ qv.1, qv.2)

theorem under_injective (n : N) (p : Path Seg) : Function.Injective (under (V := V) n p) := by
  intro ⟨q, v⟩ ⟨q', v'⟩ h
  simp only [under, Prod.mk.injEq, List.append_cancel_left_eq] at h
  obtain ⟨-, rfl, rfl⟩ := h; rfl

/-- The column for prefix `p`: the sub-document of node `n` under `p`. -/
def col (S : ZSet (Fact N Seg V)) (n : N) (p : Path Seg) : ZSet (Path Seg × V) :=
  Finsupp.comapDomain (under n p) S (under_injective n p).injOn

@[simp] theorem col_apply (S : ZSet (Fact N Seg V)) (n : N) (p : Path Seg) (q : Path Seg)
    (v : V) : col S n p (q, v) = S (n, p ++ q, v) := by
  simp [col, Finsupp.comapDomain_apply, under]

theorem col_add (S Δ : ZSet (Fact N Seg V)) (n : N) (p : Path Seg) :
    col (S + Δ) n p = col S n p + col Δ n p := by
  ext ⟨q, v⟩; simp

/-- The row of node `n`. -/
def row (S : ZSet (Fact N Seg V)) (n : N) : Row N Seg V := (n, Ps.map (col S n))

/-- The weight of node `n`'s row: the weight of its `(n, @type, τ)` fact (1 if the node
exists with type `τ`, 0 otherwise, in a well-formed state). -/
def wt (S : ZSet (Fact N Seg V)) (n : N) : ℤ := S (n, typePath, τ)

/-- The nodes mentioned by a Z-set of facts. -/
def nodesOf (S : ZSet (Fact N Seg V)) : Finset N := S.support.image Prod.fst

/-- **The projection** of node type `τ` onto prefixes `Ps`. -/
def proj (S : ZSet (Fact N Seg V)) : ZSet (Row N Seg V) :=
  ∑ n ∈ nodesOf S, Finsupp.single (row Ps S n) (wt typePath τ S n)

/-- A fact is *relevant* to the query if it is a `@type` fact or lies under a projected
prefix. -/
def Relevant (f : Fact N Seg V) : Prop := f.2.1 = typePath ∨ ∃ p ∈ Ps, p <+: f.2.1

/-- The nodes a delta touches on relevant paths. -/
def touched (Δ : ZSet (Fact N Seg V)) : Finset N :=
  (Δ.support.filter (Relevant typePath Ps)).image Prod.fst

/-- The patched new row: the old row with the delta's facts applied, column by column. -/
def newRow (S Δ : ZSet (Fact N Seg V)) (n : N) : Row N Seg V :=
  (n, Ps.map (fun p => col S n p + col Δ n p))

/-- One merged `−old / +new` pair per touched node. -/
def widenNode (S Δ : ZSet (Fact N Seg V)) (n : N) : ZSet (Row N Seg V) :=
  Finsupp.single (newRow Ps S Δ n) (wt typePath τ S n + wt typePath τ Δ n) -
    Finsupp.single (row Ps S n) (wt typePath τ S n)

/-- **Widening** a fact delta `Δ` against the state `S` it applies to. -/
def widen (S Δ : ZSet (Fact N Seg V)) : ZSet (Row N Seg V) :=
  ∑ n ∈ touched typePath Ps Δ, widenNode typePath τ Ps S Δ n

/-! ### Lemmas -/

theorem proj_eq_sum_of_subset (S : ZSet (Fact N Seg V)) {T : Finset N}
    (hT : nodesOf S ⊆ T) :
    proj typePath τ Ps S = ∑ n ∈ T, Finsupp.single (row Ps S n) (wt typePath τ S n) := by
  unfold proj
  apply Finset.sum_subset hT
  intro n _ hn
  have : wt typePath τ S n = 0 := by
    unfold wt
    by_contra h
    apply hn
    unfold nodesOf
    exact Finset.mem_image.mpr ⟨(n, typePath, τ), Finsupp.mem_support_iff.mpr h, rfl⟩
  simp [this]

theorem newRow_eq (S Δ : ZSet (Fact N Seg V)) (n : N) :
    newRow Ps S Δ n = row Ps (S + Δ) n := by
  simp [newRow, row, col_add]

theorem untouched_col (Δ : ZSet (Fact N Seg V)) {n : N} (hn : n ∉ touched typePath Ps Δ)
    {p : Path Seg} (hp : p ∈ Ps) : col Δ n p = 0 := by
  ext ⟨q, v⟩
  rw [col_apply]
  by_contra h
  apply hn
  unfold touched
  refine Finset.mem_image.mpr ⟨(n, p ++ q, v), ?_, rfl⟩
  rw [Finset.mem_filter, Finsupp.mem_support_iff]
  exact ⟨by simpa using h, Or.inr ⟨p, hp, List.prefix_append p q⟩⟩

theorem untouched_wt (Δ : ZSet (Fact N Seg V)) {n : N} (hn : n ∉ touched typePath Ps Δ) :
    wt typePath τ Δ n = 0 := by
  unfold wt
  by_contra h
  apply hn
  unfold touched
  refine Finset.mem_image.mpr ⟨(n, typePath, τ), ?_, rfl⟩
  rw [Finset.mem_filter, Finsupp.mem_support_iff]
  exact ⟨h, Or.inl rfl⟩

theorem untouched_row (S Δ : ZSet (Fact N Seg V)) {n : N} (hn : n ∉ touched typePath Ps Δ) :
    row Ps (S + Δ) n = row Ps S n := by
  simp only [row, Prod.mk.injEq, true_and]
  apply List.map_congr_left
  intro p hp
  rw [col_add, untouched_col typePath Ps Δ hn hp, add_zero]

theorem touched_subset_nodesOf (Δ : ZSet (Fact N Seg V)) :
    touched typePath Ps Δ ⊆ nodesOf Δ := by
  intro n hn
  unfold touched at hn; unfold nodesOf
  obtain ⟨f, hf, rfl⟩ := Finset.mem_image.mp hn
  exact Finset.mem_image.mpr ⟨f, (Finset.mem_filter.mp hf).1, rfl⟩

/-! ### Main theorem -/

/-- **Widening is exact**: for *any* state `S` and delta `Δ`,
`widen S Δ = proj (S + Δ) − proj S`.

No well-formedness precondition is needed, because the new row is computed additively
(each column is the old sub-document plus `Δ`'s facts under that prefix). This holds for
projected prefixes at any depth, for list-valued leaves (lists are just values), for set
paths, and for maps. Merging (one pair per node per delta, §11.5) is built in. -/
theorem widen_correct (S Δ : ZSet (Fact N Seg V)) :
    widen typePath τ Ps S Δ = proj typePath τ Ps (S + Δ) - proj typePath τ Ps S := by
  classical
  set T := nodesOf S ∪ nodesOf (S + Δ) ∪ nodesOf Δ with hTdef
  have h1 : nodesOf S ⊆ T := fun n h => by simp [hTdef, h]
  have h2 : nodesOf (S + Δ) ⊆ T := fun n h => by simp [hTdef, h]
  have h3 : touched typePath Ps Δ ⊆ T := fun n h => by
    simp [hTdef, touched_subset_nodesOf typePath Ps Δ h]
  rw [proj_eq_sum_of_subset typePath τ Ps (S + Δ) h2, proj_eq_sum_of_subset typePath τ Ps S h1,
    ← Finset.sum_sub_distrib]
  unfold widen
  apply Finset.sum_subset_zero_on_sdiff h3
  · intro n hn
    have hn' := (Finset.mem_sdiff.mp hn).2
    rw [untouched_row typePath Ps S Δ hn']
    have : wt typePath τ (S + Δ) n = wt typePath τ S n := by
      simp [wt, Finsupp.add_apply, show Δ (n, typePath, τ) = 0 from untouched_wt typePath τ Ps Δ hn']
    rw [this, sub_self]
  · intro n _
    simp only [widenNode, newRow_eq, wt, Finsupp.add_apply]

/-- The §11.5 form: `ΔI` is the sum of the window's deltas and the state at `V1` is
`S₂ − ΔI` (the snapshot "rolled back" by the whole window). -/
theorem widen_window (S₂ ΔI : ZSet (Fact N Seg V)) :
    widen typePath τ Ps (S₂ - ΔI) ΔI = proj typePath τ Ps S₂ - proj typePath τ Ps (S₂ - ΔI) := by
  have := widen_correct typePath τ Ps (S₂ - ΔI) ΔI
  simpa using this

/-- Widening entry by entry, each entry against the state just before it (in the order
given), telescopes to widening the summed window: per-entry and whole-window merging agree.
(Holds for any order of the entries, causal or not, because `widen_correct` is
unconditional.) -/
def widenSeq (S : ZSet (Fact N Seg V)) : List (ZSet (Fact N Seg V)) → ZSet (Row N Seg V)
  | [] => 0
  | Δ :: Δs => widen typePath τ Ps S Δ + widenSeq (S + Δ) Δs

theorem widen_telescope (S₀ : ZSet (Fact N Seg V)) (Δs : List (ZSet (Fact N Seg V))) :
    widenSeq typePath τ Ps S₀ Δs = widen typePath τ Ps S₀ Δs.sum := by
  induction Δs generalizing S₀ with
  | nil => simp [widenSeq, widen_correct]
  | cons Δ Δs ih =>
    rw [widenSeq, ih, widen_correct, widen_correct, widen_correct, List.sum_cons, add_assoc]
    abel

/-- **Skipping** (§11.5): a delta whose facts all lie outside the projected prefixes (and
are not `@type` facts) contributes nothing. -/
theorem proj_skip (S Δ : ZSet (Fact N Seg V))
    (hΔ : ∀ f ∈ Δ.support, ¬ Relevant typePath Ps f) :
    proj typePath τ Ps (S + Δ) = proj typePath τ Ps S := by
  have ht : touched typePath Ps Δ = ∅ := by
    unfold touched
    rw [Finset.image_eq_empty, Finset.filter_eq_empty_iff]
    exact hΔ
  have := widen_correct typePath τ Ps S Δ
  rw [widen, ht, Finset.sum_empty] at this
  exact (sub_eq_zero.mp this.symm)

/-- Footprint filtering is sound: widening only ever looks at the relevant facts. -/
theorem widen_filter (S Δ : ZSet (Fact N Seg V)) :
    widen typePath τ Ps S Δ = widen typePath τ Ps S (Finsupp.filter (Relevant typePath Ps) Δ) := by
  rw [widen_correct, widen_correct]
  -- `S + Δ` and `S + filter Δ` differ by the irrelevant part of `Δ`
  have hsplit : S + Δ = (S + Finsupp.filter (Relevant typePath Ps) Δ) +
      Finsupp.filter (fun f => ¬ Relevant typePath Ps f) Δ := by
    rw [add_assoc, Finsupp.filter_add_filter_not]
  rw [hsplit, proj_skip]
  intro f hf
  rw [Finsupp.support_filter, Finset.mem_filter] at hf
  exact hf.2

/-- §11.5 combines footprint filtering with "the state at `V1` is `V2 − ΔI`". If `ΔI` has been
filtered to the relevant facts, `S₂ − filter ΔI` is *not* the state at `V1` (irrelevant facts
were not rolled back), but it agrees with it on everything the projection reads, so widening
against it is still exact. -/
theorem widen_window_filtered (S₂ ΔI : ZSet (Fact N Seg V)) :
    let ΔR := Finsupp.filter (Relevant typePath Ps) ΔI
    widen typePath τ Ps (S₂ - ΔR) ΔR =
      proj typePath τ Ps S₂ - proj typePath τ Ps (S₂ - ΔI) := by
  intro ΔR
  rw [widen_correct, sub_add_cancel]
  congr 1
  have hsplit : S₂ - ΔR = (S₂ - ΔI) + Finsupp.filter (fun f => ¬ Relevant typePath Ps f) ΔI := by
    have := Finsupp.filter_add_filter_not (p := Relevant typePath Ps) ΔI
    have h2 : Finsupp.filter (fun f => ¬ Relevant typePath Ps f) ΔI = ΔI - ΔR := by
      rw [eq_sub_iff_add_eq, add_comm]; exact this
    rw [h2]; abel
  rw [hsplit, proj_skip]
  intro f hf
  rw [Finsupp.support_filter, Finset.mem_filter] at hf
  exact hf.2

/-- Footprints by prefix (§10.1): if every fact of the delta lies under some footprint
prefix `r`, and each such `r` is prefix-incomparable with every projected prefix and with
`@type`, the entry can be skipped without decoding. (Comparability, not just
"`r` under a projected prefix", is the right test: a footprint prefix *above* a projected
prefix may cover relevant facts.) -/
theorem footprint_skip_sound (S Δ : ZSet (Fact N Seg V)) (fp : Finset (Path Seg))
    (hcover : ∀ f ∈ Δ.support, ∃ r ∈ fp, r <+: f.2.1)
    (hdisj : ∀ r ∈ fp, (¬ r <+: typePath ∧ ¬ typePath <+: r) ∧
      ∀ p ∈ Ps, ¬ r <+: p ∧ ¬ p <+: r) :
    proj typePath τ Ps (S + Δ) = proj typePath τ Ps S := by
  apply proj_skip
  intro f hf hrel
  obtain ⟨r, hr, hrf⟩ := hcover f hf
  obtain ⟨⟨ht1, ht2⟩, hp⟩ := hdisj r hr
  rcases hrel with h | ⟨p, hp', hpf⟩
  · exact ht1 (h ▸ hrf)
  · rcases List.prefix_or_prefix_of_prefix hrf hpf with h | h
    · exact (hp p hp').1 h
    · exact (hp p hp').2 h

/-- A linear query `Q` over projection rows is maintained by applying `Q` to the widened
rows. -/
theorem widen_linear_query {β : Type*} {Q : ZSet (Row N Seg V) → ZSet β} (hQ : IsLinear Q)
    (S Δ : ZSet (Fact N Seg V)) :
    changes (Q ∘ proj typePath τ Ps) S (S + Δ) = Q (widen typePath τ Ps S Δ) := by
  rw [widen_correct, hQ.map_sub]; rfl

/-- A join of two projections (e.g. two node types joined along an edge relation) is
maintained by the bilinear formula applied to the widened rows. -/
theorem widen_join {α γ : Type*} {J : ZSet (Row N Seg V) → ZSet α → ZSet γ}
    (hJ : IsBilinear J) (S Δ : ZSet (Fact N Seg V)) (B₁ ΔB : ZSet α) :
    J (proj typePath τ Ps (S + Δ)) (B₁ + ΔB) - J (proj typePath τ Ps S) B₁ =
      J (widen typePath τ Ps S Δ) B₁ + J (proj typePath τ Ps S) ΔB +
        J (widen typePath τ Ps S Δ) ΔB := by
  have h := bilinear_delta hJ (proj typePath τ Ps S) (widen typePath τ Ps S Δ) B₁ ΔB
  rw [widen_correct, add_sub_cancel] at h
  rw [h, widen_correct]

end Projection

/-! ## Option-valued leaf columns ("replace" widening)

An implementation will usually represent a scalar or list column as `Option V` and compute
the new value by *replacing*: take the value of the `+` fact on that path if there is one,
else `none` if there is a `−` fact, else keep the old value. Unlike the additive form above,
this is only exact when both the old and the new state are well-formed at that key — in
particular a change delta must carry the `−old` fact along with `+new` (§10.1), otherwise
the new state has two live values. -/

section Leaf

/-- The value of a single-valued leaf, if any. -/
def leafVal (S : ZSet (Fact N Seg V)) (n : N) (p : Path Seg) : Option V :=
  if h : ∃ v, S (n, p, v) ≠ 0 then some (Classical.choose h) else none

/-- Replace-style patching of an optional leaf value by a delta. -/
def patchLeaf (o : Option V) (Δ : ZSet (Fact N Seg V)) (n : N) (p : Path Seg) : Option V :=
  if h : ∃ v, 0 < Δ (n, p, v) then some (Classical.choose h)
  else if ∃ v, Δ (n, p, v) < 0 then none else o

/-- Functional dependency at one key. -/
def FDAt (S : ZSet (Fact N Seg V)) (n : N) (p : Path Seg) : Prop :=
  ∀ v w, S (n, p, v) ≠ 0 → S (n, p, w) ≠ 0 → v = w

theorem leafVal_eq_some {S : ZSet (Fact N Seg V)} {n : N} {p : Path Seg} (hfd : FDAt S n p)
    {v : V} (hv : S (n, p, v) ≠ 0) : leafVal S n p = some v := by
  have h : ∃ v, S (n, p, v) ≠ 0 := ⟨v, hv⟩
  rw [leafVal, dite_eq_left h, hfd _ _ (Classical.choose_spec h) hv]

theorem leafVal_eq_none {S : ZSet (Fact N Seg V)} {n : N} {p : Path Seg}
    (h : ∀ v, S (n, p, v) = 0) : leafVal S n p = none := by
  rw [leafVal, dite_eq_right]; push Not; exact h

/-- **Replace-style widening of a leaf column is exact** when the old and new states are sets
and functional at that key. -/
theorem leaf_patch {S Δ : ZSet (Fact N Seg V)} {n : N} {p : Path Seg}
    (hS : IsSet S) (hS' : IsSet (S + Δ)) (hfd : FDAt S n p) (hfd' : FDAt (S + Δ) n p) :
    leafVal (S + Δ) n p = patchLeaf (leafVal S n p) Δ n p := by
  unfold patchLeaf
  by_cases hpos : ∃ v, 0 < Δ (n, p, v)
  · rw [dite_eq_left hpos]
    have hc := Classical.choose_spec hpos
    apply leafVal_eq_some hfd'
    have := hS (n, p, Classical.choose hpos); have := hS' (n, p, Classical.choose hpos)
    simp only [Finsupp.add_apply] at *
    omega
  · rw [dite_eq_right hpos]
    push Not at hpos
    by_cases hneg : ∃ v, Δ (n, p, v) < 0
    · rw [ite_eq_left hneg]
      obtain ⟨v, hv⟩ := hneg
      apply leafVal_eq_none
      intro w
      by_contra hw
      have h1 := hS (n, p, w); have h2 := hS' (n, p, w); have h3 := hpos w
      have h4 := hS (n, p, v); have h5 := hS' (n, p, v)
      simp only [Finsupp.add_apply] at *
      -- `w` is live in `S`, and so is `v`; functional dependency forces `w = v`
      have hwS : S (n, p, w) ≠ 0 := by omega
      have hvS : S (n, p, v) ≠ 0 := by omega
      have := hfd w v hwS hvS
      subst this
      omega
    · rw [ite_eq_right hneg]
      push Not at hneg
      have hz : ∀ v, Δ (n, p, v) = 0 := fun v => le_antisymm (hpos v) (hneg v)
      unfold leafVal
      simp only [Finsupp.add_apply, hz, add_zero]

end Leaf

end Shep
