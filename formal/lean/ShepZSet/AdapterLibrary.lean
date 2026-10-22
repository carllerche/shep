/-
# The built-in adapter library (design.md §4.2 table, §4.3)

Most built-ins act on one field. `fieldLens f₀ g w` reads field `f₀` through `g` and writes
it through `w` (`none` = reject), passing every other field through. Its laws reduce to
three value-level laws (`FieldLaws`), and its representable set is
`{x | w (g (x f₀)) = some (x f₀)}`. Each built-in is an instance with its exact
representable set. Fusion rules for built-ins are lens equalities.
-/
import ShepZSet.Adapters
import Mathlib.Order.Interval.Set.Basic

noncomputable section
open Classical

namespace Shep.Adapters

variable {F V : Type*} [DecidableEq F]

/-- Per-field typing: a row is valid iff each field's value has that field's type. -/
def Typed (T : F → Set V) : Set (Row F V) := {x | ∀ f, x f ∈ T f}

/-- A single-field adapter on field `f₀`. -/
def fieldLens (f₀ : F) (g : V → V) (w : V → Option V) : Lens F V where
  get x := some (Function.update x f₀ (g (x f₀)))
  put p _x := match p f₀ with
    | none => some p
    | some v => (w v).map (fun s => Function.update p f₀ (some s))

/-- The value-level laws of a single-field adapter: storage type `Tx`, view type `Ty`,
written-value normalization `nrm` (`id` when the write is exact). -/
structure FieldLaws (g : V → V) (w : V → Option V) (Tx Ty : Set V) (nrm : V → V) : Prop where
  read : ∀ s ∈ Tx, g s ∈ Ty
  write : ∀ v ∈ Ty, ∀ s, w v = some s → s ∈ Tx
  putGet : ∀ v ∈ Ty, ∀ s, w v = some s → g s = nrm v

/-- The normalization of a single-field adapter, as a per-field function. -/
def fieldNorm (f₀ : F) (nrm : V → V) : F → V → V :=
  fun f => if f = f₀ then nrm else fun v => v

section FieldLens

variable {f₀ : F} {g : V → V} {w : V → Option V} {Tx Ty : Set V} {nrm : V → V}
  {T : F → Set V}

theorem fieldLens_put_none {p : Patch F V} (_x : Row F V) (h : p f₀ = none) :
    (fieldLens f₀ g w).put p _x = some p := by
  simp [fieldLens, h]

theorem fieldLens_put_some {p : Patch F V} (x : Row F V) {v : V} (h : p f₀ = some v) :
    (fieldLens f₀ g w).put p x = (w v).map (fun s => Function.update p f₀ (some s)) := by
  simp [fieldLens, h]

