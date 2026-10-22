/-
# Patch merging is PutPut through a version's lens (design.md §8, §4.4 law 5)

Restatement-round obligation 7 (§18). In a mutation, "all updates to one node merge in statement
order into one patch"; the merged patch then goes through the version's lens once, and checks
and `update write` policies run once, on the merged post-state (§8, §4.0 item 6). We prove that
this equals applying the patches through the lens one after another, whenever the lens is
lawful (`Adapters.Laws`, including composed chains that satisfy the laws, `comp_laws`).

## The model

The adapter model of `Adapters.lean`: rows `F → V`, patches `F → Option V`, `mergeP p₁ p₂`
(`p₁ ⊕ p₂`, the second wins), and `Lens.write`. `mergeAll` merges a mutation's patches in
statement order; `writeSeq` applies them through the lens in sequence.

## Results

* `merge_eq_seq`: **merging, then writing through a lawful lens, equals writing the patches in
  sequence**, provided each patch is a valid write on the view it would see in the sequence
  (`Steps`): the hypothesis under which law 5 is stated. That includes the case where the last
  write is rejected: then the merged write is rejected too.
* `mergeAll_perm`: patches with **pairwise disjoint fields** merge to the same patch in any
  order. §8 makes assigning one field twice an error, so a mutation's patches to one node are
  disjoint, and statement order only matters through `Steps`.
* `merge_accepts_invalid_prefix` (**design note**): the converse needs `Steps`. With a lawful
  lens that rejects writes producing invalid rows (a cross-field check `a ≤ b`), writing
  `a := 5` then `b := 10` from `(0, 0)` rejects at the first step, while the merged patch is
  accepted. §8's semantics is the merged one (checks run once, on the merged state), so this is
  the intended answer; the sequence is only a model of it when every intermediate view is valid.
* For chains with a lossy write below a cross-field adapter, law 5 itself fails
  (`chain_putPut_fails`), and merging differs from the sequence. §4.4 already requires such
  chains to be re-proven or the lossy write replaced by `reject`.
-/
import ShepZSet.AdapterCounterexamples

noncomputable section
open Classical

namespace Shep.Adapters
namespace PatchMerge

variable {F V : Type*}

/-- The empty patch. -/
def emptyP : Patch F V := fun _ => none

/-- Merge a mutation's patches to one node, in statement order (later wins). -/
def mergeAll : List (Patch F V) → Patch F V
  | [] => emptyP
  | p :: ps => mergeP p (mergeAll ps)

@[simp] theorem mergeP_empty (p : Patch F V) : mergeP p emptyP = p := by
  funext f; simp [mergeP, emptyP]

@[simp] theorem mergeAll_single (p : Patch F V) : mergeAll [p] = p := by
  simp [mergeAll]

/-- Write the patches through the lens one after another. -/
def writeSeq (L : Lens F V) : List (Patch F V) → Row F V → Option (Row F V)
  | [], x => some x
  | p :: ps, x => (L.write p x).bind (writeSeq L ps)

