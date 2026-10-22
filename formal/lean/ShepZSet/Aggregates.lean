/-
# Grouped aggregates (design.md §11.5 "Grouped aggregates", §11.1 "counts")

* **Linear form.** `COUNT`, `SUM`, and the `(COUNT, SUM)` pair behind `AVG`, kept as
  weights on group keys, are linear and incrementalize directly.
* **Rows form.** One row `(g, value)` per present group. Its delta is
  `−(g, old) + (g, new)` for each group the input delta touches. It needs the old integrated
  aggregate for exactly those groups, as `distinct` does.
* **`MAX`/`MIN`** can't be maintained from the old aggregate value alone under deletions.
  They need the integrated input of each touched group. Insert-only changes are fine.
-/
import ShepZSet.DBSP

noncomputable section
open Classical

namespace Shep

variable {α κ M β : Type*} [AddCommGroup M]

/-! ## Linear form: aggregates as weights on group keys -/

/-- `GROUP BY k` with per-row contribution `f`: the aggregate of group `g` is
`Σ_{a : k a = g} S(a) • f(a)`. With `f = 1` this is `COUNT`; with `f = v` it is `SUM v`; with
`f = (1, v)` it is the `(COUNT, SUM)` pair behind `AVG`. -/
def groupAgg (k : α → κ) (f : α → M) (S : ZSet α) : κ →₀ M :=
  S.sum (fun a w => Finsupp.single (k a) (w • f a))

