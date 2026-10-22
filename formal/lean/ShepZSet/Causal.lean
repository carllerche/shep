/-
# Causally closed versions are well-formed states (design.md §9, §10.4, §12)

The design reads states only at causally closed versions (snapshots, view frontiers,
replay checkpoints). We prove that the state at *every* causally closed version is
well-formed, under three explicit side conditions:

1. the causal order is a strict order: `d ∉ ctx d`, and contexts are causally closed;
2. each dot's delta is a valid transition from the state at its own context;
3. **concurrent dots don't conflict**: on each node they touch prefix-incomparable paths
   (implied by "concurrent dots touch disjoint nodes", which Postgres row conflicts give:
   a node is one row, §10.2 "Concurrency is still per node").

Condition 3 cannot be dropped (`Facts.concurrent_same_field_not_set`).
-/
import ShepZSet.Facts

noncomputable section
open Classical

namespace Shep

variable {N Seg V Dot : Type*} {kind : Path Seg → Kind}

/-- Concurrent deltas *conflict* if, on some node, they touch prefix-comparable paths (the
same path, or one above the other). This is weaker than "touch the same node": two
transactions may write different, unrelated paths of one node (or different edges of one
source) without conflicting. -/
def PathDisjoint (Δ₁ Δ₂ : ZSet (Fact N Seg V)) : Prop :=
  ∀ n p q v w, Δ₁ (n, p, v) ≠ 0 → Δ₂ (n, q, w) ≠ 0 → ¬ (p <+: q ∨ q <+: p)

theorem NodeDisjoint.pathDisjoint {Δ₁ Δ₂ : ZSet (Fact N Seg V)} (h : NodeDisjoint Δ₁ Δ₂) :
    PathDisjoint Δ₁ Δ₂ :=
  fun _ _ _ _ _ h₁ h₂ _ => h _ _ h₁ h₂ rfl

/-- Well-formedness is local to pairs of prefix-related keys of one node. -/
theorem wf_of_local {S : ZSet (Fact N Seg V)}
    (h : ∀ n p q, p <+: q → ∃ T : ZSet (Fact N Seg V), WF kind T ∧
      ∀ v, S (n, p, v) = T (n, p, v) ∧ S (n, q, v) = T (n, q, v)) :
    WF kind S := by
  refine ⟨?_, ?_, ?_⟩
  · intro ⟨n, p, v⟩
    obtain ⟨T, hT, he⟩ := h n p p (List.prefix_refl p)
    rw [(he v).1]; exact hT.isSet _
  · intro n p v w hk hv hw
    obtain ⟨T, hT, he⟩ := h n p p (List.prefix_refl p)
    rw [(he v).1] at hv; rw [(he w).1] at hw; exact hT.fd n p v w hk hv hw
  · intro n p q v w hv hw hpq
    obtain ⟨T, hT, he⟩ := h n p q hpq.1
    rw [(he v).1] at hv; rw [(he w).2] at hw; exact hT.tree n p q v w hv hw hpq

theorem wf_zero : WF kind (0 : ZSet (Fact N Seg V)) :=
  ⟨isSet_zero, fun _ _ _ _ _ h => absurd rfl h, fun _ _ _ _ _ h => absurd rfl h⟩

/-- A delta *touches* key `(n, p)` or `(n, q)`. -/
def Touches (Δ : ZSet (Fact N Seg V)) (n : N) (p q : Path Seg) : Prop :=
  ∃ v, Δ (n, p, v) ≠ 0 ∨ Δ (n, q, v) ≠ 0

theorem not_touches {Δ : ZSet (Fact N Seg V)} {n : N} {p q : Path Seg}
    (h : ¬ Touches Δ n p q) (v : V) : Δ (n, p, v) = 0 ∧ Δ (n, q, v) = 0 := by
  constructor <;> by_contra h' <;> exact h ⟨v, by tauto⟩

