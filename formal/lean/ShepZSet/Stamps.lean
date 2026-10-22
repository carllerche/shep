/-
# Stamp-based visibility and two-phase commit (design.md §3, §8, §9.1, §9.2, §9.5)

Shep stores facts in **parts**: a single-partition mutation's delta is one part, and a
two-phase commit has one part per participant. Each part carries a **stamp**, a dot. The state
at a version `V` is the sum of the parts whose stamp is in `V` (§9.2: "a fact is visible at a
version exactly when its stamp is in it").

## Results

* `stateAtS_eq_stateAt`: stamp-based visibility is `stateAt` over the merged per-dot deltas
  `mdelta` (a dot's delta is the sum of the parts stamped with it).
* `wf_stateAtS_closed`: **generalizes `wf_stateAt_closed`**. For every causally closed `V`, the
  state at `V` (the parts with stamp in `V`) is valid, under the same three conditions, stated
  for the merged per-dot deltas.
* `wf_add_parts`: condition 2 for a two-phase commit. If each participant's part is a valid
  transition from the state at the decision's context, and parts touch disjoint nodes (they
  live in different partitions), their sum is a valid transition.
* `TwoPC`, `twoPC_atomic`, `twoPC_closed_prepares`, `twoPC_state`: **atomic visibility**.
  Stamping every part with the home's decision dot puts a transaction wholly in or wholly out
  of every version; a causally closed version that holds it also holds every participant's
  prepare.
* `localStamp_not_atomic`: **counterexample**. Stamping each participant's part with a local
  dot (its intent release, whose context even includes the decision) lets a causally closed
  version see one participant's part and not the other's.
-/
import ShepZSet.Causal

noncomputable section
open Classical

namespace Shep
namespace Stamps

variable {α Pt Dot : Type*}

/-- The state at `W` by stamps: the sum of the parts whose stamp is in `W`. -/
def stateAtS (parts : Finset Pt) (stamp : Pt → Dot) (pdelta : Pt → ZSet α)
    (W : Finset Dot) : ZSet α :=
  ∑ x ∈ parts.filter (fun x => stamp x ∈ W), pdelta x

/-- A dot's delta: the sum of the parts stamped with it (all of a two-phase commit's parts,
from every participant, for its decision dot). -/
def mdelta (parts : Finset Pt) (stamp : Pt → Dot) (pdelta : Pt → ZSet α) (d : Dot) :
    ZSet α :=
  ∑ x ∈ parts.filter (fun x => stamp x = d), pdelta x

/-- **Stamp visibility is `stateAt` over the merged deltas.** -/
theorem stateAtS_eq_stateAt (parts : Finset Pt) (stamp : Pt → Dot) (pdelta : Pt → ZSet α)
    (W : Finset Dot) :
    stateAtS parts stamp pdelta W = stateAt (mdelta parts stamp pdelta) W := by
  unfold stateAtS stateAt mdelta
  rw [← Finset.sum_fiberwise_of_maps_to (s := parts.filter (fun x => stamp x ∈ W)) (t := W)
    (g := stamp) (fun x hx => (Finset.mem_filter.mp hx).2)]
  refine Finset.sum_congr rfl (fun d hd => ?_)
  rw [Finset.filter_filter]
  congr 1
  ext x
  simp only [Finset.mem_filter]
  constructor
  · rintro ⟨hx, -, rfl⟩; exact ⟨hx, rfl⟩
  · rintro ⟨hx, rfl⟩; exact ⟨hx, hd, rfl⟩

variable {N Seg V : Type*} {kind : Path Seg → Kind}

/-- **Every causally closed version is a valid state, by stamps.** The state at `W` is the
sum of the parts whose stamp is in `W`. The conditions are those of `wf_stateAt_closed`, for
each dot's merged delta: causality is a strict order; each dot's delta (all its parts) is a
valid transition from the state at its context; concurrent dots touch prefix-incomparable
paths. -/
theorem wf_stateAtS_closed (parts : Finset Pt) (stamp : Pt → Dot)
    (pdelta : Pt → ZSet (Fact N Seg V)) (ctx : Dot → Finset Dot)
    (hirr : ∀ d, d ∉ ctx d)
    (hctx : ∀ d, CausallyClosed ctx (ctx d))
    (hvalid : ∀ d, WF kind (stateAtS parts stamp pdelta (ctx d) +
      mdelta parts stamp pdelta d))
    (hconc : ∀ d e, d ≠ e → d ∉ ctx e → e ∉ ctx d →
      PathDisjoint (mdelta parts stamp pdelta d) (mdelta parts stamp pdelta e))
    (W : Finset Dot) (hW : CausallyClosed ctx W) :
    WF kind (stateAtS parts stamp pdelta W) := by
  rw [stateAtS_eq_stateAt]
  refine wf_stateAt_closed _ ctx hirr hctx (fun d => ?_) hconc W hW
  rw [← stateAtS_eq_stateAt]; exact hvalid d