/-- **A single-field adapter satisfies all the laws** when its value-level laws hold. Its
write footprint is the field itself. -/
theorem fieldLens_laws (hT : T f₀ = Tx) (hL : FieldLaws g w Tx Ty nrm) :
    Laws (fieldLens f₀ g w) (Typed T) (Typed (Function.update T f₀ Ty)) (fun h => {h})
      (fieldNorm f₀ nrm) := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_⟩
  · -- total read
    intro x hx y hy f
    simp only [fieldLens, Option.some.injEq] at hy
    subst hy
    by_cases hf : f = f₀
    · subst hf; simp only [Function.update_self]; exact hL.read _ (hT ▸ hx f)
    · simp only [Function.update_of_ne hf]; exact hx f
  · -- total write
    intro x hx y hy p hp q hq f
    simp only [fieldLens, Option.some.injEq] at hy
    subst hy
    have hpf := hp f
    rcases Option.eq_none_or_eq_some (p f₀) with h0 | ⟨v, h0⟩
    · rw [fieldLens_put_none x h0, Option.some.injEq] at hq
      subst hq
      by_cases hf : f = f₀
      · subst hf; simp [applyP, h0]; exact hx f
      · simp only [applyP, Function.update_of_ne hf] at hpf ⊢; exact hpf
    · rw [fieldLens_put_some x h0] at hq
      cases hs : w v with
      | none => simp [hs] at hq
      | some s =>
        simp only [hs, Option.map_some, Option.some.injEq] at hq
        subst hq
        by_cases hf : f = f₀
        · subst hf
          have hv : v ∈ Ty := by
            have := hp f; simp only [applyP, h0, Option.getD_some, Function.update_self] at this
            exact this
          simp only [applyP, Function.update_self, Option.getD_some]
          rw [hT]; exact hL.write v hv s hs
        · simp only [applyP, Function.update_of_ne hf] at hpf ⊢; exact hpf
  · -- no hidden writes
    intro x p hx; simp [fieldLens] at hx
  · -- put-get
    intro x _ y hy p hp q hq
    simp only [fieldLens, Option.some.injEq] at hy
    subst hy
    simp only [fieldLens, Option.some.injEq]
    funext f
    rcases Option.eq_none_or_eq_some (p f₀) with h0 | ⟨v, h0⟩
    · rw [fieldLens_put_none x h0, Option.some.injEq] at hq
      subst hq
      by_cases hf : f = f₀
      · subst hf; simp [applyP, normP, h0]
      · simp [applyP, normP, fieldNorm, hf]
    · rw [fieldLens_put_some x h0] at hq
      cases hs : w v with
      | none => simp [hs] at hq
      | some s =>
        simp only [hs, Option.map_some, Option.some.injEq] at hq
        subst hq
        have hv : v ∈ Ty := by
          have := hp f₀; simp only [applyP, h0, Option.getD_some, Function.update_self] at this
          exact this
        by_cases hf : f = f₀
        · subst hf
          simp [applyP, normP, fieldNorm, h0, hL.putGet v hv s hs]
        · simp [applyP, normP, fieldNorm, hf]
  · -- put-put
    intro x _ y hy p₁ p₂ _ q₁ hq₁
    rcases Option.eq_none_or_eq_some (p₂ f₀) with h2 | ⟨v₂, h2⟩
    · rcases Option.eq_none_or_eq_some (p₁ f₀) with h1 | ⟨v₁, h1⟩
      · rw [fieldLens_put_none x h1, Option.some.injEq] at hq₁
        subst hq₁
        have hm : mergeP p₁ p₂ f₀ = none := by simp [mergeP, h1, h2]
        rw [fieldLens_put_none x hm, fieldLens_put_none _ h2]; rfl
      · rw [fieldLens_put_some x h1] at hq₁
        cases hs : w v₁ with
        | none => simp [hs] at hq₁
        | some s =>
          simp only [hs, Option.map_some, Option.some.injEq] at hq₁
          subst hq₁
          have hm : mergeP p₁ p₂ f₀ = some v₁ := by simp [mergeP, h1, h2]
          rw [fieldLens_put_some x hm, hs, fieldLens_put_none _ h2]
          simp only [Option.map_some, Option.some.injEq]
          funext f
          by_cases hf : f = f₀
          · subst hf; simp [mergeP, h2]
          · simp [mergeP, Function.update_of_ne hf]
    · have hm : mergeP p₁ p₂ f₀ = some v₂ := by simp [mergeP, h2]
      rw [fieldLens_put_some x hm, fieldLens_put_some _ h2]
      cases hs : w v₂ with
      | none => simp
      | some s =>
        simp only [Option.map_some, Option.some.injEq]
        have hq₁f : ∀ f, f ≠ f₀ → q₁ f = p₁ f := by
          intro f hf
          rcases Option.eq_none_or_eq_some (p₁ f₀) with h1 | ⟨v₁, h1⟩
          · rw [fieldLens_put_none x h1, Option.some.injEq] at hq₁; rw [hq₁]
          · rw [fieldLens_put_some x h1] at hq₁
            cases hs1 : w v₁ with
            | none => simp [hs1] at hq₁
            | some s1 =>
              simp only [hs1, Option.map_some, Option.some.injEq] at hq₁
              rw [← hq₁, Function.update_of_ne hf]
        funext f
        by_cases hf : f = f₀
        · subst hf; simp [mergeP]
        · simp [mergeP, Function.update_of_ne hf, hq₁f f hf]
  · -- locality
    intro x p q hq f hf
    refine ⟨f, ?_, rfl⟩
    simp only [domP, Set.mem_ofPred_eq] at hf ⊢
    rcases Option.eq_none_or_eq_some (p f₀) with h0 | ⟨v, h0⟩
    · rw [fieldLens_put_none x h0, Option.some.injEq] at hq; subst hq; exact hf
    · rw [fieldLens_put_some x h0] at hq
      cases hs : w v with
      | none => simp [hs] at hq
      | some s =>
        simp only [hs, Option.map_some, Option.some.injEq] at hq
        subst hq
        by_cases hff : f = f₀
        · subst hff; simp [h0]
        · simpa [Function.update_of_ne hff] using hf

