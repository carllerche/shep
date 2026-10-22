/-
# State as a sum of deltas; order independence (design.md §9, §10.4, §12)

A version is a finite set of dots. Each dot has a delta (a Z-set of facts) and a
context (the set of dots in its causal past). The state at a version is the sum of the
deltas of its dots.
-/
import ShepZSet.Basic

noncomputable section
open Classical

namespace Shep

variable {Dot α : Type*}

/-- The state at version `V`: the sum of the deltas of its dots (§9, §10.4). -/
def stateAt (delta : Dot → ZSet α) (V : Finset Dot) : ZSet α := ∑ d ∈ V, delta d

/-- Applying deltas one after another in the order given by a list. -/
def applyList (delta : Dot → ZSet α) (l : List Dot) : ZSet α := (l.map delta).sum

/-- **Order independence.** Any two application orders that are permutations of each other
give the same integrated state. -/
theorem applyList_perm (delta : Dot → ZSet α) {l₁ l₂ : List Dot} (h : l₁.Perm l₂) :
    applyList delta l₁ = applyList delta l₂ :=
  (h.map delta).sum_eq

/-- Any duplicate-free enumeration of a version's dots (in particular, any linear extension
of the causal order, and also any non-causal order) integrates to `stateAt V`. -/
theorem applyList_eq_stateAt (delta : Dot → ZSet α) {l : List Dot} {V : Finset Dot}
    (hnd : l.Nodup) (hV : l.toFinset = V) : applyList delta l = stateAt delta V := by
  subst hV
  unfold applyList stateAt
  rw [List.sum_toFinset _ hnd]

/-- Causal structure: each dot has a context. A version is *causally closed* if it contains
the context of each of its dots. -/
def CausallyClosed (ctx : Dot → Finset Dot) (V : Finset Dot) : Prop :=
  ∀ d ∈ V, ctx d ⊆ V

/-- A list is a *causal order* if every dot's context appears strictly earlier. -/
def IsCausalOrder (ctx : Dot → Finset Dot) (l : List Dot) : Prop :=
  ∀ i (h : i < l.length), ∀ c ∈ ctx l[i], c ∈ l.take i

/-- Every prefix of a causal order is causally closed, so the intermediate states seen by a
causal replayer / view maintainer (§10.4, §12) are states at causally closed versions. -/
theorem causalOrder_prefix_closed (ctx : Dot → Finset Dot) (l : List Dot)
    (h : IsCausalOrder ctx l) (k : ℕ) : CausallyClosed ctx (l.take k).toFinset := by
  intro d hd c hc
  rw [List.mem_toFinset] at hd ⊢
  obtain ⟨i, hi, rfl⟩ := List.getElem_of_mem hd
  have hi' : i < l.length := by
    have := List.length_take_le k l; simp at hi; omega
  have hik : i < k := by simp at hi; omega
  have := h i hi' c (by simpa [List.getElem_take] using hc)
  have hsub : (l.take i) <+: (l.take k) := by
    exact (List.take_prefix_take_left (by omega))
  exact hsub.subset this

/-! ## Non-causal intermediate states need not be sets

Dot `a` creates a fact `f` (`+f`); dot `b`, made with `a` in its context, removes it (`-f`).
The final state `{a, b}` is the (valid) empty set, but applying `b` before `a` passes through
a state with weight `-1` on `f`. This is why observable states (snapshots, view frontiers,
replay checkpoints) must be causally closed. -/

/-- The two-dot counterexample. Dots are `Bool`: `false` = `a`, `true` = `b`. -/
def cexDelta : Bool → ZSet Unit
  | false => Finsupp.single () 1
  | true => -Finsupp.single () 1

def cexCtx : Bool → Finset Bool
  | false => ∅
  | true => {false}

theorem noncausal_intermediate_not_set :
    -- the final state is a set (in fact empty)
    stateAt cexDelta {false, true} = 0 ∧
    -- `[false, true]` is a causal order, `[true, false]` is not
    CausallyClosed cexCtx {false} ∧ ¬ CausallyClosed cexCtx {true} ∧
    -- the non-causal prefix `[true]` integrates to a non-set
    ¬ IsSet (applyList cexDelta [true]) := by
  refine ⟨?_, ?_, ?_, ?_⟩
  · simp [stateAt, cexDelta]
  · intro d hd; simp at hd; subst hd; simp [cexCtx]
  · intro h
    have := h true (by simp)
    simp [cexCtx] at this
  · intro h
    rcases h () with h | h <;> simp [applyList, cexDelta] at h

end Shep
