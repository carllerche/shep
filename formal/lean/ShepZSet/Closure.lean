/-
# Closing versions, and the frontier (design.md §9.4, §9.7)

## Closure

A version is a finite set of dots. `step V = V ∪ ⋃_{x ∈ V} ctx x` adds the context of every dot.
The closure iterates `step` until nothing changes. The committed dots `C` are finite and every
context names committed dots, so the loop stays inside `C`.

* `closure_fixed`, `closure_terminates`: the loop is stable after at most `|C|` rounds.
* `closure_closed`, `subset_closure`, `closure_least`: the result is **the least causally
  closed version containing `V`**.
* `closure_mono`: closure is monotone.

## The causal-frontier formula (`cf`)

Dots are `(partition, index)`, indices from `0`. `cf p n` is the union of the contexts of
entries `0 … n-1` of `p`, and `pre V p` is `V`'s contiguous prefix in `p`. Dots of `V` beyond
the prefix are its **extras**.

* `stepDVV_eq`: one lookup of `cf p (pre V p)` per partition, plus each extra's own context, is
  exactly `step`. So the closure computed with `cf` is the closure.
* `stepPre` is the §9.7 formula as written: only `cf p (V[p])`, nothing for extras.
  `iterate_stepPre`: it computes the same closure **when every extra of the starting version
  already has its context in it** (`Covered`), and contexts are causally closed. Covered
  versions include prefix-only versions and causally closed tokens, and unions of them
  (`covered_prefixOnly`, `covered_of_closed`, `covered_union`): that is §9.7's start, "the
  indexes to read, at least the request's `at_least`".
* `stepPre_unsound`: without that, the formula stops at a version that isn't causally closed.

## The frontier

§9.7 computes the frontier from the applied entries `A` by dropping, repeatedly, any entry
whose context isn't covered by what remains.

* `frontier_closed`, `frontier_subset`, `frontier_greatest`: the result is **the largest
  causally closed subset** of `A`.
* `frontier_mono`: as materializers apply more, the frontier only grows.
* `closed_biUnion`: frontiers built by joining causally closed versions stay closed.
-/
import ShepZSet.Integrate
import Mathlib.Data.Finset.Card
import Mathlib.Data.Finset.Lattice.Fold
import Mathlib.Order.Interval.Finset.Nat

noncomputable section
open Classical

namespace Shep
namespace Closure

variable {α : Type*} [DecidableEq α]

/-! ## Iterating to a fixpoint inside a finite set -/