/-- Each patch is a valid write on the view it sees in the sequence, and every write but the
last is accepted. -/
def Steps (L : Lens F V) (SY : Set (Row F V)) (norm : F → V → V) :
    List (Patch F V) → Row F V → Row F V → Prop
  | [], _, _ => True
  | [p], _, y => applyP p y ∈ SY
  | p :: p' :: ps, x, y => applyP p y ∈ SY ∧
      ∃ q, L.put p x = some q ∧ Steps L SY norm (p' :: ps) (applyP q x) (applyP (normP norm p) y)

/-- **Merging, then writing, equals writing in sequence**, through any lawful lens. -/
theorem merge_eq_seq {L : Lens F V} {SX SY : Set (Row F V)} {wr : F → Set F}
    {norm : F → V → V} (hL : Laws L SX SY wr norm) :
    ∀ (ps : List (Patch F V)) (x y : Row F V), ps ≠ [] → x ∈ SX → L.get x = some y →
      Steps L SY norm ps x y → L.write (mergeAll ps) x = writeSeq L ps x := by
  intro ps
  induction ps with
  | nil => intro _ _ h; exact absurd rfl h
  | cons p ps ih =>
    intro x y _ hx hy hs
    match ps, ih, hs with
    | [], _, _ =>
      simp only [mergeAll_single, writeSeq]
      cases L.write p x <;> rfl
    | p' :: ps', ih, ⟨hp, q, hq, hs'⟩ =>
      have hw : L.write p x = some (applyP q x) := by simp [Lens.write, hq]
      have hx₁ : applyP q x ∈ SX := hL.totalWrite x hx y hy p hp q hq
      have hy₁ := hL.putGet x hx y hy p hp q hq
      rw [show mergeAll (p :: p' :: ps') = mergeP p (mergeAll (p' :: ps')) from rfl,
        Laws.putPut_state hL hx hy hp hw,
        ih (applyP q x) _ (List.cons_ne_nil _ _) hx₁ hy₁ hs']
      simp [writeSeq, hw]

/-! ## Disjoint patches merge in any order -/

/-- Two patches name disjoint fields. -/
def Disj (p q : Patch F V) : Prop := ∀ f, p f = none ∨ q f = none

theorem disj_symm {p q : Patch F V} (h : Disj p q) : Disj q p := fun f => (h f).symm

theorem mergeAll_none {ps : List (Patch F V)} {f : F} (h : ∀ p ∈ ps, p f = none) :
    mergeAll ps f = none := by
  induction ps with
  | nil => rfl
  | cons p ps ih =>
    simp only [mergeAll, mergeP]
    rw [ih (fun q hq => h q (List.mem_cons_of_mem _ hq)), h p List.mem_cons_self]
    rfl

theorem mergeAll_some {ps : List (Patch F V)} (hd : ps.Pairwise Disj) {p : Patch F V}
    (hp : p ∈ ps) {f : F} {v : V} (hv : p f = some v) : mergeAll ps f = some v := by
  induction ps with
  | nil => simp at hp
  | cons q ps ih =>
    rw [List.pairwise_cons] at hd
    simp only [mergeAll, mergeP]
    rcases List.mem_cons.mp hp with rfl | hp
    · have : mergeAll ps f = none := mergeAll_none (fun r hr => by
        rcases hd.1 r hr f with h | h
        · rw [hv] at h; cases h
        · exact h)
      rw [this, hv]; rfl
    · rw [ih hd.2 hp]; rfl

/-- **Disjoint patches merge to the same patch in any order.** -/
theorem mergeAll_perm {ps ps' : List (Patch F V)} (hd : ps.Pairwise Disj) (hp : ps.Perm ps') :
    mergeAll ps = mergeAll ps' := by
  have hd' : ps'.Pairwise Disj := (hp.pairwise_iff (fun h => disj_symm h)).mp hd
  funext f
  by_cases h : ∃ p ∈ ps, ∃ v, p f = some v
  · obtain ⟨p, hmem, v, hv⟩ := h
    rw [mergeAll_some hd hmem hv, mergeAll_some hd' (hp.mem_iff.mp hmem) hv]
  · have h1 : ∀ p ∈ ps, p f = none := fun p hm =>
      Option.eq_none_iff_forall_ne_some.mpr (fun v hv => h ⟨p, hm, v, hv⟩)
    rw [mergeAll_none h1, mergeAll_none (fun p hm => h1 p (hp.mem_iff.mpr hm))]

/-! ## The converse needs valid intermediate views -/

/-- A lens that keeps the row and rejects any write whose result breaks the row check. -/
def guardLens (S : Set (Row F V)) : Lens F V where
  get x := some x
  put p x := if applyP p x ∈ S then some p else none

/-- It is lawful, with no normalization, for any valid-row set. -/
theorem guardLens_laws (S : Set (Row F V)) :
    Laws (guardLens S) S S (fun g => {g}) exactNorm := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro x hx y hy; simp only [guardLens, Option.some.injEq] at hy; subst hy; exact hx
  · intro x _ y _ p _ q hq
    simp only [guardLens] at hq
    split_ifs at hq with h
    simp only [Option.some.injEq] at hq; subst hq; exact h
  · intro x p h; simp [guardLens] at h
  · intro x _ y hy p _ q hq
    simp only [guardLens, Option.some.injEq] at hy hq ⊢
    subst hy
    split_ifs at hq
    simp only [Option.some.injEq] at hq; subst hq
    simp
  · intro x _ y _ p₁ p₂ _ q₁ hq₁
    simp only [guardLens] at hq₁ ⊢
    split_ifs at hq₁
    simp only [Option.some.injEq] at hq₁; subst hq₁
    rw [applyP_merge]
    split_ifs <;> rfl
  · intro x p q hq f hf
    simp only [guardLens] at hq
    split_ifs at hq
    simp only [Option.some.injEq] at hq; subst hq
    exact ⟨f, hf, rfl⟩

/-- Rows over fields `a` (`false`) and `b` (`true`) with the check `a ≤ b`. -/
def abValid : Set (Row Bool ℤ) := {x | x false ≤ x true}

def p₁ : Patch Bool ℤ := fun f => if f = false then some 5 else none
def p₂ : Patch Bool ℤ := fun f => if f = true then some 10 else none

theorem merge_accepts_invalid_prefix :
    Laws (guardLens abValid) abValid abValid (fun g => {g}) exactNorm ∧
    Disj p₁ p₂ ∧
    -- in sequence, `a := 5` is rejected: `(5, 0)` breaks `a ≤ b`
    writeSeq (guardLens abValid) [p₁, p₂] (fun _ => 0) = none ∧
    -- merged, the write is accepted: `(5, 10)`
    (∃ r, (guardLens abValid).write (mergeAll [p₁, p₂]) (fun _ => 0) = some r ∧
      r false = 5 ∧ r true = 10) ∧
    -- and `Steps` fails: the intermediate view is invalid
    ¬ Steps (guardLens abValid) abValid exactNorm [p₁, p₂] (fun _ => 0) (fun _ => 0) := by
  refine ⟨guardLens_laws _, ?_, ?_, ?_, ?_⟩
  · intro f; cases f <;> simp [p₁, p₂]
  · simp [writeSeq, Lens.write, guardLens, abValid, applyP, p₁]
  · have hm : applyP (mergeP p₁ p₂) (fun _ => (0 : ℤ)) ∈ abValid := by
      simp [abValid, applyP, mergeP, p₁, p₂]
    refine ⟨applyP (mergeP p₁ p₂) (fun _ => 0), ?_, by simp [applyP, mergeP, p₁, p₂],
      by simp [applyP, mergeP, p₁, p₂]⟩
    simp only [Lens.write, guardLens, mergeAll, mergeP_empty, hm, ↓reduceIte, Option.map_some]
  · simp [Steps, abValid, applyP, p₁]

end PatchMerge
end Shep.Adapters
