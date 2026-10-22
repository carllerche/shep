/-
# Adapter counterexamples (design.md §4.2–§4.4)

1. **A lossy write below a cross-field adapter breaks the composed laws.** Each step is
   lawful, but the chain fails PutGet-with-normalization (no normalization function of the
   old version's view exists) and PutPut. So §4.4's re-check of composed chains is
   necessary, for laws 4 and 5, exactly when a step below has a lossy write.
2. **The composed representable set** is larger than `R_B ∩ get_B⁻¹(R_A)` when `B`
   normalizes. The formula is sound (`rep_comp_of`) but not exact.
3. **`add` for edge writes** (§4.2 table, "replace or add") violates PutGet and PutPut.
4. **The name split** (§4.2 table) violates PutGet and GetPut as written. It's a lossy write
   with a normalization, and its representable set is "`first` contains no space".
-/
import ShepZSet.AdapterLibrary

noncomputable section
open Classical

namespace Shep.Adapters

/-- `Laws.putPut` implies the design's state-level PutPut. -/
theorem Laws.putPut_state {F V : Type*} {L : Lens F V} {SX SY : Set (Row F V)}
    {wr : F → Set F} {norm : F → V → V} (hL : Laws L SX SY wr norm) {x : Row F V}
    (hx : x ∈ SX) {y : Row F V} (hy : L.get x = some y) {p₁ p₂ : Patch F V}
    (hp₁ : applyP p₁ y ∈ SY) {x₁ : Row F V} (hw : L.write p₁ x = some x₁) :
    L.write (mergeP p₁ p₂) x = L.write p₂ x₁ := by
  simp only [Lens.write] at hw ⊢
  cases hq₁ : L.put p₁ x with
  | none => simp [hq₁] at hw
  | some q₁ =>
    simp only [hq₁, Option.map_some, Option.some.injEq] at hw
    subst hw
    rw [hL.putPut x hx y hy p₁ p₂ hp₁ q₁ hq₁]
    cases L.put p₂ (applyP q₁ x) <;> simp [applyP_merge]

/-! ## 1. A lossy write below a cross-field adapter -/

/-- Fields: an old `total`, and the newer `sub` (subtotal) and `tax`. -/
inductive Fld | total | tax | sub
  deriving DecidableEq

open Fld

/-- v3 (the head) storage types: `total` is unused (0), `sub ≥ 0` (a tightening), any `tax`. -/
def T3 : Fld → Set ℤ
  | total => {0}
  | sub => {v | 0 ≤ v}
  | tax => Set.univ

/-- v2 rows: `total` unused, any `sub`, any `tax`. -/
def S2 : Set (Row Fld ℤ) := Typed (Function.update T3 sub Set.univ)

theorem mem_S2 (y : Row Fld ℤ) : y ∈ S2 ↔ y total = 0 := by
  constructor
  · intro h; have := h total; simpa [T3] using this
  · intro h f; cases f <;> simp [T3, h]

/-- The v3 → v2 step: tightening `sub ≥ 0` with a lossy write `clamp` (normalization
`max 0`). A built-in, single-field adapter. -/
def B3 : Lens Fld ℤ := fieldLens sub id (fun v => some (max 0 v))

theorem B3_laws : Laws B3 (Typed T3) S2 (fun h => {h}) (fieldNorm sub (fun v => max 0 v)) :=
  fieldLens_laws (f₀ := sub) (T := T3) (Tx := T3 sub) (Ty := Set.univ) rfl
    ⟨fun _ _ => trivial,
     fun v _ s hs => by
      simp only [Option.some.injEq] at hs; subst hs
      simp only [T3, Set.mem_ofPred_eq]; omega,
     fun v _ s hs => by simp only [Option.some.injEq] at hs; subst hs; rfl⟩

/-- v1 views (seeing only `total`): `sub` and `tax` read as 0. -/
def S1 : Set (Row Fld ℤ) := {z | z sub = 0 ∧ z tax = 0}

/-- The v2 → v1 step: `total = sub + tax`. Writing `total` sets `sub := total − tax`. An
exact (lossless) cross-field adapter. -/
def A1 : Lens Fld ℤ where
  get y := some (fun f => match f with
    | total => y sub + y tax
    | tax => 0
    | sub => 0)
  put p y := some (fun f => match f with
    | sub => (p total).map (fun t => t - y tax)
    | tax => none
    | total => none)

theorem A1_laws :
    Laws A1 S2 S1 (fun f => match f with | total => {sub} | _ => ∅) exactNorm := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro y _ z hz; simp only [A1, Option.some.injEq] at hz; subst hz; exact ⟨rfl, rfl⟩
  · intro y hy z _ p _ q hq
    simp only [A1, Option.some.injEq] at hq; subst hq
    rw [mem_S2] at hy ⊢; simp [applyP, hy]
  · intro y p h; simp [A1] at h
  · intro y _ z hz p hp q hq
    simp only [A1, Option.some.injEq] at hz hq; subst hz hq
    obtain ⟨hs, ht⟩ := hp
    simp only [A1, Option.some.injEq]
    funext f
    cases f
    · rcases Option.eq_none_or_eq_some (p total) with h | ⟨t, h⟩ <;> simp [applyP, h]
    · simp only [applyP] at ht ⊢; simpa using ht.symm
    · simp only [applyP] at hs ⊢; simpa using hs.symm
  · intro y _ z hz p₁ p₂ _ q₁ hq₁
    simp only [A1, Option.some.injEq] at hz hq₁; subst hz hq₁
    simp only [A1, Option.map_some, Option.some.injEq]
    funext f
    cases f
    · rfl
    · rfl
    · simp only [mergeP, applyP, Option.getD_none]
      rcases Option.eq_none_or_eq_some (p₂ total) with h2 | ⟨t2, h2⟩ <;>
        rcases Option.eq_none_or_eq_some (p₁ total) with h1 | ⟨t1, h1⟩ <;> simp [h1, h2]
  · intro y p q hq f hf
    simp only [A1, Option.some.injEq] at hq; subst hq
    cases f <;> simp [domP] at hf
    exact ⟨total, by simpa [domP] using hf, rfl⟩

/-- **The composed chain has no normalization.** Two v3 rows look the same to v1
(`total = 5`). The same v1 write (`total := 2`) reads back as 5 on one and 2 on the other,
because the clamp's effect depends on `tax`, which v1 can't see. So no function of v1's view
can serve as the composed `norm`. -/
theorem chain_putGet_no_norm :
    ¬ ∃ N : Row Fld ℤ → Row Fld ℤ, ∀ x ∈ Typed T3, ∀ z, (A1 ⨾ B3).get x = some z →
      ∀ p, applyP p z ∈ S1 → ∀ x₁, (A1 ⨾ B3).write p x = some x₁ →
        (A1 ⨾ B3).get x₁ = some (N (applyP p z)) := by
  rintro ⟨N, hN⟩
  let x : Row Fld ℤ := fun f => match f with | total => 0 | sub => 0 | tax => 5
  let x' : Row Fld ℤ := fun f => match f with | total => 0 | sub => 3 | tax => 2
  let z : Row Fld ℤ := fun f => match f with | total => 5 | sub => 0 | tax => 0
  let p : Patch Fld ℤ := fun f => match f with | total => some 2 | _ => none
  have hx : x ∈ Typed T3 := by intro f; cases f <;> simp [x, T3]
  have hx' : x' ∈ Typed T3 := by intro f; cases f <;> simp [x', T3]
  have hz : (A1 ⨾ B3).get x = some z := by
    simp only [Lens.comp, B3, fieldLens, A1, Option.bind_some, Option.some.injEq]
    funext f; cases f <;> simp [x, z]
  have hz' : (A1 ⨾ B3).get x' = some z := by
    simp only [Lens.comp, B3, fieldLens, A1, Option.bind_some, Option.some.injEq]
    funext f; cases f <;> simp [x', z]
  have hp : applyP p z ∈ S1 := by simp [S1, applyP, p, z]
  let x₁ : Row Fld ℤ := fun f => match f with | total => 0 | sub => 0 | tax => 5
  let x₁' : Row Fld ℤ := fun f => match f with | total => 0 | sub => 0 | tax => 2
  have hw : (A1 ⨾ B3).write p x = some x₁ := by
    simp only [Lens.write, Lens.comp, B3, fieldLens, A1, Option.bind_some]
    simp only [p, Option.map_some, Option.some.injEq]
    funext f; cases f <;> simp [applyP, x, x₁]
  have hw' : (A1 ⨾ B3).write p x' = some x₁' := by
    simp only [Lens.write, Lens.comp, B3, fieldLens, A1, Option.bind_some]
    simp only [p, Option.map_some, Option.some.injEq]
    funext f; cases f <;> simp [applyP, x', x₁']
  have e1 := hN x hx z hz p hp x₁ hw
  have e2 := hN x' hx' z hz' p hp x₁' hw'
  rw [← e2] at e1
  simp only [Lens.comp, B3, fieldLens, A1, Option.bind_some, Option.some.injEq] at e1
  have := congrFun e1 total
  simp [x₁, x₁'] at this

/-- v1' views, seeing `total` and `tax`. -/
def S1' : Set (Row Fld ℤ) := {z | z sub = 0}

/-- A second exact cross-field step, where v1 sees `total` and `tax`. Writing `tax` keeps
`total` fixed by adjusting `sub`. -/
def A2 : Lens Fld ℤ where
  get y := some (fun f => match f with
    | total => y sub + y tax
    | tax => y tax
    | sub => 0)
  put p y := some (fun f => match f with
    | sub => if (p total).isSome ∨ (p tax).isSome then
        some ((p total).getD (y sub + y tax) - (p tax).getD (y tax)) else none
    | tax => p tax
    | total => none)

theorem A2_laws : Laws A2 S2 S1'
    (fun f => match f with | total => {sub} | tax => {sub, tax} | sub => ∅) exactNorm := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro y _ z hz; simp only [A2, Option.some.injEq] at hz; subst hz; rfl
  · intro y hy z _ p _ q hq
    simp only [A2, Option.some.injEq] at hq; subst hq
    rw [mem_S2] at hy ⊢; simp [applyP, hy]
  · intro y p h; simp [A2] at h
  · intro y _ z hz p hp q hq
    simp only [A2, Option.some.injEq] at hz hq; subst hz hq
    simp only [A2, Option.some.injEq, normP_exact]
    funext f
    cases f
    · rcases Option.eq_none_or_eq_some (p total) with h1 | ⟨t, h1⟩ <;>
        rcases Option.eq_none_or_eq_some (p tax) with h2 | ⟨w, h2⟩ <;> simp [applyP, h1, h2]
    · rcases Option.eq_none_or_eq_some (p tax) with h2 | ⟨w, h2⟩ <;> simp [applyP, h2]
    · simp only [S1', Set.mem_ofPred_eq, applyP] at hp ⊢; simpa using hp.symm
  · intro y _ z hz p₁ p₂ _ q₁ hq₁
    simp only [A2, Option.some.injEq] at hz hq₁; subst hz hq₁
    simp only [A2, Option.map_some, Option.some.injEq]
    funext f
    cases f <;> simp only [mergeP, applyP]
    · rcases Option.eq_none_or_eq_some (p₂ tax) with h2 | ⟨t2, h2⟩ <;>
        rcases Option.eq_none_or_eq_some (p₁ tax) with h1 | ⟨t1, h1⟩ <;> simp
    · rcases Option.eq_none_or_eq_some (p₂ total) with h3 | ⟨u2, h3⟩ <;>
      rcases Option.eq_none_or_eq_some (p₁ total) with h4 | ⟨u1, h4⟩ <;>
      rcases Option.eq_none_or_eq_some (p₂ tax) with h2 | ⟨t2, h2⟩ <;>
      rcases Option.eq_none_or_eq_some (p₁ tax) with h1 | ⟨t1, h1⟩ <;>
      simp [h1, h2, h3, h4]
  · intro y p q hq f hf
    simp only [A2, Option.some.injEq] at hq; subst hq
    cases f
    · simp [domP] at hf
    · refine ⟨tax, by simpa [domP] using hf, by simp⟩
    · simp only [domP, Set.mem_ofPred_eq] at hf
      split_ifs at hf with h
      · rcases h with h | h
        · exact ⟨total, by simpa [domP] using h, rfl⟩
        · exact ⟨tax, by simpa [domP] using h, by simp⟩
      · simp at hf

/-- The witness: `sub = 0`, `tax = 5` (so `total` reads as 5). -/
def cx : Row Fld ℤ := fun f => match f with | total => 0 | sub => 0 | tax => 5
/-- First write: `total := 2`. -/
def cp₁ : Patch Fld ℤ := fun f => match f with | total => some 2 | _ => none
/-- Second write: `tax := 0`. -/
def cp₂ : Patch Fld ℤ := fun f => match f with | tax => some 0 | _ => none
def cx₁ : Row Fld ℤ := fun f => match f with | total => 0 | sub => 0 | tax => 5
def cx₂ : Row Fld ℤ := fun f => match f with | total => 0 | sub => 5 | tax => 0
def cx₃ : Row Fld ℤ := fun f => match f with | total => 0 | sub => 2 | tax => 0

/-- **The composed chain fails PutPut.** Writing `total := 2` and then `tax := 0` differs from
writing both at once. The clamp on the first write changes the state the second write
reads. Both steps satisfy every law (`A2_laws`, `B3_laws`). -/
theorem chain_putPut_fails :
    cx ∈ Typed T3 ∧
      (A2 ⨾ B3).write cp₁ cx = some cx₁ ∧ (A2 ⨾ B3).write cp₂ cx₁ = some cx₂ ∧
      (A2 ⨾ B3).write (mergeP cp₁ cp₂) cx = some cx₃ ∧ cx₂ ≠ cx₃ := by
  refine ⟨?_, ?_, ?_, ?_, ?_⟩
  · intro f; cases f <;> simp [cx, T3]
  · simp [Lens.write, Lens.comp, B3, fieldLens, A2, cp₁, cx]
    funext f; cases f <;> simp [applyP, cx, cx₁]
  · simp [Lens.write, Lens.comp, B3, fieldLens, A2, cp₂, cx₁]
    funext f; cases f <;> simp [applyP, cx₁, cx₂]
  · simp [Lens.write, Lens.comp, B3, fieldLens, A2, mergeP, cp₁, cp₂, cx]
    funext f; cases f <;> simp [applyP, cx, cx₃]
  · intro h; have := congrFun h sub; simp [cx₂, cx₃] at this

/-- Hence the composed chain satisfies the laws for no footprint and no normalization. -/
theorem chain_not_lawful (wr : Fld → Set Fld) (norm : Fld → ℤ → ℤ) :
    ¬ Laws (A2 ⨾ B3) (Typed T3) S1' wr norm := by
  intro hL
  obtain ⟨hx, hw₁, hw₂, hw₃, hne⟩ := chain_putPut_fails
  let y : Row Fld ℤ := fun f => match f with | total => 5 | tax => 5 | sub => 0
  have hy : (A2 ⨾ B3).get cx = some y := by
    simp only [Lens.comp, B3, fieldLens, A2, Option.bind_some, Option.some.injEq]
    funext f; cases f <;> simp [cx, y]
  have hp₁ : applyP cp₁ y ∈ S1' := by simp [S1', applyP, cp₁, y]
  have := hL.putPut_state hx hy hp₁ (p₂ := cp₂) hw₁
  rw [hw₃, hw₂] at this
  exact hne (Option.some.inj this).symm

/-! ## 2. The composed representable set is larger than `R_B ∩ get_B⁻¹(R_A)` -/

/-- An old version that reads a value narrowed to `≤ 10`. -/
def narrow10 : Lens Unit ℤ := fieldLens () (fun m => min m 10) some

/-- A head that tightened to `≥ 15`, with a lossy write that raises values to 15. -/
def raise15 : Lens Unit ℤ := fieldLens () id (fun v => some (max v 15))

theorem raise15_laws :
    Laws raise15 (Typed (fun _ => {v | 15 ≤ v}))
      (Typed (Function.update (fun _ => {v : ℤ | 15 ≤ v}) () Set.univ)) (fun h => {h})
      (fieldNorm () (fun v => max v 15)) :=
  fieldLens_laws (f₀ := ()) (T := fun _ => {v : ℤ | 15 ≤ v}) (Tx := {v : ℤ | 15 ≤ v})
    (Ty := Set.univ) rfl
    ⟨fun _ _ => trivial,
     fun v _ s hs => by simp only [Option.some.injEq] at hs; subst hs; simp,
     fun v _ s hs => by simp only [Option.some.injEq] at hs; subst hs; rfl⟩

/-- At stored value 15: the composition represents it (reading 10 and writing it back
yields 15 again), but the view-side adapter doesn't represent the middle value 15. -/
theorem rep_comp_strict :
    let x : Row Unit ℤ := fun _ => 15
    Rep (narrow10 ⨾ raise15) {()} x ∧ raise15.get x = some x ∧ ¬ Rep narrow10 {()} x := by
  intro x
  refine ⟨?_, ?_, ?_⟩
  · rw [narrow10, raise15, fieldLens_fuse, fieldLens_rep]
    intro _; simp [x]
  · simp only [raise15, fieldLens, Option.some.injEq]; funext u; cases u; simp [x]
  · rw [narrow10, fieldLens_rep]; simp [x]

/-! ## 3. `add` for edge writes is unlawful -/

/-- `first` for reads with `add` for writes: an old writer's edge is added to the set. -/
def firstAdd : Lens Unit (Finset ℕ) where
  get x := some (fun u => firstOf (x u))
  put p x := match p () with
    | none => some p
    | some v => some (fun _ => some (x () ∪ v))

/-- **`add` fails PutGet**: writing edge `2` on a row holding edge `1` reads back `1`. -/
theorem firstAdd_putGet_fails :
    let x : Row Unit (Finset ℕ) := fun _ => {1}
    let p : Patch Unit (Finset ℕ) := fun _ => some {2}
    ∃ x₁, firstAdd.write p x = some x₁ ∧ firstAdd.get x₁ ≠ some (applyP p (fun u => firstOf (x u))) := by
  intro x p
  refine ⟨fun _ => {1, 2}, ?_, ?_⟩
  · simp only [Lens.write, firstAdd, p, Option.map_some, Option.some.injEq]
    funext u; simp [applyP, x]
  · simp only [firstAdd]
    intro h
    have := congrFun (Option.some.inj h) ()
    simp [applyP, p, firstOf] at this

/-- **`add` fails PutPut**: writing `1` then `2` leaves both edges; writing the merge
leaves only `2`. -/
theorem firstAdd_putPut_fails :
    let x : Row Unit (Finset ℕ) := fun _ => ∅
    let p₁ : Patch Unit (Finset ℕ) := fun _ => some {1}
    let p₂ : Patch Unit (Finset ℕ) := fun _ => some {2}
    ∃ x₁, firstAdd.write p₁ x = some x₁ ∧
      firstAdd.write p₂ x₁ ≠ firstAdd.write (mergeP p₁ p₂) x := by
  intro x p₁ p₂
  refine ⟨fun _ => {1}, ?_, ?_⟩
  · simp only [Lens.write, firstAdd, p₁, Option.map_some, Option.some.injEq]
    funext u; simp [applyP, x]
  · simp only [Lens.write, firstAdd, p₁, p₂, mergeP, Option.map_some, Option.or_some]
    intro h
    have := congrFun (Option.some.inj h) ()
    have h1 := congrArg (fun s : Finset ℕ => 1 ∈ s) this
    simp [applyP, x] at h1

/-! ## 4. The name split -/

/-- A small alphabet with a space. -/
inductive Ch | a | b | c | sp
  deriving DecidableEq

/-- Split at the first space (§4.2 table): `(before, after)`. -/
def splitName (s : List Ch) : List Ch × List Ch :=
  (s.takeWhile (· ≠ Ch.sp), (s.dropWhile (· ≠ Ch.sp)).drop 1)

/-- Join: `first + " " + last`, without a trailing space when `last` is empty. -/
def joinName (fl : List Ch × List Ch) : List Ch :=
  if fl.2 = [] then fl.1 else fl.1 ++ Ch.sp :: fl.2

/-- **PutGet fails**: an old writer writing `"a "` reads back `"a"`. -/
theorem split_putGet_fails : joinName (splitName [Ch.a, Ch.sp]) ≠ [Ch.a, Ch.sp] := by decide

/-- **GetPut fails** when `first` contains a space: `("a b", "c")` reads as `"a b c"`, and
writing that back stores `("a", "b c")`. -/
theorem split_getPut_fails :
    splitName (joinName ([Ch.a, Ch.sp, Ch.b], [Ch.c])) ≠ ([Ch.a, Ch.sp, Ch.b], [Ch.c]) := by
  decide

/-- With the plain join `first + " " + last`, PutGet fails even on names without a space:
`"cher"` reads back as `"cher "`. -/
theorem split_plainJoin_fails :
    (fun fl : List Ch × List Ch => fl.1 ++ Ch.sp :: fl.2) (splitName [Ch.c]) ≠ [Ch.c] := by
  decide

end Shep.Adapters