theorem groupAgg_add (k : α → κ) (f : α → M) (S T : ZSet α) :
    groupAgg k f (S + T) = groupAgg k f S + groupAgg k f T := by
  unfold groupAgg
  rw [Finsupp.sum_add_index']
  · intro a; simp
  · intro a w₁ w₂; rw [add_zsmul, Finsupp.single_add]

theorem groupAgg_zero (k : α → κ) (f : α → M) : groupAgg k f 0 = 0 := by
  simp [groupAgg]

/-- **Linear aggregates incrementalize directly**: the new aggregate is the old one plus the
aggregate of the delta. -/
theorem groupAgg_delta (k : α → κ) (f : α → M) (S Δ : ZSet α) :
    groupAgg k f (S + Δ) - groupAgg k f S = groupAgg k f Δ := by
  rw [groupAgg_add]; abel

/-- `COUNT` per group. -/
def groupCount (k : α → κ) (S : ZSet α) : ZSet κ := groupAgg k (fun _ => (1 : ℤ)) S

/-- `SUM v` per group. -/
def groupSum (k : α → κ) (v : α → ℤ) (S : ZSet α) : ZSet κ := groupAgg k v S

/-- The `(COUNT, SUM)` pair per group; `AVG = SUM / COUNT` is a function of it. -/
def groupCountSum (k : α → κ) (v : α → ℤ) (S : ZSet α) : κ →₀ ℤ × ℤ :=
  groupAgg k (fun a => ((1 : ℤ), v a)) S

theorem groupCount_linear (k : α → κ) : IsLinear (groupCount k) :=
  fun S T => groupAgg_add k _ S T

theorem groupSum_linear (k : α → κ) (v : α → ℤ) : IsLinear (groupSum k v) :=
  fun S T => groupAgg_add k v S T

theorem groupCountSum_add (k : α → κ) (v : α → ℤ) (S T : ZSet α) :
    groupCountSum k v (S + T) = groupCountSum k v S + groupCountSum k v T :=
  groupAgg_add k _ S T

/-- `COUNT` as weights is `zmap k`: projection with summing. -/
theorem groupCount_eq_zmap (k : α → κ) (S : ZSet α) : groupCount k S = zmap k S := by
  unfold groupCount groupAgg zmap Finsupp.mapDomain
  apply Finsupp.sum_congr
  intro a _; simp

/-- **Locality**: the delta's aggregate is supported on the groups of the delta's rows. -/
theorem groupAgg_support (k : α → κ) (f : α → M) (Δ : ZSet α) :
    (groupAgg k f Δ).support ⊆ Δ.support.image k := by
  unfold groupAgg Finsupp.sum
  refine (Finsupp.support_finsetSum).trans ?_
  intro g hg
  simp only [Finset.mem_biUnion] at hg
  obtain ⟨a, ha, hg⟩ := hg
  have := Finsupp.support_single_subset hg
  simp only [Finset.mem_singleton] at this
  exact Finset.mem_image.mpr ⟨a, ha, this.symm⟩

/-! ## Rows form: one row per present group -/

/-- The row a group contributes, given its aggregate value `m`: `(g, out m)` if the group is
present (`present m`), nothing otherwise. For `COUNT`, a group is present when its count is
non-zero; for `SUM`/`AVG`, when the count component is non-zero (a group whose values sum to
0 still exists). -/
def aggRow (present : M → Prop) (out : M → β) (g : κ) (m : M) : ZSet (κ × β) :=
  if present m then Finsupp.single (g, out m) 1 else 0

/-- The result as rows. -/
def aggRows (present : M → Prop) (out : M → β) (A : κ →₀ M) : ZSet (κ × β) :=
  ∑ g ∈ A.support, aggRow present out g (A g)

theorem aggRows_eq_sum_of_subset (present : M → Prop) (out : M → β)
    (hp : ¬ present 0) (A : κ →₀ M) {T : Finset κ} (hT : A.support ⊆ T) :
    aggRows present out A = ∑ g ∈ T, aggRow present out g (A g) := by
  unfold aggRows
  apply Finset.sum_subset hT
  intro g _ hg
  rw [Finsupp.notMem_support_iff] at hg
  simp [aggRow, hg, hp]

/-- **Incremental rows form.** The change in the result rows is a sum over the groups the
delta touches only, and it reads the old aggregate `A g` only at those groups. -/
theorem aggRows_delta (present : M → Prop) (out : M → β) (hp : ¬ present 0)
    (A dA : κ →₀ M) :
    aggRows present out (A + dA) - aggRows present out A =
      ∑ g ∈ dA.support, (aggRow present out g (A g + dA g) - aggRow present out g (A g)) := by
  set T := A.support ∪ (A + dA).support ∪ dA.support with hT
  rw [aggRows_eq_sum_of_subset present out hp (A + dA) (T := T)
      (fun g h => by simp [hT, h]),
    aggRows_eq_sum_of_subset present out hp A (T := T) (fun g h => by simp [hT, h]),
    ← Finset.sum_sub_distrib]
  symm
  apply Finset.sum_subset_zero_on_sdiff (fun g h => by simp [hT, h])
  · intro g hg
    have : dA g = 0 := Finsupp.notMem_support_iff.mp (Finset.mem_sdiff.mp hg).2
    simp [this]
  · intro g _; simp

/-- The incremental formula needs the old aggregate only on the touched groups: two old
states that agree there give the same output delta. -/
theorem aggRows_delta_local (present : M → Prop) (out : M → β) (A₁ A₂ dA : κ →₀ M)
    (h : ∀ g ∈ dA.support, A₁ g = A₂ g) :
    (∑ g ∈ dA.support, (aggRow present out g (A₁ g + dA g) - aggRow present out g (A₁ g))) =
      ∑ g ∈ dA.support, (aggRow present out g (A₂ g + dA g) - aggRow present out g (A₂ g)) :=
  Finset.sum_congr rfl (fun g hg => by rw [h g hg])

/-- End to end, for an input delta `Δ`: only the groups of `Δ`'s rows are touched. -/
theorem groupRows_delta (present : M → Prop) (out : M → β) (hp : ¬ present 0)
    (k : α → κ) (f : α → M) (S Δ : ZSet α) :
    aggRows present out (groupAgg k f (S + Δ)) - aggRows present out (groupAgg k f S) =
      ∑ g ∈ (groupAgg k f Δ).support,
        (aggRow present out g (groupAgg k f S g + groupAgg k f Δ g) -
          aggRow present out g (groupAgg k f S g)) := by
  rw [groupAgg_add]; exact aggRows_delta present out hp _ _

omit [AddCommGroup M] in
/-- Edge case: a group that appears (absent before) emits only `+(g, new)`. -/
theorem aggRow_appear (present : M → Prop) (out : M → β) (g : κ) {m m' : M}
    (h : ¬ present m) (h' : present m') :
    aggRow present out g m' - aggRow present out g m = Finsupp.single (g, out m') 1 := by
  simp [aggRow, h, h']

omit [AddCommGroup M] in
/-- Edge case: a group that disappears emits only `−(g, old)`. -/
theorem aggRow_disappear (present : M → Prop) (out : M → β) (g : κ) {m m' : M}
    (h : present m) (h' : ¬ present m') :
    aggRow present out g m' - aggRow present out g m = -Finsupp.single (g, out m) 1 := by
  simp [aggRow, h, h']

omit [AddCommGroup M] in
/-- A group that stays present emits `−(g, old) + (g, new)` (zero if the value didn't change). -/
theorem aggRow_change (present : M → Prop) (out : M → β) (g : κ) {m m' : M}
    (h : present m) (h' : present m') :
    aggRow present out g m' - aggRow present out g m =
      Finsupp.single (g, out m') 1 - Finsupp.single (g, out m) 1 := by
  simp [aggRow, h, h']

/-- `COUNT` as rows: present when the count is non-zero. -/
def countRows (k : α → κ) (S : ZSet α) : ZSet (κ × ℤ) :=
  aggRows (fun c : ℤ => c ≠ 0) id (groupCount k S)

/-- `SUM` as rows: present when the group has rows (count ≠ 0), value = the sum. -/
def sumRows (k : α → κ) (v : α → ℤ) (S : ZSet α) : ZSet (κ × ℤ) :=
  aggRows (fun cs : ℤ × ℤ => cs.1 ≠ 0) Prod.snd (groupCountSum k v S)

theorem countRows_delta (k : α → κ) (S Δ : ZSet α) :
    countRows k (S + Δ) - countRows k S =
      ∑ g ∈ (groupCount k Δ).support,
        (aggRow (fun c : ℤ => c ≠ 0) id g (groupCount k S g + groupCount k Δ g) -
          aggRow (fun c : ℤ => c ≠ 0) id g (groupCount k S g)) :=
  groupRows_delta _ _ (by simp) k _ S Δ

theorem sumRows_delta (k : α → κ) (v : α → ℤ) (S Δ : ZSet α) :
    sumRows k v (S + Δ) - sumRows k v S =
      ∑ g ∈ (groupCountSum k v Δ).support,
        (aggRow (fun cs : ℤ × ℤ => cs.1 ≠ 0) Prod.snd g
            (groupCountSum k v S g + groupCountSum k v Δ g) -
          aggRow (fun cs : ℤ × ℤ => cs.1 ≠ 0) Prod.snd g (groupCountSum k v S g)) :=
  groupRows_delta _ _ (by simp) k _ S Δ

/-- The rows form is not linear: a count going from 1 to 2 replaces the row, it doesn't add
one. -/
theorem countRows_not_linear :
    ¬ IsLinear (countRows (fun _ : Unit => ())) := by
  intro h
  have h1 := h (Finsupp.single () 1) (Finsupp.single () 1)
  have e1 : countRows (fun _ : Unit => ()) (Finsupp.single () 1) =
      Finsupp.single ((), 1) 1 := by
    simp [countRows, aggRows, groupCount, groupAgg, aggRow]
  have e2 : countRows (fun _ : Unit => ()) (Finsupp.single () 1 + Finsupp.single () 1) =
      Finsupp.single ((), 2) 1 := by
    rw [← Finsupp.single_add]
    simp [countRows, aggRows, groupCount, groupAgg, aggRow]
  rw [e1, e2] at h1
  have := congrArg (fun S => S ((), 2)) h1
  simp at this

/-! ## `MAX` / `MIN` -/

/-- `MAX v` per group over the rows with positive weight (`⊥` for an empty group). -/
def groupMax (k : α → κ) (v : α → ℤ) (S : ZSet α) (g : κ) : WithBot ℤ :=
  (S.support.filter (fun a => k a = g ∧ 0 < S a)).sup (fun a => (v a : WithBot ℤ))

theorem groupMax_eq_of_set (k : α → κ) (v : α → ℤ) (S : ZSet α) (g : κ) (s : Finset α)
    (hs : ∀ a, a ∈ s ↔ k a = g ∧ 0 < S a) :
    groupMax k v S g = s.sup (fun a => (v a : WithBot ℤ)) := by
  unfold groupMax
  congr 1
  ext a
  rw [hs a, Finset.mem_filter, Finsupp.mem_support_iff]
  constructor
  · exact fun h => h.2
  · exact fun h => ⟨by omega, h⟩

/-- **`MAX` is not incremental from the old aggregate alone.** Two states with the same
maximum (5), the same delta (delete the 5), and different new maxima (3 and 1): the new value
depends on the next-largest element, which the old aggregate doesn't record. -/
theorem max_not_incremental :
    ¬ ∃ h : WithBot ℤ → ZSet ℤ → WithBot ℤ, ∀ S Δ : ZSet ℤ,
      groupMax (fun _ => ()) id (S + Δ) () = h (groupMax (fun _ => ()) id S ()) Δ := by
  rintro ⟨h, hh⟩
  have two : ∀ x : ℤ, x < 5 →
      groupMax (fun _ => ()) id (Finsupp.single x 1 + Finsupp.single 5 1) () = 5 := by
    intro x hx
    rw [groupMax_eq_of_set _ _ _ _ {x, 5}]
    · rw [Finset.sup_insert, Finset.sup_singleton]
      simp only [id]
      exact sup_eq_right.mpr (by exact_mod_cast hx.le)
    · intro a
      simp only [Finset.mem_insert, Finset.mem_singleton, Finsupp.add_apply,
        Finsupp.single_apply, true_and]
      split_ifs <;> constructor <;> intro h <;> omega
  have one : ∀ x : ℤ, x < 5 →
      groupMax (fun _ => ()) id
        (Finsupp.single x 1 + Finsupp.single 5 1 + -Finsupp.single 5 1) () = x := by
    intro x _
    rw [add_neg_cancel_right, groupMax_eq_of_set _ _ _ _ {x}]
    · simp
    · intro a
      simp only [Finset.mem_singleton, Finsupp.single_apply, true_and]
      split_ifs <;> constructor <;> intro h <;> omega
  have e3 := hh (Finsupp.single 3 1 + Finsupp.single 5 1) (-Finsupp.single 5 1)
  have e1 := hh (Finsupp.single 1 1 + Finsupp.single 5 1) (-Finsupp.single 5 1)
  rw [one 3 (by norm_num), two 3 (by norm_num)] at e3
  rw [one 1 (by norm_num), two 1 (by norm_num)] at e1
  have : ((3 : ℤ) : WithBot ℤ) = ((1 : ℤ) : WithBot ℤ) := e3.trans e1.symm
  simp at this

/-- `MAX` of a group depends only on that group's rows: incremental `MAX` needs the
integrated input restricted to the touched groups (or an ordered index over it). -/
theorem groupMax_restrict (k : α → κ) (v : α → ℤ) (S : ZSet α) (g : κ) :
    groupMax k v S g = groupMax k v (Finsupp.filter (fun a => k a = g) S) g := by
  rw [groupMax_eq_of_set k v (Finsupp.filter (fun a => k a = g) S) g
    (S.support.filter (fun a => k a = g ∧ 0 < S a))]
  · rfl
  · intro a
    rw [Finset.mem_filter, Finsupp.mem_support_iff, Finsupp.filter_apply]
    by_cases hk : k a = g
    · rw [ite_eq_left hk]
      constructor
      · rintro ⟨-, -, h⟩; exact ⟨hk, h⟩
      · rintro ⟨-, h⟩; exact ⟨by omega, hk, h⟩
    · rw [ite_eq_right hk]; simp [hk]

/-- Groups the delta doesn't touch keep their `MAX`. -/
theorem groupMax_untouched (k : α → κ) (v : α → ℤ) (S Δ : ZSet α) (g : κ)
    (hΔ : ∀ a, k a = g → Δ a = 0) : groupMax k v (S + Δ) g = groupMax k v S g := by
  rw [groupMax_eq_of_set k v (S + Δ) g (S.support.filter (fun a => k a = g ∧ 0 < S a))]
  · rfl
  · intro a
    rw [Finset.mem_filter, Finsupp.mem_support_iff, Finsupp.add_apply]
    constructor
    · rintro ⟨-, hk, hpos⟩; exact ⟨hk, by rw [hΔ a hk]; omega⟩
    · rintro ⟨hk, hpos⟩
      rw [hΔ a hk] at hpos
      exact ⟨by omega, hk, by omega⟩

/-- **Insert-only changes are incremental from the old value**: if the state and the delta
are both non-negative, the new `MAX` is the old one joined with the delta's. Only deletions
need the group's integrated input. -/
theorem groupMax_insert (k : α → κ) (v : α → ℤ) (S Δ : ZSet α) (g : κ)
    (hS : IsPositive S) (hΔ : IsPositive Δ) :
    groupMax k v (S + Δ) g = groupMax k v S g ⊔ groupMax k v Δ g := by
  rw [groupMax_eq_of_set k v (S + Δ) g
    ((S.support.filter (fun a => k a = g ∧ 0 < S a)) ∪
      (Δ.support.filter (fun a => k a = g ∧ 0 < Δ a)))]
  · rw [Finset.sup_union]; rfl
  · intro a
    have h1 := hS a; have h2 := hΔ a
    simp only [Finset.mem_union, Finset.mem_filter, Finsupp.mem_support_iff,
      Finsupp.add_apply]
    constructor
    · rintro (⟨-, hk, hp⟩ | ⟨-, hk, hp⟩) <;> exact ⟨hk, by omega⟩
    · rintro ⟨hk, hp⟩
      by_cases hs : 0 < S a
      · exact Or.inl ⟨by omega, hk, hs⟩
      · exact Or.inr ⟨by omega, hk, by omega⟩

end Shep