/-- **The representable set of a single-field adapter**: a write naming `f₀` is
representable exactly where writing back the read value of `f₀` reproduces it. Writes that
don't name `f₀` are always representable. -/
theorem fieldLens_rep (x : Row F V) (D : Set F) :
    Rep (fieldLens f₀ g w) D x ↔ (f₀ ∈ D → w (g (x f₀)) = some (x f₀)) := by
  constructor
  · rintro ⟨y, q, hy, hq, hqx⟩ hD
    simp only [fieldLens, Option.some.injEq] at hy
    subst hy
    have h0 : restrictP (Function.update x f₀ (g (x f₀))) D f₀ = some (g (x f₀)) := by
      simp [restrictP, hD]
    rw [fieldLens_put_some x h0] at hq
    cases hs : w (g (x f₀)) with
    | none => simp [hs] at hq
    | some s =>
      simp only [hs, Option.map_some, Option.some.injEq] at hq
      subst hq
      have := congrFun hqx f₀
      simp only [applyP, Function.update_self, Option.getD_some] at this
      rw [this]
  · intro h
    by_cases hD : f₀ ∈ D
    · have h0 : restrictP (Function.update x f₀ (g (x f₀))) D f₀ = some (g (x f₀)) := by
        simp [restrictP, hD]
      refine ⟨_, Function.update (restrictP (Function.update x f₀ (g (x f₀))) D) f₀
        (some (x f₀)), rfl, ?_, ?_⟩
      · rw [fieldLens_put_some x h0, h hD]; rfl
      · funext f
        by_cases hf : f = f₀
        · subst hf; simp [applyP]
        · simp only [applyP, Function.update_of_ne hf, restrictP]; split_ifs <;> rfl
    · have h0 : restrictP (Function.update x f₀ (g (x f₀))) D f₀ = none := by
        simp [restrictP, hD]
      refine ⟨_, _, rfl, fieldLens_put_none x h0, ?_⟩
      funext f
      simp only [applyP, restrictP]
      split_ifs with hfD
      · have hf : f ≠ f₀ := fun h => hD (h ▸ hfD)
        simp [Function.update_of_ne hf]
      · rfl

/-- **Fusion of two single-field adapters on the same field.** -/
theorem fieldLens_fuse (g₁ g₂ : V → V) (w₁ w₂ : V → Option V) :
    fieldLens f₀ g₁ w₁ ⨾ fieldLens f₀ g₂ w₂ =
      fieldLens f₀ (g₁ ∘ g₂) (fun v => (w₁ v).bind w₂) := by
  apply Lens.ext'
  · funext x
    simp [Lens.comp, fieldLens, Function.update_idem]
  · funext p x
    simp only [Lens.comp, fieldLens, Option.bind_some]
    rcases Option.eq_none_or_eq_some (p f₀) with h0 | ⟨v, h0⟩
    · simp [h0]
    · simp only [h0]
      cases hs : w₁ v with
      | none => simp
      | some s =>
        simp only [Option.map_some, Option.bind_some, Function.update_self]
        cases w₂ s <;> simp [Function.update_idem]