theorem sum_ne_zero_exists {I : Finset Pt} {f : Pt → ZSet α} {y : α}
    (h : (∑ x ∈ I, f x) y ≠ 0) : ∃ x ∈ I, f x y ≠ 0 := by
  rw [Finsupp.finsetSum_apply] at h
  exact Finset.exists_ne_zero_of_sum_ne_zero h

/-- **A two-phase commit's merged delta is a valid transition** when the state it starts
from is valid, each participant's part is a valid transition from it, and the parts touch
pairwise disjoint nodes (each participant holds different nodes). -/
theorem wf_add_parts {S : ZSet (Fact N Seg V)} (hS : WF kind S) (I : Finset Pt)
    (pdelta : Pt → ZSet (Fact N Seg V)) (hpart : ∀ x ∈ I, WF kind (S + pdelta x))
    (hdisj : ∀ x ∈ I, ∀ y ∈ I, x ≠ y → NodeDisjoint (pdelta x) (pdelta y)) :
    WF kind (S + ∑ x ∈ I, pdelta x) := by
  induction I using Finset.induction_on with
  | empty => simpa using hS
  | @insert a I ha ih =>
    rw [Finset.sum_insert ha, add_comm (pdelta a), ← add_assoc]
    refine wf_concurrent (ih (fun x hx => hpart x (Finset.mem_insert_of_mem hx))
      (fun x hx y hy => hdisj x (Finset.mem_insert_of_mem hx) y (Finset.mem_insert_of_mem hy)))
      (hpart a (Finset.mem_insert_self _ _)) ?_
    intro u v hu hv
    obtain ⟨x, hx, hxu⟩ := sum_ne_zero_exists hu
    exact hdisj x (Finset.mem_insert_of_mem hx) a (Finset.mem_insert_self _ _)
      (fun h => ha (h ▸ hx)) u v hxu hv

/-! ## Two-phase commit: one dot -/

/-- A two-phase commit (§8): its parts (one per participant), the home's decision dot, and
each part's prepare dot. -/
structure TwoPC (Pt Dot : Type*) where
  parts : Finset Pt
  dec : Dot
  prep : Pt → Dot

/-- The design's rules: every part is stamped with the decision dot, and the decision's
context contains every participant's prepare dot. -/
structure TwoPC.Stamped (T : TwoPC Pt Dot) (stamp : Pt → Dot) (ctx : Dot → Finset Dot) :
    Prop where
  stamp_dec : ∀ x ∈ T.parts, stamp x = T.dec
  prep_ctx : ∀ x ∈ T.parts, T.prep x ∈ ctx T.dec

/-- **Atomic visibility.** For every version `W`, a transaction stamped with its decision dot
is wholly visible or wholly invisible. -/
theorem twoPC_atomic {T : TwoPC Pt Dot} {stamp : Pt → Dot} {ctx : Dot → Finset Dot}
    (hT : T.Stamped stamp ctx) (W : Finset Dot) :
    (∀ x ∈ T.parts, stamp x ∈ W) ∨ (∀ x ∈ T.parts, stamp x ∉ W) := by
  by_cases h : T.dec ∈ W
  · exact Or.inl (fun x hx => hT.stamp_dec x hx ▸ h)
  · exact Or.inr (fun x hx => hT.stamp_dec x hx ▸ h)

/-- A causally closed version that holds the transaction holds every participant's prepare,
so every participant's materializer has the prepared part at hand. -/
theorem twoPC_closed_prepares {T : TwoPC Pt Dot} {stamp : Pt → Dot} {ctx : Dot → Finset Dot}
    (hT : T.Stamped stamp ctx) {W : Finset Dot} (hW : CausallyClosed ctx W)
    (hx : ∃ x ∈ T.parts, stamp x ∈ W) : ∀ x ∈ T.parts, T.prep x ∈ W := by
  obtain ⟨x, hx, hxW⟩ := hx
  rw [hT.stamp_dec x hx] at hxW
  exact fun y hy => hW _ hxW (hT.prep_ctx y hy)

