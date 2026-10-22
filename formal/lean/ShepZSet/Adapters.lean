/-
# Schema adapters as patch lenses (design.md §4.2–§4.4)

Model:
* A row is `F → V`: field IDs to values. Field IDs are global, so one field type serves
  every version (renames are free, §4.1.4). Each version has its own set of valid rows.
* A patch is a partial update `F → Option V`.
* An adapter (step lens) between a storage-side version and a view-side version is
  - `get : Row → Option Row` (`none` = the declared `hide`);
  - `put : Patch → Row → Option Patch` (`none` = the declared `reject`). `put` translates a
    view-side patch into a storage-side patch, given the current storage row. The write then
    applies that patch, so storage changes only on the fields the patch names. This is
    "writes stay patches" (§4.3).
* Composition `A ⨾ B` puts `A` on the view side and `B` on the storage side, matching
  `L_j = A_{j+1} ; … ; A_n` in §4.4.
-/
import ShepZSet.Facts

noncomputable section
open Classical

namespace Shep.Adapters

variable {F V : Type*}

/-- A row: field IDs to values. -/
abbrev Row (F V : Type*) := F → V

/-- A patch: a partial update. -/
abbrev Patch (F V : Type*) := F → Option V

/-- Apply a patch. -/
def applyP (p : Patch F V) (x : Row F V) : Row F V := fun f => (p f).getD (x f)

/-- Merge two patches; the second wins (`p₁ ⊕ p₂` in §4.4 law 5). -/
def mergeP (p₁ p₂ : Patch F V) : Patch F V := fun f => (p₂ f).or (p₁ f)

/-- The fields a patch touches. -/
def domP (p : Patch F V) : Set F := {f | (p f).isSome}

/-- The patch writing `y`'s values on the fields `D`. -/
def restrictP (y : Row F V) (D : Set F) : Patch F V := fun f => if f ∈ D then some (y f) else none

/-- Normalizing a patch's written values field by field. -/
def normP (norm : F → V → V) (p : Patch F V) : Patch F V := fun f => (p f).map (norm f)

/-- The identity normalization. -/
def exactNorm : F → V → V := fun _ v => v

@[simp] theorem normP_exact (p : Patch F V) : normP exactNorm p = p := by
  funext f; simp only [normP]; cases p f <;> rfl

theorem applyP_merge (p₁ p₂ : Patch F V) (x : Row F V) :
    applyP (mergeP p₁ p₂) x = applyP p₂ (applyP p₁ x) := by
  funext f; simp only [applyP, mergeP]; cases p₂ f <;> cases p₁ f <;> rfl

theorem applyP_restrict_self (y : Row F V) (D : Set F) : applyP (restrictP y D) y = y := by
  funext f; simp only [applyP, restrictP]; split_ifs <;> rfl

/-- If a patch leaves `y` unchanged, it is the restriction of `y` to its own domain. -/
theorem eq_restrict_of_applyP_eq {q : Patch F V} {y : Row F V} (h : applyP q y = y) :
    q = restrictP y (domP q) := by
  funext f
  have hf := congrFun h f
  simp only [applyP] at hf
  simp only [restrictP, domP, Set.mem_ofPred_eq]
  rcases Option.eq_none_or_eq_some (q f) with hq | ⟨v, hq⟩
  · simp [hq]
  · simp only [hq, Option.getD_some] at hf; simp [hq, hf]

theorem mergeP_restrict_same_dom {p : Patch F V} (y : Row F V) :
    mergeP p (restrictP y (domP p)) = restrictP y (domP p) := by
  funext f
  simp only [mergeP, restrictP, domP, Set.mem_ofPred_eq]
  rcases Option.eq_none_or_eq_some (p f) with hp | ⟨v, hp⟩ <;> simp [hp]

theorem applyP_eq_of_not_mem {p : Patch F V} {x : Row F V} {f : F} (h : f ∉ domP p) :
    applyP p x f = x f := by
  simp only [domP, Set.mem_ofPred_eq, Option.not_isSome_iff_eq_none] at h
  simp [applyP, h]

/-- An adapter, or a composed chain of them. -/
structure Lens (F V : Type*) where
  get : Row F V → Option (Row F V)
  put : Patch F V → Row F V → Option (Patch F V)

namespace Lens

/-- The storage row after a write, or `none` if the write is rejected. -/
def write (L : Lens F V) (p : Patch F V) (x : Row F V) : Option (Row F V) :=
  (L.put p x).map (fun q => applyP q x)