/-- **Single-field adapters compose lawfully, even with lossy writes.** The composite of
two value-level lawful adapters is value-level lawful. Its normalization is "write through
`w₁`, normalize with `n₂`, read through `g₁`", a function of the written value alone. -/
theorem fieldLaws_comp {g₁ g₂ : V → V} {w₁ w₂ : V → Option V} {Tm : Set V}
    {n₁ n₂ : V → V} (hA : FieldLaws g₁ w₁ Tm Ty n₁) (hB : FieldLaws g₂ w₂ Tx Tm n₂) :
    FieldLaws (g₁ ∘ g₂) (fun v => (w₁ v).bind w₂) Tx Ty
      (fun v => match w₁ v with | some m => g₁ (n₂ m) | none => v) := by
  refine ⟨fun s hs => hA.read _ (hB.read s hs), ?_, ?_⟩
  · intro v hv s hs
    cases hm : w₁ v with
    | none => simp [hm] at hs
    | some m =>
      simp only [hm, Option.bind_some] at hs
      exact hB.write m (hA.write v hv m hm) s hs
  · intro v hv s hs
    cases hm : w₁ v with
    | none => simp [hm] at hs
    | some m =>
      simp only [hm, Option.bind_some] at hs
      simp only [Function.comp, hB.putGet m (hA.write v hv m hm) s hs]

/-- Hence a chain of single-field adapters on one field is lawful as a whole, whatever their
normalizations. Only cross-field steps need a per-chain re-proof (see
`AdapterCounterexamples.chain_not_lawful`). -/
theorem fieldLens_comp_laws {g₁ g₂ : V → V} {w₁ w₂ : V → Option V} {Tm : Set V}
    {n₁ n₂ : V → V} (hT : T f₀ = Tx) (hA : FieldLaws g₁ w₁ Tm Ty n₁)
    (hB : FieldLaws g₂ w₂ Tx Tm n₂) :
    Laws (fieldLens f₀ g₁ w₁ ⨾ fieldLens f₀ g₂ w₂) (Typed T)
      (Typed (Function.update T f₀ Ty)) (fun h => {h})
      (fieldNorm f₀ (fun v => match w₁ v with | some m => g₁ (n₂ m) | none => v)) := by
  rw [fieldLens_fuse]
  exact fieldLens_laws hT (fieldLaws_comp hA hB)

end FieldLens

/-! ## Instances -/

omit [DecidableEq F] in
/-- **Rename** is the identity lens: fields are addressed by ID (§4.1.4). -/
theorem id_laws (S : Set (Row F V)) : Laws Lens.id S S (fun h => {h}) exactNorm := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro x hx y hy; simp [Lens.id] at hy; exact hy ▸ hx
  · intro x _ y hy p hp q hq; simp [Lens.id] at hy hq; subst hy hq; exact hp
  · intro x p hx; simp [Lens.id] at hx
  · intro x _ y hy p _ q hq; simp [Lens.id] at hy hq; subst hy hq; simp [Lens.id]
  · intro x _ y _ p₁ p₂ _ q₁ hq₁; simp [Lens.id] at hq₁; subst hq₁; simp [Lens.id]
  · intro x p q hq f hf; simp [Lens.id] at hq; subst hq; exact ⟨f, hf, rfl⟩

omit [DecidableEq F] in
theorem id_rep (D : Set F) (x : Row F V) : Rep (Lens.id : Lens F V) D x :=
  ⟨x, restrictP x D, rfl, rfl, applyP_restrict_self x D⟩

section Truncate

variable {α : Type*}

/-- `truncate(n)` as a **read** adapter (a string limit widened from `n`): the old version
sees the first `n` characters. Writes pass through. -/
def truncRead (f₀ : F) (n : ℕ) : Lens F (List α) := fieldLens f₀ (List.take n) some

