/-
# Polarity never over-grants, under union minus deny (design.md §6.1, §6.5, §6.7, §7.6)

Restatement-round obligation 1 (§18). If a read decision isn't RevocationSensitive, admitting a
row at a stale state implies admitting it at every later state. So a stale read can only
under-grant, and the read needs no data-partition horizon.

This restates the earlier CEL model (now `PolarityCEL.lean`, superseded) for ShepQL:

* guards are **two-valued** (§4.0 item 1; `Totality.lean` proves a guard typed One evaluates to
  exactly one boolean), so there is no error case, no `ErrMode`, and no `allowError_overgrants`;
* a policy is **union minus deny**, `(∨A) ∧ ¬(∨D)` (`PolicyCombine.umd`), so rule order is
  gone, and RevocationSensitive asks *every* deny to be Stable⁻, not just the earlier ones;
* RevocationSensitive uses its **v1 form**: every allow guard is Stable (§7.6);
* **field policies** are part of the read decision (§6.1's field rule, `PolicyCombine.fieldOK`).

## The model

* **What policies read** (`Reads`): from a state, whether a node is created (has `@type`, so a
  placeholder isn't) and deleted, its field facts, and its edges. **Disciplines** (`Schema`)
  give frozen (`readonly`) fields and links, `shep::Permanent` types, and each link's target
  type.
* **The discipline order** `DLe σ τ` (`σ ≤_D τ`): creation only grows. A created node's frozen
  fields and links are equal. A permanent node isn't newly deleted. Mutable facts are
  unconstrained. `history_dle` shows that `V ⊆ V'` gives `stateAt V ≤_D stateAt V'` for every
  history whose entries pass the leader's discipline checks (`Disciplines.lean`).
* **Guards** (`Cond`), two-valued, over the node `.` and the request's viewer (`Option N`):
  - `fieldIs f P d`: `(… .f …) ?? d`, a test of the field's values, `d` when it is unset;
  - `viewerIn e`: `global viewer in .e`, **false with no viewer** (the per-atom rule, §6.1);
  - `hasViewer`: `exists global viewer`, an ordinary atom;
  - `can e`: `shep::visible(.e)` (per element, `any(…)` over a `multi` link): some target is
    visible to the same request; false on `{}`;
  - `not`, `and`, `or`, `true`, `false`.
* **Policies** (`Policy`): an optional `contained_in` link, the type's allows and denies, and,
  per field, the field's allows and denies.
* **`vis`** (§6.5), with fuel for the delegation depth: the node is created (so placeholders and
  absent nodes read as absent); it isn't deleted unless `include_deleted`; its `contained_in`
  container isn't deleted unless `include_deleted`; and `umd allows denies` holds. Delegation
  evaluates targets with `include_deleted = false`, so deleting a parent hides its children.
  **`fieldVis`** adds §6.1's field rule on top.

## RevocationSensitive, syntactically

`Stab G true c` (Stable⁺: true stays true) and `Stab G false c` (Stable⁻: false stays false)
track polarity through `not`. Field reads and `viewerIn` need frozen inputs; a `??` default
reads nothing that can change once the node exists, so any default is fine here (contrast
`Planner.lean`, where the node may not exist yet). `can e` needs a frozen link and a target
type whose own decision isn't sensitive (`G`). **`can e` with deny polarity is never Stable⁻:**
the flip turns the target's lifecycle into a negative input, and lifecycles grow (create,
fill). Polarity is tracked through `not`, so `not shep::visible(…)` inside a deny *is*
Stable⁻ when the `visible` is Stable⁺. A type's decision is **not RevocationSensitive**
(`Closed G`, `NotRS`) when:

* its type is permanent (`visible()`'s deletion check);
* its `contained_in` link is frozen, and the container's type is permanent;
* every allow guard is Stable⁺ (the v1 form);
* every deny guard, wherever it appears in the source, is Stable⁻.

A field's read decision is not RevocationSensitive (`FieldNotRS`) when, in addition, every
field allow is Stable⁺ and every field deny Stable⁻.

## Results

* `stab_sound`: the syntactic classes are sound.
* `vis_mono`, `notRS_mono`: **if a type's decision isn't RevocationSensitive, `σ ≤_D τ` and
  admission at `σ` imply admission at `τ`**, for every viewer (present or absent).
* `fieldVis_mono`: the same for a field's read decision.
* `history_admit_mono`, `history_fieldVis_mono`: the same for `stateAt V` and `stateAt V'`
  with `V ⊆ V'`, over any history that passes the leader's discipline checks.
* `port_safe` (with `PolicyCombine.port_exact`): **the Ent port preserves non-sensitivity.** If
  every allow is Stable⁺ and every deny Stable⁻, every rewritten deny `D_j ∧ ¬(∨_{i<j} A_i)` is
  Stable⁻, including when an `A_i` is a `shep::visible`.
* Counterexamples, each a decision that meets every other condition:
  * `denyMutable_overgrants`: a deny over a mutable link (a blocked list). Under union minus
    deny, the deny's place in the source doesn't matter.
  * `allowMutable_overgrants`: an allow over a mutable link (membership): why the v1 form asks
    every allow to be Stable⁺.
  * `ignoreVisible_overgrants`: stable guards on a non-permanent type. A classifier that
    ignores `visible()` over-grants after a soft delete.
  * `denyDelegate_overgrants`: `shep::visible` in a deny, to a type whose own decision isn't
    sensitive. The target is a placeholder at `σ` and filled at `τ`.
  * `fieldDenyMutable_overgrants`: a field deny over a mutable link: the field policies are
    part of the decision.
-/
import ShepZSet.Disciplines
import ShepZSet.PolicyCombine

noncomputable section
open Classical

namespace Shep
namespace Polarity

open PolicyCombine

variable {St N T F E Fl Val : Type*}

/-- What policies read from a state. -/
structure Reads (St N F E Val : Type*) where
  created : St → N → Prop
  deleted : St → N → Prop
  field : St → N → F → Val → Prop
  edge : St → N → E → N → Prop

/-- Types and declared disciplines. -/
structure Schema (N T F E : Type*) where
  ty : N → T
  permT : T → Prop
  frozenF : F → Prop
  frozenE : E → Prop
  tgtTy : E → T

/-- **The discipline order** `σ ≤_D τ`. -/
def DLe (R : Reads St N F E Val) (D : Schema N T F E) (σ τ : St) : Prop :=
  (∀ n, R.created σ n → R.created τ n) ∧
  (∀ n f v, D.frozenF f → R.created σ n → (R.field σ n f v ↔ R.field τ n f v)) ∧
  (∀ n e t, D.frozenE e → R.created σ n → (R.edge σ n e t ↔ R.edge τ n e t)) ∧
  (∀ n, D.permT (D.ty n) → R.deleted τ n → R.deleted σ n)

/-- Links point at nodes of their declared target type. -/
def Typed (R : Reads St N F E Val) (D : Schema N T F E) (σ : St) : Prop :=
  ∀ x e t, R.edge σ x e t → D.ty t = D.tgtTy e

/-- Two-valued policy guards. -/
inductive Cond (F E Val : Type*)
  | tt
  | ff
  /-- `(… .f …) ?? d`: every value of `.f` satisfies `P`, or `d` when `.f` is unset -/
  | fieldIs (f : F) (P : Val → Prop) (d : Bool)
  /-- `global viewer in .e`; false with no viewer -/
  | viewerIn (e : E)
  /-- `exists global viewer` -/
  | hasViewer
  /-- `shep::visible(.e)`: some target is visible to the same request; false on `{}` -/
  | can (e : E)
  | not (c : Cond F E Val)
  | and (c d : Cond F E Val)
  | or (c d : Cond F E Val)

/-- Whether a guard holds on node `x`, for viewer `v` (`none`: no viewer); `dv` is the
delegated decision. -/
def holds (R : Reads St N F E Val) (σ : St) (dv : N → Prop) (v : Option N) (x : N) :
    Cond F E Val → Prop
  | .tt => True
  | .ff => False
  | .fieldIs f P d => if ∃ a, R.field σ x f a then ∀ a, R.field σ x f a → P a else d = true
  | .viewerIn e => ∃ u, v = some u ∧ R.edge σ x e u
  | .hasViewer => v.isSome
  | .can e => ∃ t, R.edge σ x e t ∧ dv t
  | .not c => ¬ holds R σ dv v x c
  | .and c d => holds R σ dv v x c ∧ holds R σ dv v x d
  | .or c d => holds R σ dv v x c ∨ holds R σ dv v x d

/-- **The per-atom rule**: with no viewer, `viewerIn` is false, and its negation true. -/
theorem holds_viewerIn_none (R : Reads St N F E Val) (σ : St) (dv : N → Prop) (x : N) (e : E) :
    ¬ holds R σ dv none x (.viewerIn e) ∧ holds R σ dv none x (.not (.viewerIn e)) := by
  simp [holds]

/-- A type's policies: `contained_in`, the type's allows and denies, and each field's. -/
structure Policy (Fl F E Val : Type*) where
  contained : Option E
  allows : List (Cond F E Val)
  denies : List (Cond F E Val)
  fAllows : Fl → List (Cond F E Val)
  fDenies : Fl → List (Cond F E Val)

/-- `visible(node)` (§6.5) with `contained_in` (§6.7) and union minus deny (§6.1), at
delegation depth `k`. -/
def vis (R : Reads St N F E Val) (D : Schema N T F E) (pol : T → Policy Fl F E Val) :
    ℕ → St → Option N → N → Bool → Prop
  | 0, _, _, _, _ => False
  | k + 1, σ, v, x, incl =>
    R.created σ x ∧ (incl = true ∨ ¬ R.deleted σ x) ∧
    (∀ ce, (pol (D.ty x)).contained = some ce →
      incl = true ∨ ¬ ∃ t, R.edge σ x ce t ∧ R.deleted σ t) ∧
    umd (holds R σ (fun t => vis R D pol k σ v t false) v x)
      (pol (D.ty x)).allows (pol (D.ty x)).denies

/-- A field's read decision (§6.1): the node's decision, then the field rule. -/
def fieldVis (R : Reads St N F E Val) (D : Schema N T F E) (pol : T → Policy Fl F E Val)
    (k : ℕ) (σ : St) (v : Option N) (x : N) (incl : Bool) (fl : Fl) : Prop :=
  fieldOK (holds R σ (fun t => vis R D pol k σ v t false) v x) (vis R D pol (k + 1) σ v x incl)
    ((pol (D.ty x)).fAllows fl) ((pol (D.ty x)).fDenies fl)

/-! ## The syntactic classes -/

/-- `Stab G true c`: Stable⁺ (true stays true); `Stab G false c`: Stable⁻ (false stays
false). `G` is a set of types whose decisions aren't sensitive. -/
def Stab (D : Schema N T F E) (G : Set T) : Bool → Cond F E Val → Prop
  | _, .tt => True
  | _, .ff => True
  | _, .fieldIs f _ _ => D.frozenF f
  | _, .viewerIn e => D.frozenE e
  | _, .hasViewer => True
  | true, .can e => D.frozenE e ∧ D.tgtTy e ∈ G
  | false, .can _ => False
  | b, .not c => Stab D G (!b) c
  | b, .and c d => Stab D G b c ∧ Stab D G b d
  | b, .or c d => Stab D G b c ∧ Stab D G b d

/-- Every type in `G` has a non-sensitive decision, given that every type in `G` does
(RevocationSensitive's v1 form: every allow Stable⁺, every deny Stable⁻). -/
def Closed (D : Schema N T F E) (pol : T → Policy Fl F E Val) (G : Set T) : Prop :=
  ∀ T₀ ∈ G, D.permT T₀ ∧
    (∀ ce, (pol T₀).contained = some ce → D.frozenE ce ∧ D.permT (D.tgtTy ce)) ∧
    (∀ c ∈ (pol T₀).allows, Stab D G true c) ∧ (∀ c ∈ (pol T₀).denies, Stab D G false c)

/-- **A type's read decision isn't RevocationSensitive.** -/
def NotRS (D : Schema N T F E) (pol : T → Policy Fl F E Val) (T₀ : T) : Prop :=
  ∃ G, Closed D pol G ∧ T₀ ∈ G

/-- **A field's read decision isn't RevocationSensitive.** -/
def FieldNotRS (D : Schema N T F E) (pol : T → Policy Fl F E Val) (T₀ : T) (fl : Fl) : Prop :=
  ∃ G, Closed D pol G ∧ T₀ ∈ G ∧
    (∀ c ∈ (pol T₀).fAllows fl, Stab D G true c) ∧ (∀ c ∈ (pol T₀).fDenies fl, Stab D G false c)

/-! ## Soundness -/

/-- `Keeps true p q`: `p → q`; `Keeps false p q`: `¬p → ¬q`. -/
def Keeps : Bool → Prop → Prop → Prop
  | true, p, q => p → q
  | false, p, q => ¬ p → ¬ q

theorem keeps_of_iff {p q : Prop} (h : p ↔ q) : ∀ b, Keeps b p q
  | true => h.mp
  | false => fun hp hq => hp (h.mpr hq)

variable {R : Reads St N F E Val} {D : Schema N T F E}

/-- **Stable⁺ and Stable⁻ are sound.** Given `σ ≤_D τ`, a created node, and a delegated
decision that is monotone on the types in `G`, a Stable⁺ guard that holds at `σ` holds at `τ`,
and a Stable⁻ guard that fails at `σ` fails at `τ`; for every viewer. -/
theorem stab_sound {G : Set T} {σ τ : St} (hle : DLe R D σ τ) (hty : Typed R D σ) {v : Option N}
    {x : N} (hx : R.created σ x) {dσ dτ : N → Prop} (hd : ∀ t, D.ty t ∈ G → dσ t → dτ t) :
    ∀ (c : Cond F E Val) (b : Bool), Stab D G b c →
      Keeps b (holds R σ dσ v x c) (holds R τ dτ v x c) := by
  intro c
  induction c with
  | tt => intro b _; exact keeps_of_iff Iff.rfl b
  | ff => intro b _; exact keeps_of_iff Iff.rfl b
  | fieldIs f P d =>
    intro b hs
    apply keeps_of_iff
    have h : ∀ a, R.field σ x f a ↔ R.field τ x f a := fun a => hle.2.1 x f a hs hx
    simp only [holds, exists_congr h, forall_congr' (fun a => imp_congr_left (h a))]
  | viewerIn e =>
    intro b hs
    apply keeps_of_iff
    simp only [holds]
    exact exists_congr (fun u => and_congr_right (fun _ => hle.2.2.1 x e u hs hx))
  | hasViewer => intro b _; exact keeps_of_iff Iff.rfl b
  | can e =>
    intro b hs
    cases b with
    | false => exact absurd hs (by simp [Stab])
    | true =>
      obtain ⟨hfe, hG⟩ := hs
      rintro ⟨t, ht, hdt⟩
      exact ⟨t, (hle.2.2.1 x e t hfe hx).mp ht, hd t (by rw [hty x e t ht]; exact hG) hdt⟩
  | not c ih =>
    intro b hs
    have := ih (!b) hs
    cases b with
    | true => exact fun h1 h2 => this h1 h2
    | false => exact fun h1 h2 => h2 (this (not_not.mp h1))
  | and c d ihc ihd =>
    intro b hs
    have h1 := ihc b hs.1
    have h2 := ihd b hs.2
    cases b with
    | true => exact fun h => ⟨h1 h.1, h2 h.2⟩
    | false =>
      intro h hτ
      by_cases hc : holds R σ dσ v x c
      · exact h2 (fun hd => h ⟨hc, hd⟩) hτ.2
      · exact h1 hc hτ.1
  | or c d ihc ihd =>
    intro b hs
    have h1 := ihc b hs.1
    have h2 := ihd b hs.2
    cases b with
    | true => exact fun h => h.elim (fun h => Or.inl (h1 h)) (fun h => Or.inr (h2 h))
    | false =>
      intro h hτ
      exact hτ.elim (h1 (fun hc => h (Or.inl hc))) (h2 (fun hd => h (Or.inr hd)))

/-- Union minus deny is monotone when every allow is Stable⁺ and every deny Stable⁻. -/
theorem umd_keep {G : Set T} {σ τ : St} (hle : DLe R D σ τ) (hty : Typed R D σ) {v : Option N}
    {x : N} (hx : R.created σ x) {dσ dτ : N → Prop} (hd : ∀ t, D.ty t ∈ G → dσ t → dτ t)
    {A Dn : List (Cond F E Val)} (hA : ∀ c ∈ A, Stab D G true c)
    (hD : ∀ c ∈ Dn, Stab D G false c)
    (h : umd (holds R σ dσ v x) A Dn) : umd (holds R τ dτ v x) A Dn := by
  obtain ⟨⟨a, ha, hav⟩, hn⟩ := h
  refine ⟨⟨a, ha, stab_sound hle hty hx hd a true (hA a ha) hav⟩, ?_⟩
  rintro ⟨c, hc, hcv⟩
  exact stab_sound hle hty hx hd c false (hD c hc) (fun h => hn ⟨c, hc, h⟩) hcv

/-- **Polarity never over-grants.** If every type in `G` has a non-sensitive decision
(`Closed G`), a node of a type in `G` that is visible at `σ` is visible at every `τ` with
`σ ≤_D τ`, for every viewer (present or absent), delegation depth, and `include_deleted`. -/
theorem vis_mono {pol : T → Policy Fl F E Val} {G : Set T} (hG : Closed D pol G) {σ τ : St}
    (hle : DLe R D σ τ) (hty : Typed R D σ) :
    ∀ k v x incl, D.ty x ∈ G → vis R D pol k σ v x incl → vis R D pol k τ v x incl := by
  intro k
  induction k with
  | zero => intro _ _ _ _ h; exact h
  | succ k ih =>
    intro v x incl hx h
    obtain ⟨hperm, hcont, hA, hD⟩ := hG _ hx
    obtain ⟨hc, hdel, hcon, hr⟩ := h
    refine ⟨hle.1 x hc, ?_, ?_, ?_⟩
    · rcases hdel with h | h
      · exact Or.inl h
      · exact Or.inr (fun h' => h (hle.2.2.2 x hperm h'))
    · intro ce hce
      rcases hcon ce hce with h | h
      · exact Or.inl h
      · right
        rintro ⟨t, ht, htd⟩
        obtain ⟨hfe, hpt⟩ := hcont ce hce
        have htσ := (hle.2.2.1 x ce t hfe hc).mpr ht
        have hpt' : D.permT (D.ty t) := by rw [hty x ce t htσ]; exact hpt
        exact h ⟨t, htσ, hle.2.2.2 t hpt' htd⟩
    · exact umd_keep hle hty hc (fun t ht h => ih v t false ht h) hA hD hr

/-- **The decision-level statement**: a type whose read decision isn't RevocationSensitive. -/
theorem notRS_mono {pol : T → Policy Fl F E Val} {σ τ : St} (hle : DLe R D σ τ)
    (hty : Typed R D σ) {k : ℕ} {v : Option N} {x : N} {incl : Bool} (hrs : NotRS D pol (D.ty x))
    (h : vis R D pol k σ v x incl) : vis R D pol k τ v x incl := by
  obtain ⟨G, hG, hx⟩ := hrs
  exact vis_mono hG hle hty k v x incl hx h

/-- **A field's read decision never over-grants when stale**, if it isn't
RevocationSensitive. -/
theorem fieldVis_mono {pol : T → Policy Fl F E Val} {σ τ : St} (hle : DLe R D σ τ)
    (hty : Typed R D σ) {k : ℕ} {v : Option N} {x : N} {incl : Bool} {fl : Fl}
    (hrs : FieldNotRS D pol (D.ty x) fl) (h : fieldVis R D pol k σ v x incl fl) :
    fieldVis R D pol k τ v x incl fl := by
  obtain ⟨G, hG, hx, hA, hD⟩ := hrs
  obtain ⟨hv, hfa, hfd⟩ := h
  have hc : R.created σ x := hv.1
  have hd : ∀ t, D.ty t ∈ G → vis R D pol k σ v t false → vis R D pol k τ v t false :=
    fun t ht h => vis_mono hG hle hty k v t false ht h
  refine ⟨vis_mono hG hle hty (k + 1) v x incl hx hv, ?_, ?_⟩
  · rcases hfa with h | ⟨a, ha, hav⟩
    · exact Or.inl h
    · exact Or.inr ⟨a, ha, stab_sound hle hty hc hd a true (hA a ha) hav⟩
  · rintro ⟨c, hcm, hcv⟩
    exact stab_sound hle hty hc hd c false (hD c hcm) (fun h => hfd ⟨c, hcm, h⟩) hcv

/-- Placeholders and absent nodes are never visible. -/
theorem vis_not_created {pol : T → Policy Fl F E Val} {σ : St} {k : ℕ} {v : Option N} {x : N}
    {incl : Bool} (h : ¬ R.created σ x) : ¬ vis R D pol k σ v x incl := by
  cases k with
  | zero => exact id
  | succ k => exact fun hv => h hv.1

/-! ## The Ent port preserves non-sensitivity -/

/-- `∨` of a list of guards. -/
def orList : List (Cond F E Val) → Cond F E Val
  | [] => .ff
  | c :: cs => .or c (orList cs)

/-- A ported guard as a ShepQL guard: `D_j ∧ ¬(∨_{i<j} A_i)`. -/
def portGuard : PGuard (Cond F E Val) → Cond F E Val
  | .base g => g
  | .guarded d as => .and d (.not (orList as))

theorem holds_orList (σ : St) (dv : N → Prop) (v : Option N) (x : N) :
    ∀ cs : List (Cond F E Val), holds R σ dv v x (orList cs) ↔ ∃ c ∈ cs, holds R σ dv v x c
  | [] => by simp [orList, holds]
  | c :: cs => by simp [orList, holds, holds_orList σ dv v x cs]

theorem holds_portGuard (σ : St) (dv : N → Prop) (v : Option N) (x : N)
    (g : PGuard (Cond F E Val)) :
    holds R σ dv v x (portGuard g) ↔ PGuard.ev (holds R σ dv v x) g := by
  cases g with
  | base g => rfl
  | guarded d as => simp [portGuard, PGuard.ev, holds, holds_orList]

theorem stab_orList {G : Set T} (b : Bool) :
    ∀ cs : List (Cond F E Val), (∀ c ∈ cs, Stab D G b c) → Stab D G b (orList cs)
  | [] => fun _ => by cases b <;> trivial
  | c :: cs => fun h => ⟨h c List.mem_cons_self,
      stab_orList b cs (fun c' hc' => h c' (List.mem_cons_of_mem _ hc'))⟩

/-- A rule is safe: an allow Stable⁺, a deny Stable⁻. -/
def SafeRule (D : Schema N T F E) (G : Set T) : Rule (Cond F E Val) → Prop
  | .allow c => Stab D G true c
  | .deny c => Stab D G false c

theorem portDenies_safe {G : Set T} :
    ∀ (rs : List (Rule (Cond F E Val))) (seen : List (Cond F E Val)),
      (∀ a ∈ seen, Stab D G true a) → (∀ r ∈ rs, SafeRule D G r) →
        ∀ g ∈ portDenies seen rs, Stab D G false (portGuard g) := by
  intro rs
  induction rs with
  | nil => intro _ _ _ g hg; simp [portDenies] at hg
  | cons r rs ih =>
    intro seen hseen hrs g hg
    have hr := hrs r List.mem_cons_self
    have hrs' := fun r' hr' => hrs r' (List.mem_cons_of_mem _ hr')
    cases r with
    | allow a =>
      refine ih (seen ++ [a]) ?_ hrs' g hg
      intro c hc
      rcases List.mem_append.mp hc with hc | hc
      · exact hseen c hc
      · rw [List.mem_singleton.mp hc]; exact hr
    | deny d =>
      simp only [portDenies, List.mem_cons] at hg
      rcases hg with rfl | hg
      · exact ⟨hr, stab_orList true seen hseen⟩
      · exact ih seen hseen hrs' g hg

/-- **The Ent port preserves non-sensitivity.** If every allow of an ordered rule list is
Stable⁺ and every deny Stable⁻, the ported policy's allows are Stable⁺ and its rewritten
denies `D_j ∧ ¬(∨_{i<j} A_i)` are Stable⁻, even when an `A_i` is a `shep::visible`
(`port_visible_safe`). -/
theorem port_safe {G : Set T} (rs : List (Rule (Cond F E Val)))
    (hrs : ∀ r ∈ rs, SafeRule D G r) :
    (∀ g ∈ (port rs).1, Stab D G true (portGuard g)) ∧
      (∀ g ∈ (port rs).2, Stab D G false (portGuard g)) := by
  refine ⟨?_, portDenies_safe rs [] (by simp) hrs⟩
  intro g hg
  simp only [port, List.mem_map] at hg
  obtain ⟨a, ha, rfl⟩ := hg
  suffices ∀ rs : List (Rule (Cond F E Val)), (∀ r ∈ rs, SafeRule D G r) →
      ∀ a ∈ allows rs, Stab D G true a from this rs hrs a ha
  intro rs
  induction rs with
  | nil => intro _ a ha; simp [allows] at ha
  | cons r rs ih =>
    intro hrs a ha
    have hrs' := fun r' hr' => hrs r' (List.mem_cons_of_mem _ hr')
    cases r with
    | allow b =>
      simp only [allows, List.mem_cons] at ha
      rcases ha with rfl | ha
      · exact hrs _ List.mem_cons_self
      · exact ih hrs' a ha
    | deny _ => exact ih hrs' a ha

/-- `not shep::visible(…)` inside a deny is Stable⁻ when the `visible` is Stable⁺: §7.6's rule
"`shep::visible` inside a deny is RevocationSensitive outright" is sound but stricter than
needed for the denies the Ent port produces. -/
theorem port_visible_safe {G : Set T} {e : E} (hfe : D.frozenE e) (hG : D.tgtTy e ∈ G) :
    Stab D G false (portGuard (.guarded (.ff : Cond F E Val) [.can e])) :=
  ⟨trivial, ⟨hfe, hG⟩, trivial⟩

/-! ## Histories that pass the leader's checks give the discipline order -/

section History

variable {Seg Dot : Type*}

/-- Policies' reads of a fact state (§10.1): `@type`, `@deleted_at`, field facts
`(n, fpath f, v)`, and link facts `(n, epath e t, sortKey)`. -/
def factReads (sch : Disc.Schema N Seg) (fpath : F → Path Seg) (epath : E → N → Path Seg) :
    Reads (ZSet (Fact N Seg Val)) N F E Val where
  created σ n := Disc.Created sch σ n
  deleted σ n := Disc.Deleted sch σ n
  field σ n f v := σ (n, fpath f, v) ≠ 0
  edge σ n e t := ∃ s, σ (n, epath e t, s) ≠ 0

/-- **Discipline-respecting histories are ordered by `≤_D`.** If every entry passes the
leader's frozen and permanent checks, and the declared field, link, and type disciplines match
the fact-level ones, then `V ⊆ V'` gives `stateAt V ≤_D stateAt V'`. -/
theorem history_dle {sch : Disc.Schema N Seg} {fpath : F → Path Seg} {epath : E → N → Path Seg}
    {delta : Dot → ZSet (Fact N Seg Val)} {ctx : Dot → Finset Dot}
    (hH : Disc.Hist delta ctx) (hF : ∀ d, Disc.FrozenOK sch delta ctx d)
    (hP : ∀ d, Disc.PermOK sch delta d)
    (hfF : ∀ f, D.frozenF f → sch.frz (fpath f)) (hfE : ∀ e t, D.frozenE e → sch.frz (epath e t))
    (hperm : ∀ n, D.permT (D.ty n) → sch.perm n) {X X' : Finset Dot} (hXX : X ⊆ X') :
    DLe (factReads sch fpath epath) D (stateAt delta X) (stateAt delta X') := by
  refine ⟨fun n h => Disc.created_mono hH hF hXX h, ?_, ?_, ?_⟩
  · intro n f v hf hn
    show stateAt delta X (n, fpath f, v) ≠ 0 ↔ stateAt delta X' (n, fpath f, v) ≠ 0
    rw [Disc.frozen_mono hH hF hXX hn (hfF f hf)]
  · intro n e t he hn
    show (∃ s, stateAt delta X (n, epath e t, s) ≠ 0) ↔ (∃ s, stateAt delta X' (n, epath e t, s) ≠ 0)
    simp only [Disc.frozen_mono hH hF hXX hn (hfE e t he)]
  · intro n hn hd
    exact absurd hd (Disc.perm_not_deleted hP (hperm n hn) X')

/-- **Polarity never over-grants, over histories.** For a history that passes the leader's
discipline checks, a row whose read decision isn't RevocationSensitive and that is admitted at
`V` is admitted at every `V' ⊇ V`. -/
theorem history_admit_mono {sch : Disc.Schema N Seg} {fpath : F → Path Seg}
    {epath : E → N → Path Seg} {delta : Dot → ZSet (Fact N Seg Val)} {ctx : Dot → Finset Dot}
    (hH : Disc.Hist delta ctx) (hF : ∀ d, Disc.FrozenOK sch delta ctx d)
    (hP : ∀ d, Disc.PermOK sch delta d)
    (hfF : ∀ f, D.frozenF f → sch.frz (fpath f)) (hfE : ∀ e t, D.frozenE e → sch.frz (epath e t))
    (hperm : ∀ n, D.permT (D.ty n) → sch.perm n) {pol : T → Policy Fl F E Val}
    {X X' : Finset Dot} (hXX : X ⊆ X')
    (hty : Typed (factReads sch fpath epath) D (stateAt delta X)) {k : ℕ} {v : Option N} {x : N}
    {incl : Bool} (hrs : NotRS D pol (D.ty x))
    (h : vis (factReads sch fpath epath) D pol k (stateAt delta X) v x incl) :
    vis (factReads sch fpath epath) D pol k (stateAt delta X') v x incl :=
  notRS_mono (history_dle hH hF hP hfF hfE hperm hXX) hty hrs h

/-- The same for a field's read decision. -/
theorem history_fieldVis_mono {sch : Disc.Schema N Seg} {fpath : F → Path Seg}
    {epath : E → N → Path Seg} {delta : Dot → ZSet (Fact N Seg Val)} {ctx : Dot → Finset Dot}
    (hH : Disc.Hist delta ctx) (hF : ∀ d, Disc.FrozenOK sch delta ctx d)
    (hP : ∀ d, Disc.PermOK sch delta d)
    (hfF : ∀ f, D.frozenF f → sch.frz (fpath f)) (hfE : ∀ e t, D.frozenE e → sch.frz (epath e t))
    (hperm : ∀ n, D.permT (D.ty n) → sch.perm n) {pol : T → Policy Fl F E Val}
    {X X' : Finset Dot} (hXX : X ⊆ X')
    (hty : Typed (factReads sch fpath epath) D (stateAt delta X)) {k : ℕ} {v : Option N} {x : N}
    {incl : Bool} {fl : Fl} (hrs : FieldNotRS D pol (D.ty x) fl)
    (h : fieldVis (factReads sch fpath epath) D pol k (stateAt delta X) v x incl fl) :
    fieldVis (factReads sch fpath epath) D pol k (stateAt delta X') v x incl fl :=
  fieldVis_mono (history_dle hH hF hP hfF hfE hperm hXX) hty hrs h

end History

/-! ## Counterexamples

Each uses two states, `false` (stale, `σ`) and `true` (fresh, `τ`), with `σ ≤_D τ`, a decision
that meets every condition but one, admission at `σ`, and denial at `τ`. The viewer is
present (`some ()`). -/

/-- No field policies. -/
def noFields : Unit → List (Cond Unit Unit Unit) := fun _ => []

/-- One node, which is also the viewer. A `blocked` link from the node to the viewer appears
at `τ`. -/
def blkReads : Reads Bool Unit Unit Unit Unit where
  created _ _ := True
  deleted _ _ := False
  field _ _ _ _ := False
  edge s _ _ _ := s = true

/-- A permanent type, but `blocked` is mutable. -/
def blkSch : Schema Unit Unit Unit Unit where
  ty _ := ()
  permT _ := True
  frozenF _ := True
  frozenE _ := False
  tgtTy _ := ()

theorem blk_dle : DLe blkReads blkSch false true ∧ Typed blkReads blkSch false :=
  ⟨⟨fun _ h => h, fun _ _ _ _ _ => Iff.rfl, fun _ _ _ h => absurd h (by simp [blkSch]),
    fun _ _ h => h⟩, fun _ _ _ _ => rfl⟩

/-- `allow select using (true); deny select using (global viewer in .blocked)`. Under union
minus deny, it makes no difference where the deny appears in the source. -/
def blkPol : Unit → Policy Unit Unit Unit Unit := fun _ =>
  { contained := none, allows := [.tt], denies := [.viewerIn ()], fAllows := noFields,
    fDenies := noFields }

theorem denyMutable_overgrants :
    DLe blkReads blkSch false true ∧ Typed blkReads blkSch false ∧
    -- the type is permanent and the allow is Stable⁺; the deny isn't Stable⁻
    blkSch.permT () ∧ Stab blkSch (∅ : Set Unit) true (Cond.tt : Cond Unit Unit Unit) ∧
    ¬ Stab blkSch (∅ : Set Unit) false (Cond.viewerIn () : Cond Unit Unit Unit) ∧
    vis blkReads blkSch blkPol 1 false (some ()) () false ∧
    ¬ vis blkReads blkSch blkPol 1 true (some ()) () false := by
  refine ⟨blk_dle.1, blk_dle.2, trivial, trivial, by simp [Stab, blkSch], ?_, ?_⟩
  · simp [vis, blkReads, blkPol, umd, holds]
  · simp [vis, blkReads, blkPol, umd, holds]

/-- A `members` link from the node to the viewer, removed at `τ`; `members` is mutable. -/
def memReads : Reads Bool Unit Unit Unit Unit where
  created _ _ := True
  deleted _ _ := False
  field _ _ _ _ := False
  edge s _ _ _ := s = false

/-- `allow select using (global viewer in .members)`. -/
def memPol : Unit → Policy Unit Unit Unit Unit := fun _ =>
  { contained := none, allows := [.viewerIn ()], denies := [], fAllows := noFields,
    fDenies := noFields }

/-- **Why the v1 form asks every allow to be Stable⁺**: an allow over mutable membership. -/
theorem allowMutable_overgrants :
    DLe memReads blkSch false true ∧ Typed memReads blkSch false ∧
    blkSch.permT () ∧
    ¬ Stab blkSch (∅ : Set Unit) true (Cond.viewerIn () : Cond Unit Unit Unit) ∧
    vis memReads blkSch memPol 1 false (some ()) () false ∧
    ¬ vis memReads blkSch memPol 1 true (some ()) () false := by
  refine ⟨⟨fun _ h => h, fun _ _ _ _ _ => Iff.rfl, fun _ _ _ h => absurd h (by simp [blkSch]),
    fun _ _ h => h⟩, fun _ _ _ _ => rfl, trivial, by simp [Stab, blkSch], ?_, ?_⟩
  · simp [vis, memReads, memPol, umd, holds]
  · simp [vis, memReads, memPol, umd, holds]

/-- One node, deleted at `τ`. -/
def delReads : Reads Bool Unit Unit Unit Unit where
  created _ _ := True
  deleted s _ := s = true
  field _ _ _ _ := False
  edge _ _ _ _ := False

/-- A non-permanent type; everything else frozen. -/
def delSch : Schema Unit Unit Unit Unit where
  ty _ := ()
  permT _ := False
  frozenF _ := True
  frozenE _ := True
  tgtTy _ := ()

def allowPol : Unit → Policy Unit Unit Unit Unit := fun _ =>
  { contained := none, allows := [.tt], denies := [], fAllows := noFields, fDenies := noFields }

theorem ignoreVisible_overgrants :
    DLe delReads delSch false true ∧ Typed delReads delSch false ∧
    -- every guard is safe: a guards-only classifier would call the decision non-sensitive
    (∀ c ∈ (allowPol ()).allows, Stab delSch (Set.univ : Set Unit) true c) ∧
    (allowPol ()).denies = [] ∧ ¬ delSch.permT () ∧
    vis delReads delSch allowPol 1 false (some ()) () false ∧
    ¬ vis delReads delSch allowPol 1 true (some ()) () false := by
  refine ⟨⟨fun _ h => h, fun _ _ _ _ _ => Iff.rfl, fun _ _ _ _ _ => Iff.rfl,
    fun _ h => absurd h (by simp [delSch])⟩, fun _ _ _ h => absurd h (by simp [delReads]),
    ?_, rfl, by simp [delSch], ?_, ?_⟩
  · intro c hc
    simp only [allowPol, List.mem_singleton] at hc
    subst hc; trivial
  · simp [vis, delReads, allowPol, umd, holds]
  · simp [vis, delReads]

/-- Two nodes: `false` (the row) and `true` (its parent), typed by themselves. The frozen
`parent` link goes from the row to the parent. The parent is a placeholder at `σ` and filled
at `τ`. -/
def phReads : Reads Bool Bool Unit Unit Unit where
  created s n := n = false ∨ s = true
  deleted _ _ := False
  field _ _ _ _ := False
  edge _ x _ t := x = false ∧ t = true

def phSch : Schema Bool Bool Unit Unit where
  ty n := n
  permT _ := True
  frozenF _ := True
  frozenE _ := True
  tgtTy _ := true

theorem phSch_dle : DLe phReads phSch false true ∧ Typed phReads phSch false :=
  ⟨⟨fun _ _ => Or.inr rfl, fun _ _ _ _ _ => Iff.rfl, fun _ _ _ _ _ => Iff.rfl, fun _ _ h => h⟩,
    fun _ _ _ h => h.2⟩

/-- The row: `allow using (true); deny using (shep::visible(.parent))`. The parent:
`allow using (true)`. -/
def ddPol : Bool → Policy Unit Unit Unit Unit
  | false => { contained := none, allows := [.tt], denies := [.can ()], fAllows := noFields,
               fDenies := noFields }
  | true => { contained := none, allows := [.tt], denies := [], fAllows := noFields,
              fDenies := noFields }

/-- The parent's type has a non-sensitive decision. -/
theorem ddPol_parent_notRS : NotRS phSch ddPol true := by
  refine ⟨{true}, ?_, rfl⟩
  intro T₀ hT
  rw [Set.mem_singleton_iff] at hT
  subst hT
  refine ⟨trivial, fun ce h => by simp [ddPol] at h, ?_, ?_⟩
  · intro c hc
    simp only [ddPol, List.mem_singleton] at hc
    subst hc; trivial
  · intro c hc; simp [ddPol] at hc

theorem denyDelegate_overgrants :
    DLe phReads phSch false true ∧ Typed phReads phSch false ∧
    -- the delegated decision isn't sensitive, the row's type is permanent, its allow is
    -- Stable⁺, but the `visible` sits in a deny
    NotRS phSch ddPol true ∧ phSch.permT false ∧
    ¬ Stab phSch ({true} : Set Bool) false (Cond.can () : Cond Unit Unit Unit) ∧
    vis phReads phSch ddPol 2 false (some false) false false ∧
    ¬ vis phReads phSch ddPol 2 true (some false) false false := by
  refine ⟨phSch_dle.1, phSch_dle.2, ddPol_parent_notRS, trivial, by simp [Stab], ?_, ?_⟩
  · simp [vis, phReads, phSch, ddPol, umd, holds]
  · simp [vis, phReads, phSch, ddPol, umd, holds]

/-- The type allows everything; the field has `deny select using (global viewer in .blocked)`,
over the mutable `blocked` link of `blkReads`. -/
def fdPol : Unit → Policy Unit Unit Unit Unit := fun _ =>
  { contained := none, allows := [.tt], denies := [], fAllows := noFields,
    fDenies := fun _ => [.viewerIn ()] }

/-- **Field policies are part of the decision**: the node's decision isn't sensitive, but the
field's deny reads a mutable link. -/
theorem fieldDenyMutable_overgrants :
    DLe blkReads blkSch false true ∧ Typed blkReads blkSch false ∧
    NotRS blkSch fdPol () ∧
    fieldVis blkReads blkSch fdPol 0 false (some ()) () false () ∧
    ¬ fieldVis blkReads blkSch fdPol 0 true (some ()) () false () := by
  refine ⟨blk_dle.1, blk_dle.2, ⟨Set.univ, ?_, trivial⟩, ?_, ?_⟩
  · intro T₀ _
    refine ⟨trivial, fun ce h => by simp [fdPol] at h, ?_, ?_⟩
    · intro c hc
      simp only [fdPol, List.mem_singleton] at hc
      subst hc; trivial
    · intro c hc; simp [fdPol] at hc
  · simp [fieldVis, fieldOK, vis, blkReads, fdPol, noFields, umd, holds]
  · simp [fieldVis, fieldOK, vis, blkReads, fdPol, umd, holds]

end Polarity
end Shep