/-- **The state at any causally closed version is well-formed**, given that each delta is a
valid transition from the state at its context and concurrent deltas don't conflict. -/
theorem wf_stateAt_closed (delta : Dot → ZSet (Fact N Seg V)) (ctx : Dot → Finset Dot)
    (hirr : ∀ d, d ∉ ctx d)
    (hctx : ∀ d, CausallyClosed ctx (ctx d))
    (hvalid : ∀ d, WF kind (stateAt delta (ctx d) + delta d))
    (hconc : ∀ d e, d ≠ e → d ∉ ctx e → e ∉ ctx d → PathDisjoint (delta d) (delta e))
    (W : Finset Dot) (hW : CausallyClosed ctx W) : WF kind (stateAt delta W) := by
  apply wf_of_local
  intro n p q hpq
  have hcmp : ∀ a b, (a = p ∨ a = q) → (b = p ∨ b = q) → (a <+: b ∨ b <+: a) := by
    rintro a b (rfl | rfl) (rfl | rfl)
    · exact Or.inl (List.prefix_refl _)
    · exact Or.inl hpq
    · exact Or.inr hpq
    · exact Or.inl (List.prefix_refl _)
  set D := W.filter (fun d => Touches (delta d) n p q) with hD
  by_cases hDe : D = ∅
  · -- no dot in `W` touches these keys
    refine ⟨0, wf_zero, fun v => ?_⟩
    simp only [stateAt, Finsupp.finsetSum_apply, Finsupp.coe_zero, Pi.zero_apply]
    constructor <;>
    · apply Finset.sum_eq_zero
      intro d hd
      by_cases ht : Touches (delta d) n p q
      · have : d ∈ D := by rw [hD, Finset.mem_filter]; exact ⟨hd, ht⟩
        rw [hDe] at this; simp at this
      · first | exact (not_touches ht v).1 | exact (not_touches ht v).2
  · -- the causally last dot touching these keys
    obtain ⟨m, hmD, hmax⟩ := Finset.exists_max_image D (fun d => (ctx d).card)
      (Finset.nonempty_iff_ne_empty.mpr hDe)
    have hmW : m ∈ W := (Finset.mem_filter.mp hmD).1
    have hmT : Touches (delta m) n p q := (Finset.mem_filter.mp hmD).2
    -- every other dot of `W` touching them is in `m`'s context
    have hin : ∀ e ∈ D, e ≠ m → e ∈ ctx m := by
      intro e heD hne
      by_contra hem
      by_cases hme : m ∈ ctx e
      · have hsub : ctx m ⊆ ctx e := hctx e m hme
        have hss : ctx m ⊂ ctx e := by
          refine Finset.ssubset_iff_subset_ne.mpr ⟨hsub, ?_⟩
          intro heq; exact hirr m (heq ▸ hme)
        have := Finset.card_lt_card hss
        have := hmax e heD
        omega
      · obtain ⟨v, hv⟩ := (Finset.mem_filter.mp heD).2
        obtain ⟨v', hv'⟩ := hmT
        have hpd := hconc e m hne hem hme
        rcases hv with hv | hv <;> rcases hv' with hv' | hv'
        · exact hpd _ _ _ _ _ hv hv' (hcmp p p (Or.inl rfl) (Or.inl rfl))
        · exact hpd _ _ _ _ _ hv hv' (hcmp p q (Or.inl rfl) (Or.inr rfl))
        · exact hpd _ _ _ _ _ hv hv' (hcmp q p (Or.inr rfl) (Or.inl rfl))
        · exact hpd _ _ _ _ _ hv hv' (hcmp q q (Or.inr rfl) (Or.inr rfl))
    refine ⟨stateAt delta (ctx m) + delta m, hvalid m, fun v => ?_⟩
    have hsplit : W = insert m (ctx m) ∪ (W \ insert m (ctx m)) := by
      rw [Finset.union_sdiff_of_subset]
      exact Finset.insert_subset hmW (hW m hmW)
    have hrest : ∀ e ∈ W \ insert m (ctx m), ¬ Touches (delta e) n p q := by
      intro e he ht
      rw [Finset.mem_sdiff, Finset.mem_insert, not_or] at he
      exact he.2.2 (hin e (Finset.mem_filter.mpr ⟨he.1, ht⟩) he.2.1)
    simp only [stateAt, Finsupp.add_apply, Finsupp.finsetSum_apply]
    constructor
    · conv_lhs => rw [hsplit]
      rw [Finset.sum_union Finset.disjoint_sdiff, Finset.sum_insert (hirr m),
        Finset.sum_eq_zero (fun e he => (not_touches (hrest e he) v).1), add_zero, add_comm]
    · conv_lhs => rw [hsplit]
      rw [Finset.sum_union Finset.disjoint_sdiff, Finset.sum_insert (hirr m),
        Finset.sum_eq_zero (fun e he => (not_touches (hrest e he) v).2), add_zero, add_comm]

/-- Merging version tokens (§9.4) preserves causal closure. -/
theorem causallyClosed_union {ctx : Dot → Finset Dot} {V₁ V₂ : Finset Dot}
    (h₁ : CausallyClosed ctx V₁) (h₂ : CausallyClosed ctx V₂) :
    CausallyClosed ctx (V₁ ∪ V₂) := by
  intro d hd
  rcases Finset.mem_union.mp hd with h | h
  · exact (h₁ d h).trans Finset.subset_union_left
  · exact (h₂ d h).trans Finset.subset_union_right

end Shep