theorem truncRead_fieldLaws (n : ℕ) (Tx : Set (List α)) (hsub : {v | v.length ≤ n} ⊆ Tx) :
    FieldLaws (List.take n) some Tx {v | v.length ≤ n} (fun v => v) := by
  refine ⟨?_, ?_, ?_⟩
  · intro s _; simp only [Set.mem_ofPred_eq, List.length_take]; omega
  · intro v hv s hs; simp only [Option.some.injEq] at hs; subst hs; exact hsub hv
  · intro v hv s hs
    simp only [Option.some.injEq] at hs; subst hs
    exact List.take_of_length_le hv

/-- Its representable set: strings of length `≤ n`. -/
theorem truncRead_rep (f₀ : F) (n : ℕ) (x : Row F (List α)) :
    Rep (truncRead f₀ n) {f₀} x ↔ (x f₀).length ≤ n := by
  rw [truncRead, fieldLens_rep]
  simp only [Set.mem_singleton_iff, forall_const, Option.some.injEq]
  exact List.take_eq_self_iff _

/-- **Fusion**: `truncate(a)` read through `truncate(b)` is `truncate(min a b)`, in either
order. -/
theorem truncRead_fuse (f₀ : F) (a b : ℕ) :
    truncRead (α := α) f₀ a ⨾ truncRead f₀ b = truncRead f₀ (min a b) := by
  rw [truncRead, truncRead, fieldLens_fuse]
  unfold truncRead
  congr 1
  funext s; simp [List.take_take]

theorem truncRead_fuse_comm (f₀ : F) (a b : ℕ) :
    truncRead (α := α) f₀ a ⨾ truncRead f₀ b = truncRead f₀ b ⨾ truncRead f₀ a := by
  rw [truncRead_fuse, truncRead_fuse, min_comm]

/-- The design's example: `truncate(200) ∘ truncate(500) = truncate(200)`. -/
theorem truncRead_200_500 (f₀ : F) :
    truncRead (α := α) f₀ 200 ⨾ truncRead f₀ 500 = truncRead f₀ 200 := by
  rw [truncRead_fuse]; rfl

/-- `truncate(m)` as a **write** adapter on a tightening (limit lowered to `m`): old writers'
values are cut to `m` (a lossy write, normalization `take m`). Reads pass through. -/
def truncWrite (f₀ : F) (m : ℕ) : Lens F (List α) := fieldLens f₀ id (fun v => some (v.take m))

theorem truncWrite_fieldLaws (m : ℕ) (Ty : Set (List α))
    (hTy : ∀ s : List α, s.length ≤ m → s ∈ Ty) :
    FieldLaws id (fun v => some (v.take m)) {s | s.length ≤ m} Ty (List.take m) := by
  refine ⟨fun s hs => hTy s hs, ?_, ?_⟩
  · intro v _ s hs
    simp only [Option.some.injEq] at hs; subst hs
    simp only [Set.mem_ofPred_eq, List.length_take]; omega
  · intro v _ s hs; simp only [Option.some.injEq] at hs; subst hs; rfl

/-- Its representable set is every valid stored value: nothing stored is hidden. -/
theorem truncWrite_rep (f₀ : F) (m : ℕ) (x : Row F (List α)) (hx : (x f₀).length ≤ m) :
    Rep (truncWrite f₀ m) {f₀} x := by
  rw [truncWrite, fieldLens_rep]
  intro _
  simp only [id, Option.some.injEq]
  exact List.take_of_length_le hx

/-- The rejecting alternative: writes longer than `m` are rejected. Exact, no
normalization. -/
def truncReject (f₀ : F) (m : ℕ) : Lens F (List α) :=
  fieldLens f₀ id (fun v => if v.length ≤ m then some v else none)

theorem truncReject_fieldLaws (m : ℕ) (Ty : Set (List α))
    (hTy : ∀ s : List α, s.length ≤ m → s ∈ Ty) :
    FieldLaws id (fun v => if v.length ≤ m then some v else none) {s | s.length ≤ m} Ty
      (fun v => v) := by
  refine ⟨fun s hs => hTy s hs, ?_, ?_⟩
  · intro v _ s hs
    split_ifs at hs with h
    · simp only [Option.some.injEq] at hs; subst hs; exact h
  · intro v _ s hs
    split_ifs at hs with h
    · simp only [Option.some.injEq] at hs; subst hs; rfl

