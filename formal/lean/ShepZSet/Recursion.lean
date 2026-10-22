/-
# Recursive queries: least fixed points, semi-naive evaluation, incremental maintenance
(design.md §6.8 "Recursion", §11.5 "Recursive queries", §12)

Recursive delegation (nested groups) compiles to a recursive relation such as
`reach(x, y)`, the transitive closure of a membership-edge relation. We model a *linear*
recursive query as the least fixed point of a step `F(X) = B ∪ G(X)` on finite sets (Z-sets
that are sets), where `G` distributes over union. Joins with a base relation, such as
`delegate`, are of this form.

Results:
* `lfp_fixed`, `lfp_least`: iterating `F` from `∅` reaches the least fixed point within
  `|U|` steps, for a finite universe `U`.
* `sn_fst`, `seminaive_correct`, `sn_snd_eq_empty_iff`: semi-naive evaluation
  (`R ← R ∪ D`, `D ← G(D) \ R`) computes the same iterates, and its delta is empty exactly
  at the fixed point.
* `incr_insert`, `tc_insert`: **insertions**. Continuing semi-naive from the old fixed point,
  seeded with the new consequences, reaches the new fixed point. `tc_insert_seed` shows the
  seed for transitive closure involves only the new edges.
* `naive_delete_wrong`: **deletions**. Removing every fact with a derivation through a
  deleted edge over-deletes.
* `dred_sound`, `dred_correct`: DRed (delete, then rederive) is correct.
* `nonmonotone_no_fixpoint`: recursion through a negation (such as a deny rule whose match
  depends on the recursive relation) can have no fixed point at all.
-/
import ShepZSet.Basic
import Mathlib.Logic.Relation
import Mathlib.Data.Finset.Prod
import Mathlib.Data.Finset.Card
import Mathlib.Data.Fintype.Basic

noncomputable section

namespace Shep

variable {α : Type*} [DecidableEq α]

/-- A linear recursive step `F(X) = B ∪ G(X)`, with `G` distributing over union. -/
structure LinStep (α : Type*) [DecidableEq α] where
  B : Finset α
  G : Finset α → Finset α
  G_union : ∀ X Y, G (X ∪ Y) = G X ∪ G Y

namespace LinStep

variable (s : LinStep α)

/-- The step function. -/
def F (X : Finset α) : Finset α := s.B ∪ s.G X

theorem G_mono {X Y : Finset α} (h : X ⊆ Y) : s.G X ⊆ s.G Y := by
  have := s.G_union X Y
  rw [Finset.union_eq_right.mpr h] at this
  rw [this]; exact Finset.subset_union_left

theorem F_mono {X Y : Finset α} (h : X ⊆ Y) : s.F X ⊆ s.F Y :=
  Finset.union_subset_union (subset_refl _) (s.G_mono h)

/-- `U` is a finite universe closed under the step. -/
def Bounded (U : Finset α) : Prop := s.B ⊆ U ∧ ∀ X ⊆ U, s.G X ⊆ U

theorem F_bounded {U : Finset α} (hU : s.Bounded U) {X : Finset α} (hX : X ⊆ U) :
    s.F X ⊆ U := Finset.union_subset hU.1 (hU.2 X hX)

/-- Naive iteration from a starting set `L`. -/
def iter (L : Finset α) (k : ℕ) : Finset α := s.F^[k] L

theorem iter_succ (L : Finset α) (k : ℕ) : s.iter L (k + 1) = s.F (s.iter L k) :=
  Function.iterate_succ_apply' _ _ _

theorem iter_zero (L : Finset α) : s.iter L 0 = L := rfl

theorem iter_mono_step {L : Finset α} (hL : L ⊆ s.F L) (k : ℕ) :
    s.iter L k ⊆ s.iter L (k + 1) := by
  induction k with
  | zero => simpa [iter_succ, iter_zero] using hL
  | succ k ih =>
    calc s.iter L (k + 1) = s.F (s.iter L k) := iter_succ s L k
      _ ⊆ s.F (s.iter L (k + 1)) := s.F_mono ih
      _ = s.iter L (k + 1 + 1) := (iter_succ s L (k + 1)).symm

theorem iter_mono {L : Finset α} (hL : L ⊆ s.F L) {k j : ℕ} (h : k ≤ j) :
    s.iter L k ⊆ s.iter L j := by
  induction j, h using Nat.le_induction with
  | base => exact subset_refl _
  | succ j _ ih => exact ih.trans (s.iter_mono_step hL j)