/-- An inflationary map that stays inside the finite set `C` is stable after `|C|` rounds. -/
theorem iterate_infl_fixed (f : Finset α → Finset α) (C : Finset α)
    (hinf : ∀ V, V ⊆ C → V ⊆ f V) (hin : ∀ V, V ⊆ C → f V ⊆ C) {V : Finset α}
    (hV : V ⊆ C) : f (f^[C.card] V) = f^[C.card] V := by
  have hC : ∀ k, f^[k] V ⊆ C := by
    intro k; induction k with
    | zero => exact hV
    | succ k ih => rw [Function.iterate_succ_apply']; exact hin _ ih
  have key : ∀ k, f (f^[k] V) ≠ f^[k] V → k + 1 ≤ (f^[k + 1] V).card := by
    intro k
    induction k with
    | zero =>
      intro hne
      rw [Function.iterate_one]
      have hss : V ⊂ f V := Finset.ssubset_iff_subset_ne.mpr ⟨hinf V hV, fun h => hne h.symm⟩
      have := Finset.card_lt_card hss
      omega
    | succ k ih =>
      intro hne
      have hne' : f (f^[k] V) ≠ f^[k] V := by
        intro h
        apply hne
        rw [Function.iterate_succ_apply', h, h]
      have h1 := ih hne'
      have hss : f^[k + 1] V ⊂ f^[k + 1 + 1] V := by
        rw [Function.iterate_succ_apply' f (k + 1)]
        exact Finset.ssubset_iff_subset_ne.mpr ⟨hinf _ (hC _), fun h => hne h.symm⟩
      have := Finset.card_lt_card hss
      omega
  by_contra hne
  have h1 := key _ hne
  have h2 := Finset.card_le_card (hC (C.card + 1))
  omega

/-- A deflationary map is stable on `A` after `|A|` rounds. -/
theorem iterate_defl_fixed (f : Finset α → Finset α) (hdef : ∀ V, f V ⊆ V) (A : Finset α) :
    f (f^[A.card] A) = f^[A.card] A := by
  have key : ∀ k, f (f^[k] A) ≠ f^[k] A → (f^[k + 1] A).card + k + 1 ≤ A.card := by
    intro k
    induction k with
    | zero =>
      intro hne
      rw [Function.iterate_one]
      have hss : f A ⊂ A := Finset.ssubset_iff_subset_ne.mpr ⟨hdef A, hne⟩
      have := Finset.card_lt_card hss
      simp only [Function.iterate_zero, id] at *
      omega
    | succ k ih =>
      intro hne
      have hne' : f (f^[k] A) ≠ f^[k] A := by
        intro h
        apply hne
        rw [Function.iterate_succ_apply', h, h]
      have h1 := ih hne'
      have hss : f^[k + 1 + 1] A ⊂ f^[k + 1] A := by
        rw [Function.iterate_succ_apply' f (k + 1)]
        exact Finset.ssubset_iff_subset_ne.mpr ⟨hdef _, hne⟩
      have := Finset.card_lt_card hss
      omega
  by_contra hne
  have := key _ hne
  omega

/-! ## Closure -/

variable (ctx : α → Finset α)

/-- One round of closing: add the context of every dot. -/
def step (V : Finset α) : Finset α := V ∪ V.biUnion ctx

/-- Close `V` inside the committed dots `C`: `|C|` rounds of `step`. -/
def closure (C V : Finset α) : Finset α := (step ctx)^[C.card] V

variable {ctx}

theorem subset_step (V : Finset α) : V ⊆ step ctx V := Finset.subset_union_left

theorem step_mono {V W : Finset α} (h : V ⊆ W) : step ctx V ⊆ step ctx W :=
  Finset.union_subset_union h (Finset.biUnion_subset_biUnion_of_subset_left _ h)

theorem step_eq_iff_closed {V : Finset α} : step ctx V = V ↔ CausallyClosed ctx V := by
  constructor
  · intro h x hx y hy
    rw [← h]; exact Finset.mem_union_right _ (Finset.mem_biUnion.mpr ⟨x, hx, hy⟩)
  · intro h
    apply le_antisymm _ (subset_step V)
    apply Finset.union_subset le_rfl
    intro y hy
    obtain ⟨x, hx, hy⟩ := Finset.mem_biUnion.mp hy
    exact h x hx hy

theorem step_subset_of_closed {V W : Finset α} (hW : CausallyClosed ctx W) (h : V ⊆ W) :
    step ctx V ⊆ W := (step_mono h).trans (step_eq_iff_closed.mpr hW).le

/-- The committed dots are closed under contexts: every dot a context names is committed. -/
theorem step_subset_C {C V : Finset α} (hC : CausallyClosed ctx C) (hV : V ⊆ C) :
    step ctx V ⊆ C := step_subset_of_closed hC hV

theorem iterate_subset_C {C V : Finset α} (hC : CausallyClosed ctx C) (hV : V ⊆ C) :
    ∀ k, (step ctx)^[k] V ⊆ C := by
  intro k; induction k with
  | zero => exact hV
  | succ k ih => rw [Function.iterate_succ_apply']; exact step_subset_C hC ih

/-- **The loop terminates**: after `|C|` rounds, a round adds nothing. -/
theorem closure_fixed {C V : Finset α} (hC : CausallyClosed ctx C) (hV : V ⊆ C) :
    step ctx (closure ctx C V) = closure ctx C V :=
  iterate_infl_fixed (step ctx) C (fun V _ => subset_step V) (fun _ h => step_subset_C hC h) hV

/-- Stated as termination: some round `k ≤ |C|` adds nothing, and from then on the loop's
value never changes. -/
theorem closure_terminates {C V : Finset α} (hC : CausallyClosed ctx C) (hV : V ⊆ C) :
    ∃ k ≤ C.card, ∀ m ≥ k, (step ctx)^[m] V = closure ctx C V := by
  refine ⟨C.card, le_rfl, fun m hm => ?_⟩
  obtain ⟨d, rfl⟩ := Nat.exists_eq_add_of_le hm
  induction d with
  | zero => rfl
  | succ d ih =>
    rw [← Nat.add_assoc, Function.iterate_succ_apply', ih (by omega)]
    exact closure_fixed hC hV

theorem subset_closure (C V : Finset α) : V ⊆ closure ctx C V := by
  unfold closure
  induction C.card with
  | zero => exact le_rfl
  | succ k ih => rw [Function.iterate_succ_apply']; exact ih.trans (subset_step _)

/-- **The closure is causally closed.** -/
theorem closure_closed {C V : Finset α} (hC : CausallyClosed ctx C) (hV : V ⊆ C) :
    CausallyClosed ctx (closure ctx C V) :=
  step_eq_iff_closed.mp (closure_fixed hC hV)

/-- **The closure is the least causally closed version containing `V`.** -/
theorem closure_least {C V W : Finset α} (hW : CausallyClosed ctx W) (hVW : V ⊆ W) :
    closure ctx C V ⊆ W := by
  unfold closure
  induction C.card with
  | zero => exact hVW
  | succ k ih => rw [Function.iterate_succ_apply']; exact step_subset_of_closed hW ih

/-- **Closure is monotone.** -/
theorem closure_mono {C V V' : Finset α} (hC : CausallyClosed ctx C) (hV' : V' ⊆ C)
    (h : V ⊆ V') : closure ctx C V ⊆ closure ctx C V' :=
  closure_least (closure_closed hC hV') (h.trans (subset_closure C V'))

/-- A closed version is its own closure. -/
theorem closure_of_closed {C V : Finset α} (hV : CausallyClosed ctx V) :
    closure ctx C V = V :=
  le_antisymm (closure_least hV le_rfl) (subset_closure C V)

/-! ## Frontiers -/

/-- **Joining causally closed versions gives a causally closed version**, for any finite
family (`causallyClosed_union` is the binary case). -/
theorem closed_biUnion {ι : Type*} (I : Finset ι) (V : ι → Finset α)
    (h : ∀ i ∈ I, CausallyClosed ctx (V i)) : CausallyClosed ctx (I.biUnion V) := by
  intro x hx y hy
  obtain ⟨i, hi, hxi⟩ := Finset.mem_biUnion.mp hx
  exact Finset.mem_biUnion.mpr ⟨i, hi, h i hi x hxi hy⟩

variable (ctx)

/-- One round of §9.7's frontier computation: drop every entry whose context isn't covered by
what remains. -/
def drop (A : Finset α) : Finset α := A.filter (fun x => ctx x ⊆ A)

/-- The frontier of the applied entries `A`. -/
def frontier (A : Finset α) : Finset α := (drop ctx)^[A.card] A

variable {ctx}

theorem drop_subset (A : Finset α) : drop ctx A ⊆ A := Finset.filter_subset _ _

theorem frontier_subset (A : Finset α) : frontier ctx A ⊆ A := by
  unfold frontier
  induction A.card with
  | zero => exact le_rfl
  | succ k ih => rw [Function.iterate_succ_apply']; exact (drop_subset _).trans ih

/-- **The frontier is causally closed.** -/
theorem frontier_closed (A : Finset α) : CausallyClosed ctx (frontier ctx A) := by
  have hfix : drop ctx (frontier ctx A) = frontier ctx A :=
    iterate_defl_fixed (drop ctx) drop_subset A
  intro x hx
  rw [← hfix] at hx
  exact (Finset.mem_filter.mp hx).2

/-- **The frontier is the largest causally closed set of applied entries.** -/
theorem frontier_greatest {A W : Finset α} (hW : CausallyClosed ctx W) (hWA : W ⊆ A) :
    W ⊆ frontier ctx A := by
  unfold frontier
  induction A.card with
  | zero => exact hWA
  | succ k ih =>
    rw [Function.iterate_succ_apply']
    intro x hx
    exact Finset.mem_filter.mpr ⟨ih hx, (hW x hx).trans ih⟩

/-- **Frontiers only advance.** If the applied entries grow, the frontier grows. -/
theorem frontier_mono {A A' : Finset α} (h : A ⊆ A') : frontier ctx A ⊆ frontier ctx A' :=
  frontier_greatest (frontier_closed A) ((frontier_subset A).trans h)

/-! ## The causal-frontier formula over DVVs -/

variable {P : Type*} [DecidableEq P] {cx : P × ℕ → Finset (P × ℕ)}

/-- `cf p n`: the union of the contexts of entries `0 … n-1` of partition `p` (§9.4). -/
def cf (cx : P × ℕ → Finset (P × ℕ)) (p : P) (n : ℕ) : Finset (P × ℕ) :=
  (Finset.range n).biUnion (fun j => cx (p, j))

omit [DecidableEq P] in
theorem exists_not_mem (V : Finset (P × ℕ)) (p : P) : ∃ j, (p, j) ∉ V := by
  refine ⟨(V.image Prod.snd).sup id + 1, fun h => ?_⟩
  exact Nat.not_succ_le_self _ (Finset.le_sup (f := id) (Finset.mem_image_of_mem Prod.snd h))

/-- `V`'s contiguous prefix in `p`: the first index of `p` that `V` lacks. -/
def pre (V : Finset (P × ℕ)) (p : P) : ℕ := Nat.find (exists_not_mem V p)

theorem mem_of_lt_pre {V : Finset (P × ℕ)} {p : P} {j : ℕ} (h : j < pre V p) : (p, j) ∈ V := by
  by_contra h'; exact Nat.find_min (exists_not_mem V p) h h'

theorem pre_mono {V W : Finset (P × ℕ)} (h : V ⊆ W) (p : P) : pre V p ≤ pre W p :=
  Nat.find_mono (fun _ hw hv => hw (h hv))

/-- The extras: dots of `V` beyond its prefix in their partition. -/
def extras (V : Finset (P × ℕ)) : Finset (P × ℕ) := V.filter (fun x => pre V x.1 ≤ x.2)

/-- The causal frontiers of the partitions `V` names, at `V`'s prefixes. -/
def cfs (cx : P × ℕ → Finset (P × ℕ)) (V : Finset (P × ℕ)) : Finset (P × ℕ) :=
  (V.image Prod.fst).biUnion (fun p => cf cx p (pre V p))

/-- One round, as computed: one `cf` lookup per partition, plus each extra's own context. -/
def stepDVV (cx : P × ℕ → Finset (P × ℕ)) (V : Finset (P × ℕ)) : Finset (P × ℕ) :=
  V ∪ cfs cx V ∪ (extras V).biUnion cx

/-- One round of §9.7 as written: `cf p (V[p])` for every partition `p` in `V`. -/
def stepPre (cx : P × ℕ → Finset (P × ℕ)) (V : Finset (P × ℕ)) : Finset (P × ℕ) :=
  V ∪ cfs cx V

theorem mem_cfs {V : Finset (P × ℕ)} {y : P × ℕ} :
    y ∈ cfs cx V ↔ ∃ x ∈ V, x.2 < pre V x.1 ∧ y ∈ cx x := by
  constructor
  · intro hy
    obtain ⟨p, -, hy⟩ := Finset.mem_biUnion.mp hy
    obtain ⟨j, hj, hy⟩ := Finset.mem_biUnion.mp hy
    rw [Finset.mem_range] at hj
    exact ⟨(p, j), mem_of_lt_pre hj, hj, hy⟩
  · rintro ⟨x, hx, hlt, hy⟩
    exact Finset.mem_biUnion.mpr ⟨x.1, Finset.mem_image_of_mem _ hx,
      Finset.mem_biUnion.mpr ⟨x.2, Finset.mem_range.mpr hlt, hy⟩⟩

theorem cfs_subset_step (V : Finset (P × ℕ)) : cfs cx V ⊆ step cx V := by
  intro y hy
  obtain ⟨x, hx, _, hy⟩ := mem_cfs.mp hy
  exact Finset.mem_union_right _ (Finset.mem_biUnion.mpr ⟨x, hx, hy⟩)

/-- **Computing a round with `cf` is exact**: the prefix lookups plus the extras' contexts are
the contexts of every dot of `V`. -/
theorem stepDVV_eq (V : Finset (P × ℕ)) : stepDVV cx V = step cx V := by
  apply le_antisymm
  · apply Finset.union_subset (Finset.union_subset (subset_step V) (cfs_subset_step V))
    intro y hy
    obtain ⟨x, hx, hy⟩ := Finset.mem_biUnion.mp hy
    exact Finset.mem_union_right _ (Finset.mem_biUnion.mpr ⟨x, (Finset.mem_filter.mp hx).1, hy⟩)
  · apply Finset.union_subset (Finset.subset_union_left.trans Finset.subset_union_left)
    intro y hy
    obtain ⟨x, hx, hy⟩ := Finset.mem_biUnion.mp hy
    by_cases hlt : x.2 < pre V x.1
    · exact Finset.mem_union_left _ (Finset.mem_union_right _
        (mem_cfs.mpr ⟨x, hx, hlt, hy⟩))
    · exact Finset.mem_union_right _ (Finset.mem_biUnion.mpr
        ⟨x, Finset.mem_filter.mpr ⟨hx, not_lt.mp hlt⟩, hy⟩)

/-- Every extra of `V` already has its context in `V`. -/
def Covered (cx : P × ℕ → Finset (P × ℕ)) (V : Finset (P × ℕ)) : Prop :=
  ∀ x ∈ extras V, cx x ⊆ V

theorem stepPre_eq_of_covered {V : Finset (P × ℕ)} (h : Covered cx V) :
    stepPre cx V = step cx V := by
  rw [← stepDVV_eq]
  unfold stepPre stepDVV
  refine (Finset.union_eq_left.mpr ?_).symm
  intro y hy
  obtain ⟨x, hx, hy⟩ := Finset.mem_biUnion.mp hy
  exact Finset.mem_union_left _ (h x hx hy)

/-- With causally closed contexts, a round of the §9.7 formula keeps every extra covered. -/
theorem covered_stepPre (hctx : ∀ d, CausallyClosed cx (cx d)) {V : Finset (P × ℕ)}
    (h : Covered cx V) : Covered cx (stepPre cx V) := by
  intro x hx
  obtain ⟨hxW, hpre⟩ := Finset.mem_filter.mp hx
  rcases Finset.mem_union.mp hxW with hxV | hxc
  · have hxe : x ∈ extras V :=
      Finset.mem_filter.mpr ⟨hxV, (pre_mono Finset.subset_union_left x.1).trans hpre⟩
    exact (h x hxe).trans Finset.subset_union_left
  · obtain ⟨z, hz, hlt, hxz⟩ := mem_cfs.mp hxc
    intro y hy
    exact Finset.mem_union_right _ (mem_cfs.mpr ⟨z, hz, hlt, hctx z x hxz hy⟩)

/-- **The §9.7 formula computes the closure** when the starting version is covered and
contexts are causally closed: every round equals a round of `step`. -/
theorem iterate_stepPre (hctx : ∀ d, CausallyClosed cx (cx d)) {V : Finset (P × ℕ)}
    (h : Covered cx V) : ∀ k, (stepPre cx)^[k] V = (step cx)^[k] V := by
  have hcov : ∀ k, Covered cx ((stepPre cx)^[k] V) := by
    intro k; induction k with
    | zero => exact h
    | succ k ih => rw [Function.iterate_succ_apply']; exact covered_stepPre hctx ih
  intro k; induction k with
  | zero => rfl
  | succ k ih =>
    rw [Function.iterate_succ_apply', Function.iterate_succ_apply', ← ih,
      stepPre_eq_of_covered (hcov k)]

/-- A version with no extras (prefixes only, such as "the indexes to read") is covered. -/
theorem covered_prefixOnly {V : Finset (P × ℕ)} (h : extras V = ∅) : Covered cx V := by
  intro x hx; rw [h] at hx; simp at hx

/-- A causally closed version (such as a causal token) is covered. -/
theorem covered_of_closed {V : Finset (P × ℕ)} (h : CausallyClosed cx V) : Covered cx V :=
  fun x hx => h x (Finset.mem_filter.mp hx).1

/-- Covered versions are closed under union. -/
theorem covered_union {V W : Finset (P × ℕ)} (hV : Covered cx V) (hW : Covered cx W) :
    Covered cx (V ∪ W) := by
  intro x hx
  obtain ⟨hxu, hpre⟩ := Finset.mem_filter.mp hx
  rcases Finset.mem_union.mp hxu with h | h
  · exact (hV x (Finset.mem_filter.mpr ⟨h, (pre_mono Finset.subset_union_left x.1).trans hpre⟩)).trans
      Finset.subset_union_left
  · exact (hW x (Finset.mem_filter.mpr ⟨h, (pre_mono Finset.subset_union_right x.1).trans hpre⟩)).trans
      Finset.subset_union_right

/-! ### Counterexample: the formula as written, from an uncovered start

Partitions `false` and `true`. `V = {(false, 1)}` has an extra, `(false, 1)`, whose context
is `{(true, 0)}`. Its prefix in `false` is empty, so `cf false 0 = ∅` and `true` isn't named:
a round of the formula adds nothing, but `V` isn't causally closed. Every context is causally
closed. -/

def cexCx : Bool × ℕ → Finset (Bool × ℕ)
  | (false, 1) => {(true, 0)}
  | _ => ∅

theorem stepPre_unsound :
    let V : Finset (Bool × ℕ) := {(false, 1)}
    (∀ d, CausallyClosed cexCx (cexCx d)) ∧ stepPre cexCx V = V ∧
      ¬ CausallyClosed cexCx V ∧ ¬ Covered cexCx V := by
  intro V
  have hpre : pre V false = 0 := by
    rw [pre, Nat.find_eq_zero]; simp [V]
  have hpre' : pre V true = 0 := by
    rw [pre, Nat.find_eq_zero]; simp [V]
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro d x hx y hy
    rcases d with ⟨_ | _, _ | _ | _⟩ <;> simp [cexCx] at hx
    subst hx; simp [cexCx] at hy
  · unfold stepPre cfs
    apply Finset.union_eq_left.mpr
    intro y hy
    obtain ⟨p, hp, hy⟩ := Finset.mem_biUnion.mp hy
    cases p <;> simp [cf, hpre, hpre'] at hy
  · intro h
    have := h (false, 1) (by simp [V]) (by simp [cexCx] : (true, 0) ∈ cexCx (false, 1))
    simp [V] at this
  · intro h
    have := h (false, 1) (Finset.mem_filter.mpr ⟨by simp [V], by simp [hpre]⟩)
      (by simp [cexCx] : (true, 0) ∈ cexCx (false, 1))
    simp [V] at this

end Closure
end Shep