/-- **The transaction's contribution to the state at `W`** is all of its parts or none. -/
theorem twoPC_state {T : TwoPC Pt Dot} {stamp : Pt → Dot} {ctx : Dot → Finset Dot}
    (hT : T.Stamped stamp ctx) (pdelta : Pt → ZSet α) (W : Finset Dot) :
    stateAtS T.parts stamp pdelta W = ∑ x ∈ T.parts, pdelta x ∨
      stateAtS T.parts stamp pdelta W = 0 := by
  unfold stateAtS
  rcases twoPC_atomic hT W with h | h
  · left; rw [Finset.filter_true_of_mem h]
  · right; rw [Finset.filter_false_of_mem h, Finset.sum_empty]

/-! ## Counterexample: stamping each participant's part with a local dot

Two participants `A` and `B`, home `A`. Dots: the prepares `pA`, `pB`; the decision `dc`,
whose context is `{pA, pB}`; the intent releases `rA`, `rB`, each with context its own
prepare plus the decision. Part `A` adds fact `false`, part `B` adds fact `true`. If each part
is stamped with its own release, `W = {pA, pB, dc, rA}` is causally closed and shows part `A`
without part `B`. Stamped with `dec`, both parts are visible at `W`. -/

inductive CDot | pA | pB | dc | rA | rB
  deriving DecidableEq

open CDot in
def cctx : CDot → Finset CDot
  | pA => ∅
  | pB => ∅
  | dc => {pA, pB}
  | rA => {pA, dc, pB}
  | rB => {pB, dc, pA}

/-- Part `false` is `A`'s, part `true` is `B`'s; each is stamped with its own release. -/
def localStamp : Bool → CDot
  | false => .rA
  | true => .rB

def cexTx : TwoPC Bool CDot where
  parts := {false, true}
  dec := .dc
  prep b := if b then .pB else .pA

def cexPart (b : Bool) : ZSet Bool := Finsupp.single b 1

/-- **Local stamps break atomic visibility.** The causal structure is a strict order with
closed contexts, the decision's context holds both prepares, and each release's context even
holds the decision. Still, the causally closed `W = {pA, pB, dc, rA}` shows `A`'s part and
not `B`'s, so the state at `W` is neither all of the transaction nor none of it. Stamping both
parts with the decision dot makes them visible together. -/
theorem localStamp_not_atomic :
    let W : Finset CDot := {.pA, .pB, .dc, .rA}
    (∀ d, d ∉ cctx d) ∧ (∀ d, CausallyClosed cctx (cctx d)) ∧
      (∀ x ∈ cexTx.parts, cexTx.prep x ∈ cctx cexTx.dec) ∧
      (∀ x ∈ cexTx.parts, cexTx.dec ∈ cctx (localStamp x)) ∧
      CausallyClosed cctx W ∧
      localStamp false ∈ W ∧ localStamp true ∉ W ∧
      stateAtS cexTx.parts localStamp cexPart W ≠ ∑ x ∈ cexTx.parts, cexPart x ∧
      stateAtS cexTx.parts localStamp cexPart W ≠ 0 ∧
      cexTx.Stamped (fun _ => .dc) cctx ∧
      stateAtS cexTx.parts (fun _ => .dc) cexPart W = ∑ x ∈ cexTx.parts, cexPart x := by
  intro W
  have hfil : (({false, true} : Finset Bool).filter
      (fun x => localStamp x ∈ ({.pA, .pB, .dc, .rA} : Finset CDot))) = {false} := by decide
  have hst : stateAtS cexTx.parts localStamp cexPart W = Finsupp.single false 1 := by
    simp only [stateAtS, cexTx, W, hfil, Finset.sum_singleton, cexPart]
  refine ⟨?_, ?_, ?_, ?_, ?_, by simp [W, localStamp], by simp [W, localStamp], ?_, ?_, ?_, ?_⟩
  · intro d; cases d <;> simp [cctx]
  · intro d e he x hx
    cases d <;> simp [cctx] at he <;> rcases he with rfl | rfl | rfl <;>
      simp_all [cctx] <;> tauto
  · intro x _; cases x <;> simp [cexTx, cctx]
  · intro x _; cases x <;> simp [cexTx, localStamp, cctx]
  · intro d hd x hx
    simp only [W, Finset.mem_insert, Finset.mem_singleton] at hd
    rcases hd with rfl | rfl | rfl | rfl <;> simp [cctx, W] at hx ⊢ <;> tauto
  · rw [hst]; intro h
    have := congrArg (fun f => f true) h
    simp [cexTx, cexPart] at this
  · rw [hst]; intro h
    have := congrArg (fun f => f false) h
    simp at this
  · exact ⟨fun _ _ => rfl, fun x _ => by cases x <;> simp [cexTx, cctx]⟩
  · simp [stateAtS, cexTx, W]

end Stamps
end Shep