/-- The backfill for a tightening: cut existing values to `m`. -/
def truncBackfill (f₀ : F) (m : ℕ) (x : Row F (List α)) : Row F (List α) :=
  Function.update x f₀ ((x f₀).take m)

/-- The backfilled row is valid for the new head, and the old head reads it as its declared
lossy image (`take m` of the old value; unchanged if it was already short enough). -/
theorem truncBackfill_spec (f₀ : F) (m : ℕ) (T : F → Set (List α))
    (hT : T f₀ = {s | s.length ≤ m}) (Told : F → Set (List α))
    (hTold : ∀ f, f ≠ f₀ → Told f ⊆ T f) (x : Row F (List α)) (hx : x ∈ Typed Told) :
    truncBackfill f₀ m x ∈ Typed T ∧
      (truncWrite f₀ m).get (truncBackfill f₀ m x) = some (truncBackfill f₀ m x) ∧
      ((x f₀).length ≤ m → truncBackfill f₀ m x = x) := by
  refine ⟨?_, ?_, ?_⟩
  · intro f
    by_cases hf : f = f₀
    · subst hf; rw [hT]; simp [truncBackfill]
    · simp only [truncBackfill, Function.update_of_ne hf]; exact hTold f hf (hx f)
  · simp [truncWrite, fieldLens, truncBackfill]
  · intro h; simp [truncBackfill, List.take_of_length_le h]

end Truncate

section Clamp

/-- `clamp` / `saturate` (an integer range widened, e.g. `Int32 → Int64`): the old version
sees the value clamped to `[lo, hi]`. -/
def clampRead (f₀ : F) (lo hi : ℤ) : Lens F ℤ := fieldLens f₀ (fun s => max lo (min hi s)) some

theorem clampRead_fieldLaws (lo hi : ℤ) (hlh : lo ≤ hi) (Tx : Set ℤ)
    (hsub : Set.Icc lo hi ⊆ Tx) :
    FieldLaws (fun s => max lo (min hi s)) some Tx (Set.Icc lo hi) (fun v => v) := by
  refine ⟨?_, ?_, ?_⟩
  · intro s _; simp only [Set.mem_Icc]; omega
  · intro v hv s hs; simp only [Option.some.injEq] at hs; subst hs; exact hsub hv
  · intro v hv s hs
    simp only [Option.some.injEq] at hs; subst hs
    simp only [Set.mem_Icc] at hv; omega

theorem clampRead_rep (f₀ : F) (lo hi : ℤ) (hlh : lo ≤ hi) (x : Row F ℤ) :
    Rep (clampRead f₀ lo hi) {f₀} x ↔ lo ≤ x f₀ ∧ x f₀ ≤ hi := by
  rw [clampRead, fieldLens_rep]
  simp only [Set.mem_singleton_iff, forall_const, Option.some.injEq]
  omega

end Clamp

section EnumMap

variable {E : Type*} [DecidableEq E]

/-- `map(a → b)` for a new enum variant `a`: old versions read `a` as `b`. -/
def enumMap (f₀ : F) (a b : E) : Lens F E := fieldLens f₀ (fun s => if s = a then b else s) some

theorem enumMap_fieldLaws (a b : E) (hab : b ≠ a) (Tx : Set E) (hsub : {s | s ≠ a} ⊆ Tx) :
    FieldLaws (fun s => if s = a then b else s) some Tx {s | s ≠ a} (fun v => v) := by
  refine ⟨?_, ?_, ?_⟩
  · intro s _; simp only [Set.mem_ofPred_eq]; split_ifs with h
    · exact hab
    · exact h
  · intro v hv s hs; simp only [Option.some.injEq] at hs; subst hs; exact hsub hv
  · intro v hv s hs
    simp only [Option.some.injEq] at hs; subst hs
    simp only [Set.mem_ofPred_eq] at hv; simp [hv]