/-- The identity adapter (an unchanged field, or the head's own lens). -/
def id : Lens F V := ⟨some, fun p _ => some p⟩

/-- `A ⨾ B`: `A` on the view side, `B` on the storage side. -/
def comp (A B : Lens F V) : Lens F V where
  get x := (B.get x).bind A.get
  put p x := (B.get x).bind (fun y => (A.put p y).bind (fun q => B.put q x))

infixr:80 " ⨾ " => comp

@[ext] theorem ext' {A B : Lens F V} (hg : A.get = B.get) (hp : A.put = B.put) : A = B := by
  cases A; cases B; simp_all

/-- Right identity holds unconditionally. -/
theorem comp_id_right (A : Lens F V) : A ⨾ id = A := by
  apply ext'
  · funext x; simp [comp, id]
  · funext p x; simp [comp, id]

/-- `A` never accepts a write to a row it hides. -/
def NoHiddenWrite (A : Lens F V) : Prop := ∀ x p, A.get x = none → A.put p x = none

/-- Left identity holds exactly when the adapter rejects writes to rows it hides. -/
theorem comp_id_left_iff (A : Lens F V) : id ⨾ A = A ↔ NoHiddenWrite A := by
  constructor
  · intro h x p hx
    have := congrFun (congrFun (congrArg Lens.put h) p) x
    simp [comp, id, hx] at this
    exact this.symm
  · intro h
    apply ext'
    · funext x; simp [comp, id]
    · funext p x
      simp only [comp, id]
      cases hx : A.get x with
      | none => simp [h x p hx]
      | some y => simp

/-- **Associativity**, extensionally. -/
theorem comp_assoc (A B C : Lens F V) : (A ⨾ B) ⨾ C = A ⨾ (B ⨾ C) := by
  apply ext'
  · funext x; simp only [comp]; cases C.get x <;> simp
  · funext p x
    simp only [comp]
    cases C.get x with
    | none => simp
    | some z =>
      simp only [Option.bind_some]
      cases B.get z with
      | none => simp
      | some y => simp only [Option.bind_some, Option.bind_assoc]

theorem comp_get_none_iff (A B : Lens F V) (x : Row F V) :
    (A ⨾ B).get x = none ↔ B.get x = none ∨ ∃ y, B.get x = some y ∧ A.get y = none := by
  simp only [comp]; cases B.get x <;> simp

end Lens

/-! ## The laws (§4.4) -/

/-- The adapter laws for a lens with storage-side valid rows `SX` and view-side valid rows
`SY`, write footprint `wr` (law 6), and per-field write normalization `norm` (law 4;
`exactNorm` means no normalization). Purity (law 7) is implicit: these are Lean functions.
GetPut (law 3) is the definition of the representable set `Rep` below. -/
structure Laws (L : Lens F V) (SX SY : Set (Row F V)) (wr : F → Set F) (norm : F → V → V) :
    Prop where
  /-- Law 1: total read (or the declared `hide`). -/
  totalRead : ∀ x ∈ SX, ∀ y, L.get x = some y → y ∈ SY
  /-- Law 2: total write (or the declared `reject`). Valid writes produce valid storage. -/
  totalWrite : ∀ x ∈ SX, ∀ y, L.get x = some y → ∀ p, applyP p y ∈ SY →
    ∀ q, L.put p x = some q → applyP q x ∈ SX
  /-- Writes to a hidden row are rejected. -/
  noHiddenWrite : L.NoHiddenWrite
  /-- Law 4: PutGet, up to the declared normalization of the written values. -/
  putGet : ∀ x ∈ SX, ∀ y, L.get x = some y → ∀ p, applyP p y ∈ SY →
    ∀ q, L.put p x = some q → L.get (applyP q x) = some (applyP (normP norm p) y)
  /-- Law 5: PutPut, at the level of the translated patches. -/
  putPut : ∀ x ∈ SX, ∀ y, L.get x = some y → ∀ p₁ p₂, applyP p₁ y ∈ SY →
    ∀ q₁, L.put p₁ x = some q₁ →
      L.put (mergeP p₁ p₂) x = (L.put p₂ (applyP q₁ x)).map (mergeP q₁)
  /-- Law 6: locality. The storage fields a write touches lie in the footprint of the
  view fields it names. -/
  locality : ∀ x p q, L.put p x = some q → ∀ f ∈ domP q, ∃ g ∈ domP p, f ∈ wr g

/-- Law 3's representable set, per row `x` and per set of view fields `D`: writing back
what the version reads on `D` leaves storage unchanged (`put(get x, x) = x`, restricted
to `D`). This is exactly what the write guard evaluates. -/
def Rep (L : Lens F V) (D : Set F) (x : Row F V) : Prop :=
  ∃ y q, L.get x = some y ∧ L.put (restrictP y D) x = some q ∧ applyP q x = x

/-- The footprint of a composition: the union, over the view-side footprint, of the
storage-side footprints. -/
def compWr (wrA wrB : F → Set F) : F → Set F := fun g => {f | ∃ m ∈ wrA g, f ∈ wrB m}

theorem Laws.mono_view {L : Lens F V} {SX SY : Set (Row F V)} {wr : F → Set F}
    {norm : F → V → V} (h : Laws L SX SY wr norm) : Laws L SX SY wr norm := h

/-! ## Composition -/

section Composition

variable {A B : Lens F V} {SX SM SY : Set (Row F V)} {wrA wrB : F → Set F}
  {nA nB : F → V → V}

/-- **Laws 1, 2, 6, and no-hidden-write survive any composition**, whatever the
normalizations. -/
theorem comp_totalRead (hA : Laws A SM SY wrA nA) (hB : Laws B SX SM wrB nB) :
    ∀ x ∈ SX, ∀ z, (A ⨾ B).get x = some z → z ∈ SY := by
  intro x hx z hz
  simp only [Lens.comp] at hz
  cases hy : B.get x with
  | none => simp [hy] at hz
  | some y =>
    simp only [hy, Option.bind_some] at hz
    exact hA.totalRead y (hB.totalRead x hx y hy) z hz

theorem comp_locality (hA : Laws A SM SY wrA nA) (hB : Laws B SX SM wrB nB) :
    ∀ x p r, (A ⨾ B).put p x = some r → ∀ f ∈ domP r, ∃ g ∈ domP p, f ∈ compWr wrA wrB g := by
  intro x p r hr f hf
  simp only [Lens.comp] at hr
  cases hy : B.get x with
  | none => simp [hy] at hr
  | some y =>
    simp only [hy, Option.bind_some] at hr
    cases hq : A.put p y with
    | none => simp [hq] at hr
    | some q =>
      simp only [hq, Option.bind_some] at hr
      obtain ⟨m, hm, hfm⟩ := hB.locality x q r hr f hf
      obtain ⟨g, hg, hmg⟩ := hA.locality y p q hq m hm
      exact ⟨g, hg, m, hmg, hfm⟩

theorem comp_noHiddenWrite (hA : Laws A SM SY wrA nA) :
    (A ⨾ B).NoHiddenWrite := by
  intro x p hx
  simp only [Lens.comp] at hx ⊢
  cases hy : B.get x with
  | none => simp
  | some y =>
    simp only [hy, Option.bind_some] at hx ⊢
    simp [hA.noHiddenWrite y p hx]

theorem comp_totalWrite (hA : Laws A SM SY wrA nA) (hB : Laws B SX SM wrB nB) :
    ∀ x ∈ SX, ∀ z, (A ⨾ B).get x = some z → ∀ p, applyP p z ∈ SY →
      ∀ r, (A ⨾ B).put p x = some r → applyP r x ∈ SX := by
  intro x hx z hz p hp r hr
  simp only [Lens.comp] at hz hr
  cases hy : B.get x with
  | none => simp [hy] at hz
  | some y =>
    simp only [hy, Option.bind_some] at hz hr
    cases hq : A.put p y with
    | none => simp [hq] at hr
    | some q =>
      simp only [hq, Option.bind_some] at hr
      have hyM := hB.totalRead x hx y hy
      exact hB.totalWrite x hx y hy q (hA.totalWrite y hyM z hz p hp q hq) r hr

/-- **All laws survive composition when the storage-side lens writes exactly** (no
normalization). The composed normalization is the view-side one, and the composed
footprint is `compWr`. -/
theorem comp_laws (hA : Laws A SM SY wrA nA) (hB : Laws B SX SM wrB exactNorm) :
    Laws (A ⨾ B) SX SY (compWr wrA wrB) nA := by
  refine ⟨comp_totalRead hA hB, comp_totalWrite hA hB, comp_noHiddenWrite hA, ?_, ?_,
    comp_locality hA hB⟩
  · -- put-get
    intro x hx z hz p hp r hr
    simp only [Lens.comp] at hz hr ⊢
    cases hy : B.get x with
    | none => simp [hy] at hz
    | some y =>
      simp only [hy, Option.bind_some] at hz hr
      cases hq : A.put p y with
      | none => simp [hq] at hr
      | some q =>
        simp only [hq, Option.bind_some] at hr
        have hyM := hB.totalRead x hx y hy
        have hqv := hA.totalWrite y hyM z hz p hp q hq
        rw [hB.putGet x hx y hy q hqv r hr, normP_exact, Option.bind_some]
        exact hA.putGet y hyM z hz p hp q hq
  · -- put-put
    intro x hx z hz p₁ p₂ hp₁ r₁ hr₁
    simp only [Lens.comp] at hz hr₁ ⊢
    cases hy : B.get x with
    | none => simp [hy] at hz
    | some y =>
      simp only [hy, Option.bind_some] at hz hr₁ ⊢
      cases hq₁ : A.put p₁ y with
      | none => simp [hq₁] at hr₁
      | some q₁ =>
        simp only [hq₁, Option.bind_some] at hr₁
        have hyM := hB.totalRead x hx y hy
        have hqv := hA.totalWrite y hyM z hz p₁ hp₁ q₁ hq₁
        have hget := hB.putGet x hx y hy q₁ hqv r₁ hr₁
        rw [normP_exact] at hget
        rw [hget, Option.bind_some, hA.putPut y hyM z hz p₁ p₂ hp₁ q₁ hq₁]
        cases hq₂ : A.put p₂ (applyP q₁ y) with
        | none => simp
        | some q₂ =>
          simp only [Option.map_some, Option.bind_some]
          exact hB.putPut x hx y hy q₁ q₂ hqv r₁ hr₁

/-- **The composed representable set, sufficiency.** If the view-side lens represents `D` at
`y = get_B x` (its write-back patch `q` leaves `y` unchanged) and the storage-side lens
represents `q`'s fields at `x`, the composition represents `D` at `x`. -/
theorem rep_comp_of {x y : Row F V} (hy : B.get x = some y) {D : Set F} {z : Row F V}
    {q : Patch F V} (hz : A.get y = some z) (hq : A.put (restrictP z D) y = some q)
    (hqy : applyP q y = y) (hB : Rep B (domP q) x) : Rep (A ⨾ B) D x := by
  obtain ⟨y', r, hy', hr, hrx⟩ := hB
  rw [hy] at hy'; cases hy'
  refine ⟨z, r, ?_, ?_, hrx⟩
  · simp [Lens.comp, hy, hz]
  · simp only [Lens.comp, hy, Option.bind_some, hq]
    have hqe := eq_restrict_of_applyP_eq hqy
    rw [hqe]; exact hr

/-- **The composed representable set, exactly, when the storage side writes exactly.**
`Rep (A ⨾ B) D x` holds iff `A` represents `D` at `y = get_B x` and `B` represents, at `x`,
the fields of `A`'s write-back patch. This is the per-field form of §4.4's
`R_B ∩ get_B⁻¹(R_A)`: `B` need only represent the fields `A` writes back, not every field. -/
theorem rep_comp_iff (hA : Laws A SM SY wrA nA) (hB : Laws B SX SM wrB exactNorm)
    {x : Row F V} (hx : x ∈ SX) (D : Set F) :
    Rep (A ⨾ B) D x ↔ ∃ y z q, B.get x = some y ∧ A.get y = some z ∧
      A.put (restrictP z D) y = some q ∧ applyP q y = y ∧ Rep B (domP q) x := by
  constructor
  · rintro ⟨z, r, hz, hr, hrx⟩
    simp only [Lens.comp] at hz hr
    cases hy : B.get x with
    | none => simp [hy] at hz
    | some y =>
      simp only [hy, Option.bind_some] at hz hr
      cases hq : A.put (restrictP z D) y with
      | none => simp [hq] at hr
      | some q =>
        simp only [hq, Option.bind_some] at hr
        have hyM := hB.totalRead x hx y hy
        have hzY := hA.totalRead y hyM z hz
        have hqv := hA.totalWrite y hyM z hz _ (by rw [applyP_restrict_self]; exact hzY) q hq
        have hget := hB.putGet x hx y hy q hqv r hr
        rw [normP_exact, hrx, hy] at hget
        have hqy : applyP q y = y := (Option.some.inj hget).symm
        refine ⟨y, z, q, rfl, hz, hq, hqy, y, r, hy, ?_, hrx⟩
        rw [← eq_restrict_of_applyP_eq hqy]; exact hr
  · rintro ⟨y, z, q, hy, hz, hq, hqy, hB'⟩
    exact rep_comp_of hy hz hq hqy hB'

end Composition

/-! ## The write guard: representability is exactly undoability -/

section Guard

variable {L : Lens F V} {SX SY : Set (Row F V)} {wr : F → Set F} {norm : F → V → V}

/-- **An accepted write can be undone by the same version**: if the guard passes (`Rep` on
the written fields), writing back the old values restores storage exactly. -/
theorem guard_undo (hL : Laws L SX SY wr norm) {x : Row F V} (hx : x ∈ SX) {y : Row F V}
    (hy : L.get x = some y) {p : Patch F V} (hp : applyP p y ∈ SY)
    (hrep : Rep L (domP p) x) {x₁ : Row F V} (hw : L.write p x = some x₁) :
    L.write (restrictP y (domP p)) x₁ = some x := by
  obtain ⟨y', q, hy', hq, hqx⟩ := hrep
  rw [hy] at hy'; cases hy'
  simp only [Lens.write] at hw ⊢
  cases hq₁ : L.put p x with
  | none => simp [hq₁] at hw
  | some q₁ =>
    simp only [hq₁, Option.map_some, Option.some.injEq] at hw
    subst hw
    have hpp := hL.putPut x hx y hy p (restrictP y (domP p)) hp q₁ hq₁
    rw [mergeP_restrict_same_dom, hq] at hpp
    cases hq₂ : L.put (restrictP y (domP p)) (applyP q₁ x) with
    | none => simp [hq₂] at hpp
    | some q₂ =>
      simp only [hq₂, Option.map_some, Option.some.injEq] at hpp
      simp only [Option.map_some, Option.some.injEq]
      rw [← applyP_merge, ← hpp, hqx]

theorem domP_merge (p₁ p₂ : Patch F V) : domP (mergeP p₁ p₂) = domP p₁ ∪ domP p₂ := by
  ext f; simp only [domP, mergeP, Set.mem_ofPred_eq, Set.mem_union]
  cases p₂ f <;> cases p₁ f <;> simp

/-- **Conversely, with exact PutGet**: if some valid write by the same version, to a subset
of the same fields, undoes the write, the fields were representable. So the guard rejects
exactly the writes that the writer could never undo. -/
theorem undo_implies_rep (hL : Laws L SX SY wr exactNorm) {x : Row F V} (hx : x ∈ SX)
    {y : Row F V} (hy : L.get x = some y) {p p' : Patch F V} (hp : applyP p y ∈ SY)
    (hp' : applyP p' (applyP p y) ∈ SY) (hdom : domP p' ⊆ domP p) {x₁ : Row F V}
    (hw : L.write p x = some x₁) (hundo : L.write p' x₁ = some x) : Rep L (domP p) x := by
  simp only [Lens.write] at hw hundo
  cases hq₁ : L.put p x with
  | none => simp [hq₁] at hw
  | some q₁ =>
    simp only [hq₁, Option.map_some, Option.some.injEq] at hw
    subst hw
    cases hq₂ : L.put p' (applyP q₁ x) with
    | none => simp [hq₂] at hundo
    | some q₂ =>
      simp only [hq₂, Option.map_some, Option.some.injEq] at hundo
      have hg1 := hL.putGet x hx y hy p hp q₁ hq₁
      rw [normP_exact] at hg1
      have hv1 := hL.totalWrite x hx y hy p hp q₁ hq₁
      have hg2 := hL.putGet _ hv1 _ hg1 p' hp' q₂ hq₂
      rw [hundo, hy, normP_exact] at hg2
      -- the merged view patch leaves `y` unchanged
      have hmy : applyP (mergeP p p') y = y := by
        rw [applyP_merge]; exact (Option.some.inj hg2).symm
      have hdm : domP (mergeP p p') = domP p := by
        rw [domP_merge]; exact Set.union_eq_left.mpr hdom
      have hpp := hL.putPut x hx y hy p p' hp q₁ hq₁
      rw [hq₂, Option.map_some] at hpp
      refine ⟨y, mergeP q₁ q₂, hy, ?_, ?_⟩
      · rw [← hdm, ← eq_restrict_of_applyP_eq hmy]; exact hpp
      · rw [applyP_merge, hundo]

/-- **No silent loss, view side**: after an accepted write, the writer reads back what it
wrote (normalized), and every field it didn't touch reads as before. -/
theorem write_readback (hL : Laws L SX SY wr norm) {x : Row F V} (hx : x ∈ SX) {y : Row F V}
    (hy : L.get x = some y) {p : Patch F V} (hp : applyP p y ∈ SY) {x₁ : Row F V}
    (hw : L.write p x = some x₁) :
    L.get x₁ = some (applyP (normP norm p) y) ∧
      ∀ g ∉ domP p, ∀ y₁, L.get x₁ = some y₁ → y₁ g = y g := by
  simp only [Lens.write] at hw
  cases hq : L.put p x with
  | none => simp [hq] at hw
  | some q =>
    simp only [hq, Option.map_some, Option.some.injEq] at hw
    subst hw
    have hg := hL.putGet x hx y hy p hp q hq
    refine ⟨hg, fun g hg' y₁ hy₁ => ?_⟩
    rw [hg] at hy₁
    rw [← Option.some.inj hy₁]
    have : g ∉ domP (normP norm p) := by
      simp only [domP, normP, Set.mem_ofPred_eq, Option.isSome_map] at hg' ⊢; exact hg'
    exact applyP_eq_of_not_mem this

/-- **No silent loss, storage side**: an accepted write changes only storage fields in the
footprint of the fields it names. -/
theorem write_local (hL : Laws L SX SY wr norm) {x : Row F V} {p : Patch F V}
    {x₁ : Row F V} (hw : L.write p x = some x₁) (f : F)
    (hf : ∀ g ∈ domP p, f ∉ wr g) : x₁ f = x f := by
  simp only [Lens.write] at hw
  cases hq : L.put p x with
  | none => simp [hq] at hw
  | some q =>
    simp only [hq, Option.map_some, Option.some.injEq] at hw
    subst hw
    apply applyP_eq_of_not_mem
    intro hfq
    obtain ⟨g, hg, hfg⟩ := hL.locality x p q hq f hfq
    exact hf g hg hfg

theorem mergeP_restrict (y : Row F V) (D E : Set F) :
    mergeP (restrictP y D) (restrictP y E) = restrictP y (D ∪ E) := by
  funext f
  simp only [mergeP, restrictP, Set.mem_union]
  by_cases hE : f ∈ E <;> by_cases hD : f ∈ D <;> simp [hD, hE]

/-- Representability of two field sets gives representability of their union. -/
theorem rep_union (hL : Laws L SX SY wr norm) {x : Row F V} (hx : x ∈ SX) {D E : Set F}
    (hD : Rep L D x) (hE : Rep L E x) : Rep L (D ∪ E) x := by
  obtain ⟨y, q₁, hy, hq₁, hq₁x⟩ := hD
  obtain ⟨y', q₂, hy', hq₂, hq₂x⟩ := hE
  rw [hy] at hy'; cases hy'
  have hpp := hL.putPut x hx y hy (restrictP y D) (restrictP y E)
    (by rw [applyP_restrict_self]; exact hL.totalRead x hx y hy) q₁ hq₁
  rw [hq₁x, hq₂, Option.map_some, mergeP_restrict] at hpp
  refine ⟨y, mergeP q₁ q₂, hy, hpp, ?_⟩
  rw [applyP_merge, hq₁x, hq₂x]

/-- **The per-field write guard is sound**: if each field a write names is representable
on its own, the write as a whole is representable, so `guard_undo` applies. -/
theorem rep_of_each (hL : Laws L SX SY wr norm) {x : Row F V} (hx : x ∈ SX) :
    ∀ fs : List F, fs ≠ [] → (∀ f ∈ fs, Rep L {f} x) → Rep L {f | f ∈ fs} x := by
  intro fs
  induction fs with
  | nil => intro h; exact absurd rfl h
  | cons f fs ih =>
    intro _ hall
    by_cases hfs : fs = []
    · subst hfs
      have : {g | g ∈ [f]} = ({f} : Set F) := by ext g; simp
      rw [this]; exact hall f (by simp)
    · have e : {g | g ∈ f :: fs} = {f} ∪ {g | g ∈ fs} := by ext g; simp
      rw [e]
      exact rep_union hL hx (hall f (by simp)) (ih hfs (fun g hg => hall g (by simp [hg])))

end Guard

end Shep.Adapters