theorem iter_bounded {U : Finset α} (hU : s.Bounded U) {L : Finset α} (hL : L ⊆ U) (k : ℕ) :
    s.iter L k ⊆ U := by
  induction k with
  | zero => exact hL
  | succ k ih => rw [iter_succ]; exact s.F_bounded hU ih

theorem iter_stable {L : Finset α} {k : ℕ} (h : s.iter L k = s.iter L (k + 1)) :
    ∀ j, k ≤ j → s.iter L j = s.iter L k := by
  intro j hj
  induction j, hj using Nat.le_induction with
  | base => rfl
  | succ j _ ih => rw [iter_succ, ih, ← iter_succ, ← h]

/-- The iteration stabilizes within `|U|` steps. -/
theorem exists_stable {U : Finset α} (hU : s.Bounded U) {L : Finset α} (hLU : L ⊆ U)
    (hL : L ⊆ s.F L) : ∃ k ≤ U.card, s.iter L k = s.iter L (k + 1) := by
  by_contra hne
  push Not at hne
  have hgrow : ∀ m, m ≤ U.card + 1 → m ≤ (s.iter L m).card := by
    intro m
    induction m with
    | zero => intro _; exact Nat.zero_le _
    | succ m ih =>
      intro hm
      have h1 := ih (by omega)
      have hss : s.iter L m ⊂ s.iter L (m + 1) :=
        Finset.ssubset_iff_subset_ne.mpr ⟨s.iter_mono_step hL m, hne m (by omega)⟩
      have := Finset.card_lt_card hss
      omega
  have h1 := hgrow (U.card + 1) le_rfl
  have h2 := Finset.card_le_card (s.iter_bounded hU hLU (U.card + 1))
  omega

/-- The limit of iterating from `L`. -/
def limit (U L : Finset α) : Finset α := s.iter L U.card

theorem limit_fixed {U : Finset α} (hU : s.Bounded U) {L : Finset α} (hLU : L ⊆ U)
    (hL : L ⊆ s.F L) : s.F (s.limit U L) = s.limit U L := by
  obtain ⟨k, hk, hst⟩ := s.exists_stable hU hLU hL
  unfold limit
  rw [← iter_succ, s.iter_stable hst _ (by omega), s.iter_stable hst _ hk]

/-- Iterates stay below any prefixed point above the start. -/
theorem iter_le_prefixed {L X : Finset α} (hLX : L ⊆ X) (hX : s.F X ⊆ X) (k : ℕ) :
    s.iter L k ⊆ X := by
  induction k with
  | zero => exact hLX
  | succ k ih => rw [iter_succ]; exact (s.F_mono ih).trans hX

/-- **The least fixed point**, by naive iteration from `∅`. -/
def lfp (U : Finset α) : Finset α := s.limit U ∅

theorem lfp_fixed {U : Finset α} (hU : s.Bounded U) : s.F (s.lfp U) = s.lfp U :=
  s.limit_fixed hU (Finset.empty_subset _) (Finset.empty_subset _)

/-- It is below every prefixed point, so it is the least fixed point. -/
theorem lfp_least (U : Finset α) {X : Finset α} (hX : s.F X ⊆ X) : s.lfp U ⊆ X :=
  s.iter_le_prefixed (Finset.empty_subset _) hX _

theorem lfp_subset_univ {U : Finset α} (hU : s.Bounded U) : s.lfp U ⊆ U :=
  s.iter_bounded hU (Finset.empty_subset _) _

theorem B_subset_lfp {U : Finset α} (hU : s.Bounded U) : s.B ⊆ s.lfp U := by
  rw [← s.lfp_fixed hU]; exact Finset.subset_union_left

theorem G_lfp_subset {U : Finset α} (hU : s.Bounded U) : s.G (s.lfp U) ⊆ s.lfp U := by
  conv_rhs => rw [← s.lfp_fixed hU]
  exact Finset.subset_union_right

/-- Iterating from any start that lies below the least fixed point and below its own
image converges to the least fixed point. -/
theorem limit_eq_lfp {U : Finset α} (hU : s.Bounded U) {L : Finset α} (hLU : L ⊆ U)
    (hL : L ⊆ s.F L) (hLl : L ⊆ s.lfp U) : s.limit U L = s.lfp U := by
  apply Finset.Subset.antisymm
  · exact s.iter_le_prefixed hLl (s.lfp_fixed hU).le _
  · exact s.lfp_least U (s.limit_fixed hU hLU hL).le