theorem enumMap_rep (f₀ : F) (a b : E) (hab : b ≠ a) (x : Row F E) :
    Rep (enumMap f₀ a b) {f₀} x ↔ x f₀ ≠ a := by
  rw [enumMap, fieldLens_rep]
  simp only [Set.mem_singleton_iff, forall_const, Option.some.injEq]
  constructor
  · intro h hx; rw [hx] at h; simp at h; exact hab h
  · intro h; simp [h]

end EnumMap

section First

variable {τ : Type*} [LinearOrder τ]

/-- The single edge an old version sees of a one-to-many edge: the first by sort key. -/
def firstOf (s : Finset τ) : Finset τ := if h : s.Nonempty then {s.min' h} else ∅

theorem firstOf_of_card_le_one {s : Finset τ} (h : s.card ≤ 1) : firstOf s = s := by
  rcases s.eq_empty_or_nonempty with h0 | ⟨a, ha⟩
  · subst h0; simp [firstOf]
  · have : s = {a} := Finset.eq_singleton_iff_unique_mem.mpr
      ⟨ha, fun b hb => Finset.card_le_one.mp h b hb a ha⟩
    subst this; simp [firstOf]

theorem card_firstOf_le (s : Finset τ) : (firstOf s).card ≤ 1 := by
  unfold firstOf; split_ifs <;> simp

/-- `first` (edge one → many): reads the first edge; writes replace the edge set. -/
def firstEdge (f₀ : F) : Lens F (Finset τ) := fieldLens f₀ firstOf some

theorem firstEdge_fieldLaws (Tx : Set (Finset τ)) (hsub : {s : Finset τ | s.card ≤ 1} ⊆ Tx) :
    FieldLaws firstOf some Tx {s | s.card ≤ 1} (fun v => v) := by
  refine ⟨fun s _ => card_firstOf_le s, ?_, ?_⟩
  · intro v hv s hs; simp only [Option.some.injEq] at hs; subst hs; exact hsub hv
  · intro v hv s hs
    simp only [Option.some.injEq] at hs; subst hs
    exact firstOf_of_card_le_one hv

/-- Its representable set: rows with at most one edge. -/
theorem firstEdge_rep (f₀ : F) (x : Row F (Finset τ)) :
    Rep (firstEdge f₀) {f₀} x ↔ (x f₀).card ≤ 1 := by
  rw [firstEdge, fieldLens_rep]
  simp only [Set.mem_singleton_iff, forall_const, Option.some.injEq]
  constructor
  · intro h; rw [← h]; exact card_firstOf_le _
  · exact firstOf_of_card_le_one

end First

section Default

variable {W : Type*}

/-- `default(c)` for a field made optional (required → optional): old versions, which
require a value, read a missing value as `c`. -/
def defaultRead (f₀ : F) (c : W) : Lens F (Option W) := fieldLens f₀ (fun s => some (s.getD c)) some

theorem defaultRead_fieldLaws (c : W) (Tx : Set (Option W)) (hsub : {s : Option W | s.isSome} ⊆ Tx) :
    FieldLaws (fun s => some (s.getD c)) some Tx {s | s.isSome} (fun v => v) := by
  refine ⟨fun s _ => by simp, ?_, ?_⟩
  · intro v hv s hs; simp only [Option.some.injEq] at hs; subst hs; exact hsub hv
  · intro v hv s hs
    simp only [Option.some.injEq] at hs; subst hs
    simp only [Set.mem_ofPred_eq] at hv
    obtain ⟨u, rfl⟩ := Option.isSome_iff_exists.mp hv; rfl

/-- **Its representable set excludes missing values**: `default(c)` is a lossy read. -/
theorem defaultRead_rep (f₀ : F) (c : W) (x : Row F (Option W)) :
    Rep (defaultRead f₀ c) {f₀} x ↔ (x f₀).isSome := by
  rw [defaultRead, fieldLens_rep]
  simp only [Set.mem_singleton_iff, forall_const, Option.some.injEq]
  cases x f₀ <;> simp

end Default

end Shep.Adapters