/-! ### Semi-naive evaluation -/

/-- One semi-naive step on `(R, D)`: `R ← R ∪ D`, `D ← G(D) \ (R ∪ D)`. -/
def snStep (p : Finset α × Finset α) : Finset α × Finset α :=
  (p.1 ∪ p.2, s.G p.2 \ (p.1 ∪ p.2))

/-- Semi-naive iteration from `L`, seeded with `D₀ = F(L) \ L`. From scratch, `L = ∅` and
`D₀ = B`. -/
def sn (L : Finset α) (k : ℕ) : Finset α × Finset α := s.snStep^[k] (L, s.F L \ L)

/-- The semi-naive state is `(F^k L, F(F^k L) \ F^k L)`. -/
theorem sn_eq {L : Finset α} (hL : L ⊆ s.F L) (k : ℕ) :
    s.sn L k = (s.iter L k, s.F (s.iter L k) \ s.iter L k) := by
  induction k with
  | zero => rfl
  | succ k ih =>
    unfold sn at ih ⊢
    rw [Function.iterate_succ_apply', ih]
    set R := s.iter L k
    have hR : R ⊆ s.F R := by
      have := s.iter_mono_step hL k; rwa [iter_succ] at this
    have hR' : R ∪ (s.F R \ R) = s.F R := by
      rw [Finset.union_sdiff_self_eq_union, Finset.union_eq_right.mpr hR]
    simp only [snStep, Prod.mk.injEq]
    rw [hR', iter_succ]
    refine ⟨rfl, ?_⟩
    -- F(F R) = F R ∪ G(F R \ R)
    have hF : s.F (s.F R) = s.F R ∪ s.G (s.F R \ R) := by
      conv_lhs => rw [← hR']
      unfold F
      rw [s.G_union, ← Finset.union_assoc]
    rw [hF, Finset.union_sdiff_left]

theorem sn_fst {L : Finset α} (hL : L ⊆ s.F L) (k : ℕ) : (s.sn L k).1 = s.iter L k := by
  rw [s.sn_eq hL]

/-- Semi-naive's termination test (an empty delta) holds exactly at a fixed point. -/
theorem sn_snd_eq_empty_iff {L : Finset α} (hL : L ⊆ s.F L) (k : ℕ) :
    (s.sn L k).2 = ∅ ↔ s.F (s.iter L k) = s.iter L k := by
  rw [s.sn_eq hL, Finset.sdiff_eq_empty_iff_subset]
  constructor
  · intro h
    have := s.iter_mono_step hL k
    rw [iter_succ] at this
    exact Finset.Subset.antisymm h this
  · intro h; rw [h]

/-- **Semi-naive evaluation from scratch reaches the least fixed point.** -/
theorem seminaive_correct (U : Finset α) : (s.sn ∅ U.card).1 = s.lfp U := by
  rw [s.sn_fst (Finset.empty_subset _)]; rfl

theorem seminaive_terminates {U : Finset α} (hU : s.Bounded U) : (s.sn ∅ U.card).2 = ∅ := by
  rw [s.sn_snd_eq_empty_iff (Finset.empty_subset _)]
  exact s.lfp_fixed hU

end LinStep

/-! ### Incremental insertion -/

/-- **Insertion.** If the step only grows (`B ⊆ B'`, `G ≤ G'`), continuing semi-naive from the
old fixed point, seeded with `F'(old) \ old`, reaches the new least fixed point. -/
theorem incr_insert (s s' : LinStep α) {U : Finset α} (hU : s.Bounded U) (hU' : s'.Bounded U)
    (hB : s.B ⊆ s'.B) (hG : ∀ X, s.G X ⊆ s'.G X) :
    (s'.sn (s.lfp U) U.card).1 = s'.lfp U := by
  have hFF : ∀ X, s.F X ⊆ s'.F X := fun X => Finset.union_subset_union hB (hG X)
  have hL : s.lfp U ⊆ s'.F (s.lfp U) := by
    conv_lhs => rw [← s.lfp_fixed hU]
    exact hFF _
  have hLl : s.lfp U ⊆ s'.lfp U := s.lfp_least U ((hFF _).trans (s'.lfp_fixed hU').le)
  rw [s'.sn_fst hL]
  exact s'.limit_eq_lfp hU' (s.lfp_subset_univ hU) hL hLl

/-- Insertions only add facts: the old fixed point is contained in the new one. -/
theorem lfp_mono (s s' : LinStep α) {U : Finset α} (hU' : s'.Bounded U)
    (hB : s.B ⊆ s'.B) (hG : ∀ X, s.G X ⊆ s'.G X) : s.lfp U ⊆ s'.lfp U :=
  s.lfp_least U ((Finset.union_subset_union hB (hG _)).trans (s'.lfp_fixed hU').le)

/-! ### Transitive closure (nested group membership) -/

section TC

/-- Relational composition `E ∘ X = {(x, z) | (x, y) ∈ E, (y, z) ∈ X}`. -/
def compose (E X : Finset (α × α)) : Finset (α × α) :=
  ((E ×ˢ X).filter (fun p => p.1.2 = p.2.1)).image (fun p => (p.1.1, p.2.2))

theorem mem_compose {E X : Finset (α × α)} {x z : α} :
    (x, z) ∈ compose E X ↔ ∃ y, (x, y) ∈ E ∧ (y, z) ∈ X := by
  unfold compose
  simp only [Finset.mem_image, Finset.mem_filter, Finset.mem_product, Prod.exists,
    Prod.mk.injEq]
  constructor
  · rintro ⟨a, b, c, d, ⟨⟨h1, h2⟩, rfl⟩, rfl, rfl⟩; exact ⟨b, h1, h2⟩
  · rintro ⟨y, h1, h2⟩; exact ⟨x, y, y, z, ⟨⟨h1, h2⟩, rfl⟩, rfl, rfl⟩

theorem compose_union_right (E X Y : Finset (α × α)) :
    compose E (X ∪ Y) = compose E X ∪ compose E Y := by
  ext ⟨x, z⟩
  simp only [Finset.mem_union, mem_compose]
  constructor
  · rintro ⟨y, h1, h2 | h2⟩
    · exact Or.inl ⟨y, h1, h2⟩
    · exact Or.inr ⟨y, h1, h2⟩
  · rintro (⟨y, h1, h2⟩ | ⟨y, h1, h2⟩)
    · exact ⟨y, h1, Or.inl h2⟩
    · exact ⟨y, h1, Or.inr h2⟩

theorem compose_mono_left {E E' X : Finset (α × α)} (h : E ⊆ E') :
    compose E X ⊆ compose E' X := by
  intro ⟨x, z⟩ hp
  obtain ⟨y, h1, h2⟩ := mem_compose.mp hp
  exact mem_compose.mpr ⟨y, h h1, h2⟩

/-- The step of `reach`: `reach = E ∪ E ∘ reach`. -/
def tcStep (E : Finset (α × α)) : LinStep (α × α) :=
  ⟨E, compose E, compose_union_right E⟩

/-- Every edge's endpoints lie in the node universe `N`. -/
def EdgesIn (E : Finset (α × α)) (N : Finset α) : Prop := ∀ p ∈ E, p.1 ∈ N ∧ p.2 ∈ N

theorem tc_bounded {E : Finset (α × α)} {N : Finset α} (hN : EdgesIn E N) :
    (tcStep E).Bounded (N ×ˢ N) := by
  refine ⟨fun p hp => Finset.mem_product.mpr (hN p hp), fun X hX => ?_⟩
  intro ⟨x, z⟩ hp
  obtain ⟨y, h1, h2⟩ := mem_compose.mp hp
  exact Finset.mem_product.mpr ⟨(hN _ h1).1, (Finset.mem_product.mp (hX h2)).2⟩

/-- `reach E`: the transitive closure of the edge relation, as a least fixed point. -/
def reach (E : Finset (α × α)) (N : Finset α) : Finset (α × α) := (tcStep E).lfp (N ×ˢ N)

/-- The edge relation. -/
def Edge (E : Finset (α × α)) (a b : α) : Prop := (a, b) ∈ E

/-- **`reach` is the transitive closure.** -/
theorem mem_reach {E : Finset (α × α)} {N : Finset α} (hN : EdgesIn E N) {x z : α} :
    (x, z) ∈ reach E N ↔ Relation.TransGen (Edge E) x z := by
  classical
  have hU := tc_bounded hN
  constructor
  · intro h
    let X := (N ×ˢ N).filter (fun p => Relation.TransGen (Edge E) p.1 p.2)
    have hX : (tcStep E).F X ⊆ X := by
      intro ⟨a, c⟩ hp
      rcases Finset.mem_union.mp hp with hp | hp
      · exact Finset.mem_filter.mpr ⟨Finset.mem_product.mpr (hN _ hp),
          Relation.TransGen.single hp⟩
      · obtain ⟨b, h1, h2⟩ := mem_compose.mp hp
        have h2' := Finset.mem_filter.mp h2
        exact Finset.mem_filter.mpr
          ⟨Finset.mem_product.mpr ⟨(hN _ h1).1, (Finset.mem_product.mp h2'.1).2⟩,
            Relation.TransGen.head h1 h2'.2⟩
    exact (Finset.mem_filter.mp ((tcStep E).lfp_least _ hX h)).2
  · intro h
    induction h using Relation.TransGen.head_induction_on with
    | single hab => exact (tcStep E).B_subset_lfp hU hab
    | head hab _ ih =>
      exact (tcStep E).G_lfp_subset hU (mem_compose.mpr ⟨_, hab, ih⟩)

/-- **Insertion for transitive closure**: semi-naive from the old `reach`, over the new edge
set, reaches the new `reach`. -/
theorem tc_insert {E ΔE : Finset (α × α)} {N : Finset α} (hN : EdgesIn (E ∪ ΔE) N) :
    ((tcStep (E ∪ ΔE)).sn (reach E N) (N ×ˢ N).card).1 = reach (E ∪ ΔE) N := by
  have hNE : EdgesIn E N := fun p hp => hN p (Finset.mem_union_left _ hp)
  exact incr_insert (tcStep E) (tcStep (E ∪ ΔE)) (tc_bounded hNE) (tc_bounded hN)
    Finset.subset_union_left (fun X => compose_mono_left Finset.subset_union_left)

/-- **The insertion seed involves only the new edges**: `F'(reach) \ reach ⊆ ΔE ∪ ΔE ∘ reach`.
Later semi-naive rounds join the edges only with the newly derived facts. So the work is
proportional to the new consequences. That can still be much larger than `|ΔE|`: one edge
that joins two groups adds every (member, ancestor) pair. -/
theorem tc_insert_seed {E ΔE : Finset (α × α)} {N : Finset α} (hN : EdgesIn E N) :
    (tcStep (E ∪ ΔE)).F (reach E N) \ reach E N ⊆ ΔE ∪ compose ΔE (reach E N) := by
  have hU := tc_bounded hN
  intro ⟨x, z⟩ hp
  rw [Finset.mem_sdiff] at hp
  obtain ⟨hF, hn⟩ := hp
  rcases Finset.mem_union.mp hF with h | h
  · rcases Finset.mem_union.mp h with h | h
    · exact absurd ((tcStep E).B_subset_lfp hU h) hn
    · exact Finset.mem_union_left _ h
  · obtain ⟨y, h1, h2⟩ := mem_compose.mp h
    rcases Finset.mem_union.mp h1 with h1 | h1
    · exact absurd ((tcStep E).G_lfp_subset hU (mem_compose.mpr ⟨y, h1, h2⟩)) hn
    · exact Finset.mem_union_right _ (mem_compose.mpr ⟨y, h1, h2⟩)

/-! ### Deletion -/

/-- DRed's over-deletion: every fact with *some* derivation that uses a deleted edge,
computed over the old edges and old `reach`. It is itself a linear recursion:
`over = ΔE ∪ ΔE ∘ reach_old ∪ E ∘ over`. -/
def overStep (E ΔE : Finset (α × α)) (N : Finset α) : LinStep (α × α) :=
  ⟨ΔE ∪ compose ΔE (reach E N), compose E, compose_union_right E⟩

def over (E ΔE : Finset (α × α)) (N : Finset α) : Finset (α × α) :=
  (overStep E ΔE N).lfp (N ×ˢ N)

theorem overStep_bounded {E ΔE : Finset (α × α)} {N : Finset α} (hN : EdgesIn E N)
    (hΔ : ΔE ⊆ E) : (overStep E ΔE N).Bounded (N ×ˢ N) := by
  refine ⟨?_, (tc_bounded hN).2⟩
  apply Finset.union_subset
  · exact fun p hp => Finset.mem_product.mpr (hN p (hΔ hp))
  · exact ((compose_mono_left hΔ).trans ((tcStep E).G_lfp_subset (tc_bounded hN))).trans
      ((tcStep E).lfp_subset_univ (tc_bounded hN))

/-- Naive deletion: remove the over-deleted facts and stop. -/
def naiveDelete (E ΔE : Finset (α × α)) (N : Finset α) : Finset (α × α) :=
  reach E N \ over E ΔE N

/-- The three closure properties of `over`. -/
theorem over_props {E ΔE : Finset (α × α)} {N : Finset α} (hN : EdgesIn E N) (hΔ : ΔE ⊆ E) :
    ΔE ⊆ over E ΔE N ∧ compose ΔE (reach E N) ⊆ over E ΔE N ∧
      compose E (over E ΔE N) ⊆ over E ΔE N := by
  have hU := overStep_bounded hN hΔ
  have hB := (overStep E ΔE N).B_subset_lfp hU
  refine ⟨fun p hp => hB (Finset.mem_union_left _ hp),
    fun p hp => hB (Finset.mem_union_right _ hp), (overStep E ΔE N).G_lfp_subset hU⟩

/-- **Over-deletion is sound**: every old fact that wasn't over-deleted is still derivable
from the remaining edges. -/
theorem dred_sound {E ΔE : Finset (α × α)} {N : Finset α} (hN : EdgesIn E N) (hΔ : ΔE ⊆ E) :
    naiveDelete E ΔE N ⊆ reach (E \ ΔE) N := by
  obtain ⟨h1, h2, h3⟩ := over_props hN hΔ
  have hN' : EdgesIn (E \ ΔE) N := fun p hp => hN p (Finset.mem_sdiff.mp hp).1
  intro ⟨x, z⟩ hp
  rw [naiveDelete, Finset.mem_sdiff, mem_reach hN] at hp
  obtain ⟨hpath, hno⟩ := hp
  rw [mem_reach hN']
  revert hno
  induction hpath using Relation.TransGen.head_induction_on with
  | single hab =>
    intro hno
    refine Relation.TransGen.single (Finset.mem_sdiff.mpr ⟨hab, fun hd => hno (h1 hd)⟩)
  | @head a b hab hbz ih =>
    intro hno
    have hab' : (a, b) ∉ ΔE := by
      intro hd
      exact hno (h2 (mem_compose.mpr ⟨b, hd, (mem_reach hN).mpr hbz⟩))
    have hbo : (b, z) ∉ over E ΔE N := fun ho => hno (h3 (mem_compose.mpr ⟨b, hab, ho⟩))
    exact Relation.TransGen.head (Finset.mem_sdiff.mpr ⟨hab, hab'⟩) (ih hbo)

/-- The survivors are closed under one step of the new rules, so semi-naive can resume
from them. -/
theorem dred_survivors_prefixed {E ΔE : Finset (α × α)} {N : Finset α} (hN : EdgesIn E N)
    (hΔ : ΔE ⊆ E) :
    naiveDelete E ΔE N ⊆ (tcStep (E \ ΔE)).F (naiveDelete E ΔE N) := by
  obtain ⟨h1, h2, h3⟩ := over_props hN hΔ
  intro ⟨x, z⟩ hp
  have hp' := Finset.mem_sdiff.mp hp
  have hfix := (tcStep E).lfp_fixed (tc_bounded hN)
  have hr : (x, z) ∈ (tcStep E).F (reach E N) := by
    unfold reach; rw [hfix]; exact hp'.1
  rcases Finset.mem_union.mp hr with h | h
  · exact Finset.mem_union_left _
      (Finset.mem_sdiff.mpr ⟨h, fun hd => hp'.2 (h1 hd)⟩)
  · obtain ⟨y, hxy, hyz⟩ := mem_compose.mp h
    have hxy' : (x, y) ∉ ΔE := fun hd => hp'.2 (h2 (mem_compose.mpr ⟨y, hd, hyz⟩))
    have hyz' : (y, z) ∉ over E ΔE N := fun ho => hp'.2 (h3 (mem_compose.mpr ⟨y, hxy, ho⟩))
    exact Finset.mem_union_right _ (mem_compose.mpr
      ⟨y, Finset.mem_sdiff.mpr ⟨hxy, hxy'⟩, Finset.mem_sdiff.mpr ⟨hyz, hyz'⟩⟩)

/-- **DRed is correct**: after over-deleting, resume semi-naive evaluation over the remaining
edges from the survivors (the rederive phase). The result is the new `reach`. -/
theorem dred_correct {E ΔE : Finset (α × α)} {N : Finset α} (hN : EdgesIn E N)
    (hΔ : ΔE ⊆ E) :
    ((tcStep (E \ ΔE)).sn (naiveDelete E ΔE N) (N ×ˢ N).card).1 = reach (E \ ΔE) N := by
  have hN' : EdgesIn (E \ ΔE) N := fun p hp => hN p (Finset.mem_sdiff.mp hp).1
  have hpre := dred_survivors_prefixed hN hΔ
  rw [(tcStep (E \ ΔE)).sn_fst hpre]
  apply (tcStep (E \ ΔE)).limit_eq_lfp (tc_bounded hN') _ hpre (dred_sound hN hΔ)
  exact (Finset.sdiff_subset).trans ((tcStep E).lfp_subset_univ (tc_bounded hN))

/-- Deletions only remove facts. -/
theorem reach_delete_subset {E ΔE : Finset (α × α)} {N : Finset α} (hN : EdgesIn E N) :
    reach (E \ ΔE) N ⊆ reach E N :=
  lfp_mono (tcStep (E \ ΔE)) (tcStep E) (tc_bounded hN) Finset.sdiff_subset
    (fun _ => compose_mono_left Finset.sdiff_subset)

end TC

/-- The example graph: `0→1`, `1→2`, `0→2`. -/
def exE : Finset (Fin 3 × Fin 3) := {(0, 1), (1, 2), (0, 2)}

/-- The deleted edge: `0→2`. -/
def exΔE : Finset (Fin 3 × Fin 3) := {(0, 2)}

/-- **Naive incremental deletion over-deletes.** In `exE`, delete `0→2`. `(0, 2)` has a
derivation through the deleted edge, so it is over-deleted, but it is still reachable through
`1`. -/
theorem naive_delete_wrong :
    naiveDelete exE exΔE Finset.univ ≠ reach (exE \ exΔE) Finset.univ := by
  intro h
  have hN : EdgesIn exE Finset.univ := fun _ _ => ⟨Finset.mem_univ _, Finset.mem_univ _⟩
  have hN' : EdgesIn (exE \ exΔE) Finset.univ :=
    fun _ _ => ⟨Finset.mem_univ _, Finset.mem_univ _⟩
  have hΔ : exΔE ⊆ exE := by decide
  have hin : ((0 : Fin 3), (2 : Fin 3)) ∈ reach (exE \ exΔE) Finset.univ := by
    rw [mem_reach hN']
    exact Relation.TransGen.head (b := 1) (by unfold Edge; decide)
      (Relation.TransGen.single (by unfold Edge; decide))
  have h02 : ((0 : Fin 3), (2 : Fin 3)) ∈ exΔE := by decide
  have hover : ((0 : Fin 3), (2 : Fin 3)) ∈ over exE exΔE Finset.univ := by
    have := (over_props hN hΔ).1
    exact Finset.mem_of_subset this h02
  have hin2 : ((0 : Fin 3), (2 : Fin 3)) ∈ naiveDelete exE exΔE Finset.univ := by
    rw [h]; exact hin
  exact (Finset.mem_sdiff.mp hin2).2 hover

/-- **Recursion through negation can have no fixed point.** A rule "deny if delegate(g)"
on a cycle back to `g` gives `X = {v} \ X`, which no set satisfies. -/
theorem nonmonotone_no_fixpoint : ¬ ∃ X : Finset Unit, ({()} : Finset Unit) \ X = X := by
  rintro ⟨X, hX⟩
  by_cases h : () ∈ X
  · have := hX ▸ h
    rw [Finset.mem_sdiff] at this
    exact this.2 h
  · apply h
    rw [← hX, Finset.mem_sdiff]
    exact ⟨Finset.mem_singleton_self _, h⟩

/-- Fixed points as Z-sets: the change between nested sets is a set of `+1`s. -/
def finsetZ (A : Finset α) : ZSet α := ∑ a ∈ A, Finsupp.single a 1

theorem finsetZ_sub {A B : Finset α} (h : A ⊆ B) : finsetZ B - finsetZ A = finsetZ (B \ A) := by
  unfold finsetZ
  rw [← Finset.sum_sdiff h]; abel

end Shep
