/-
# SUPERSEDED: polarity under CEL's first-match rules (design.md, earlier §6.1 and §7.6)

**Superseded by `Polarity.lean`**, which restates these results for ShepQL: two-valued guards,
union minus deny, and every allow Stable. This file is kept as history. Its `ErrMode`,
`ErrFree`, `Unch`, the error clause of `SafeRule`, and `allowError_overgrants` model CEL's
runtime errors, which ShepQL guards can't have (§4.0 item 1, `Totality.lean`), so they are
moot. Its namespace is `Shep.PolarityCEL`.

The original header follows.

# Polarity never over-grants (design.md §6.1, §6.3, §6.5, §6.7, §7.6)

Planner obligation 3. If a read decision isn't RevocationSensitive, admitting a row at a stale
state implies admitting it at every later state. So a stale read can only under-grant, and the
read needs no data-partition horizon.

## The model

* **What policies read** (`Reads`): from a state, whether a node is created (has `@type`, so a
  placeholder isn't) and deleted, its field facts, and its edges. **Disciplines** (`Schema`)
  give frozen fields and edges, `#[permanent]` types, and each edge's target type.
* **The discipline order** `DLe σ τ` (`σ ≤_D τ`): creation only grows. A created node's frozen
  fields and edges are equal. A permanent node isn't newly deleted. Mutable facts are
  unconstrained. `history_dle` shows that `V ⊆ V'` gives `stateAt V ≤_D stateAt V'` for every
  history whose entries pass the leader's discipline checks (`Disciplines.lean`).
* **Conditions** (`Cond`): CEL over the node, three-valued (`none` is an error), with CEL's
  commutative `&&`/`||` (a `false` or `true` absorbs an error). Reading an unset field is an
  error. `viewerIn e` is "viewer in `node.e`". `can e` is the delegated decision: some target of
  `node.e` is visible to the same viewer.
* **Policies**: an optional `contained_in` edge, then ordered `allow`/`deny` rules. The first
  non-Skip rule decides, every rule skipping denies, and errors count as Deny (§6.1). The
  alternative `ErrMode.skipAllow` treats an error in an Allow guard as Skip, which also fails
  closed. `delegate(e)` is `allow if can e`, and a delegation inside a Deny is `deny if … can e …`.
* **`vis`** (§6.5), with fuel for the delegation depth: the node is created, so placeholders
  and absent nodes read as absent. It isn't deleted unless `include_deleted`. Its
  `contained_in` container isn't deleted unless `include_deleted`. Its rules admit. Delegation
  evaluates targets with `include_deleted = false`, so deleting a parent hides its children.

## RevocationSensitive, syntactically

`Stab G true c` (Stable⁺, true stays true) and `Stab G false c` (Stable⁻, false stays false)
track polarity through `not`. Field reads and `viewerIn` need frozen inputs. `can e` needs a
frozen edge and a target type whose own decision isn't sensitive (`G`). **Inside a Deny,
`can e` is never Stable⁻:** the flip turns the target's lifecycle into a negative input, and
lifecycles grow (create, fill). A type's decision is **not RevocationSensitive** (`Closed G`,
`NotRS`) when:

* its type is permanent (`visible()`'s deletion check);
* its `contained_in` edge is frozen, and the container's type is permanent;
* every Deny guard is Stable⁻;
* every Allow guard is Stable⁺, and **under errors-as-Deny, also can't turn from false into an
  error**: it is error-free (no field reads) or unchanging (`SafeRule`).

## Results

* `stab_sound`, `unch_sound`, `errFree_sound`: the syntactic classes are sound.
* `vis_mono`: **if a type's decision isn't RevocationSensitive, `σ ≤_D τ` and admission at
  `σ` imply admission at `τ`.**
* `history_admit_mono`: the same for `stateAt V` and `stateAt V'` with `V ⊆ V'`, over any
  history that passes the leader's discipline checks.
* Counterexamples, each a decision that meets every other condition:
  * `denyMutable_overgrants`: a Deny over a mutable edge (a blocked list).
  * `ignoreVisible_overgrants`: stable rules on a non-permanent type. A classifier that
    ignores `visible()` over-grants after a soft delete.
  * `denyDelegate_overgrants`: a delegation inside a Deny, to a type whose own decision isn't
    sensitive. The target is a placeholder at `σ` and filled at `τ`.
  * `allowError_overgrants` (**new**): every Allow guard is Stable⁺ and there is no Deny,
    but an earlier Allow guard `delegate(e) && node.code == x`, with `code` frozen and unset,
    is `false` at `σ` and an error at `τ`. Errors count as Deny, so the row is admitted at `σ`
    and denied at `τ`. Under `skipAllow` it is admitted at both.
-/
import ShepZSet.Disciplines

noncomputable section
open Classical

namespace Shep
namespace PolarityCEL

variable {St N T F E Val : Type*}

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

/-- Edges point at nodes of their declared target type. -/
def Typed (R : Reads St N F E Val) (D : Schema N T F E) (σ : St) : Prop :=
  ∀ x e t, R.edge σ x e t → D.ty t = D.tgtTy e

/-- Policy conditions. -/
inductive Cond (F E Val : Type*)
  | tt
  | ff
  /-- `node.f` satisfies `P`; an error if `f` is unset -/
  | fieldIs (f : F) (P : Val → Prop)
  /-- `viewer in node.e` -/
  | viewerIn (e : E)
  /-- `delegate(node.e)`: some target is visible to the viewer -/
  | can (e : E)
  | not (c : Cond F E Val)
  | and (c d : Cond F E Val)
  | or (c d : Cond F E Val)

/-- CEL `!`. -/
def oNot : Option Bool → Option Bool := Option.map (fun b => !b)

/-- CEL `&&`: commutative, and `false` absorbs an error. -/
def oAnd : Option Bool → Option Bool → Option Bool
  | some false, _ => some false
  | _, some false => some false
  | some true, some true => some true
  | _, _ => none

/-- CEL `||`: commutative, and `true` absorbs an error. -/
def oOr : Option Bool → Option Bool → Option Bool
  | some true, _ => some true
  | _, some true => some true
  | some false, some false => some false
  | _, _ => none

theorem oNot_eq (a : Option Bool) (b : Bool) : oNot a = some b ↔ a = some (!b) := by
  cases a with
  | none => simp [oNot]
  | some a => cases a <;> cases b <;> simp [oNot]

theorem oAnd_true (a c : Option Bool) : oAnd a c = some true ↔ a = some true ∧ c = some true := by
  rcases a with _ | _ | _ <;> rcases c with _ | _ | _ <;> simp [oAnd]

theorem oAnd_false (a c : Option Bool) :
    oAnd a c = some false ↔ a = some false ∨ c = some false := by
  rcases a with _ | _ | _ <;> rcases c with _ | _ | _ <;> simp [oAnd]

theorem oOr_true (a c : Option Bool) : oOr a c = some true ↔ a = some true ∨ c = some true := by
  rcases a with _ | _ | _ <;> rcases c with _ | _ | _ <;> simp [oOr]

theorem oOr_false (a c : Option Bool) :
    oOr a c = some false ↔ a = some false ∧ c = some false := by
  rcases a with _ | _ | _ <;> rcases c with _ | _ | _ <;> simp [oOr]

/-- Evaluate a condition on node `x` for viewer `v`; `dv` is the delegated decision. -/
def eval (R : Reads St N F E Val) (σ : St) (dv : N → Prop) (v x : N) :
    Cond F E Val → Option Bool
  | .tt => some true
  | .ff => some false
  | .fieldIs f P =>
    if ∃ a, R.field σ x f a then some (decide (∀ a, R.field σ x f a → P a)) else none
  | .viewerIn e => some (decide (R.edge σ x e v))
  | .can e => some (decide (∃ t, R.edge σ x e t ∧ dv t))
  | .not c => oNot (eval R σ dv v x c)
  | .and c d => oAnd (eval R σ dv v x c) (eval R σ dv v x d)
  | .or c d => oOr (eval R σ dv v x c) (eval R σ dv v x d)

inductive Rule (F E Val : Type*)
  | allow (c : Cond F E Val)
  | deny (c : Cond F E Val)

/-- What an error in an Allow guard does: Deny (the design, §6.1) or Skip. -/
inductive ErrMode
  | deny
  | skipAllow

structure Policy (F E Val : Type*) where
  contained : Option E
  rules : List (Rule F E Val)

/-- First match wins; all Skip denies; an error in a Deny guard denies; an error in an Allow
guard denies or skips, per `mode`. -/
def evalRules (mode : ErrMode) (ev : Cond F E Val → Option Bool) : List (Rule F E Val) → Prop
  | [] => False
  | .allow c :: rs =>
    ev c = some true ∨ (ev c = some false ∧ evalRules mode ev rs) ∨
      (ev c = none ∧ mode = .skipAllow ∧ evalRules mode ev rs)
  | .deny c :: rs => ev c = some false ∧ evalRules mode ev rs

/-- `visible(node)` (§6.5) with `contained_in` (§6.7), at delegation depth `k`. -/
def vis (R : Reads St N F E Val) (D : Schema N T F E) (pol : T → Policy F E Val)
    (mode : ErrMode) : ℕ → St → N → N → Bool → Prop
  | 0, _, _, _, _ => False
  | k + 1, σ, v, x, incl =>
    R.created σ x ∧ (incl = true ∨ ¬ R.deleted σ x) ∧
    (∀ ce, (pol (D.ty x)).contained = some ce →
      incl = true ∨ ¬ ∃ t, R.edge σ x ce t ∧ R.deleted σ t) ∧
    evalRules mode (eval R σ (fun t => vis R D pol mode k σ v t false) v x) (pol (D.ty x)).rules

/-! ## The syntactic classes -/

/-- `Stab G true c`: Stable⁺ (true stays true); `Stab G false c`: Stable⁻ (false stays
false). `G` is a set of types whose decisions aren't sensitive. -/
def Stab (D : Schema N T F E) (G : Set T) : Bool → Cond F E Val → Prop
  | _, .tt => True
  | _, .ff => True
  | _, .fieldIs f _ => D.frozenF f
  | _, .viewerIn e => D.frozenE e
  | true, .can e => D.frozenE e ∧ D.tgtTy e ∈ G
  | false, .can _ => False
  | b, .not c => Stab D G (!b) c
  | b, .and c d => Stab D G b c ∧ Stab D G b d
  | b, .or c d => Stab D G b c ∧ Stab D G b d

/-- Can't error: no field reads. -/
def ErrFree : Cond F E Val → Prop
  | .fieldIs _ _ => False
  | .not c => ErrFree c
  | .and c d => ErrFree c ∧ ErrFree d
  | .or c d => ErrFree c ∧ ErrFree d
  | _ => True

/-- Unchanging: frozen inputs only, no delegation. -/
def Unch (D : Schema N T F E) : Cond F E Val → Prop
  | .tt => True
  | .ff => True
  | .fieldIs f _ => D.frozenF f
  | .viewerIn e => D.frozenE e
  | .can _ => False
  | .not c => Unch D c
  | .and c d => Unch D c ∧ Unch D d
  | .or c d => Unch D c ∧ Unch D d

/-- A rule that can't make a stale admission an over-grant. -/
def SafeRule (D : Schema N T F E) (G : Set T) (mode : ErrMode) : Rule F E Val → Prop
  | .allow c => Stab D G true c ∧ (mode = .skipAllow ∨ ErrFree c ∨ Unch D c)
  | .deny c => Stab D G false c

/-- Every type in `G` has a non-sensitive decision, given that every type in `G` does. -/
def Closed (D : Schema N T F E) (pol : T → Policy F E Val) (mode : ErrMode) (G : Set T) : Prop :=
  ∀ T₀ ∈ G, D.permT T₀ ∧
    (∀ ce, (pol T₀).contained = some ce → D.frozenE ce ∧ D.permT (D.tgtTy ce)) ∧
    ∀ r ∈ (pol T₀).rules, SafeRule D G mode r

/-- **A type's read decision isn't RevocationSensitive.** -/
def NotRS (D : Schema N T F E) (pol : T → Policy F E Val) (mode : ErrMode) (T₀ : T) : Prop :=
  ∃ G, Closed D pol mode G ∧ T₀ ∈ G

/-! ## Soundness -/

variable {R : Reads St N F E Val} {D : Schema N T F E}

theorem eval_fieldIs_eq {σ τ : St} {x : N} {f : F} {P : Val → Prop}
    (h : ∀ a, R.field σ x f a ↔ R.field τ x f a) (dσ dτ : N → Prop) (v : N) :
    eval R σ dσ v x (.fieldIs f P) = eval R τ dτ v x (.fieldIs f P) := by
  simp only [eval]
  have h1 : (∃ a, R.field σ x f a) ↔ (∃ a, R.field τ x f a) := exists_congr h
  have h2 : (∀ a, R.field σ x f a → P a) ↔ (∀ a, R.field τ x f a → P a) :=
    forall_congr' (fun a => imp_congr_left (h a))
  by_cases hx : ∃ a, R.field σ x f a
  · have hx' := h1.mp hx
    simp only [hx, hx', ↓reduceIte, Option.some.injEq, decide_eq_decide]
    exact h2
  · have hx' : ¬ ∃ a, R.field τ x f a := fun h' => hx (h1.mpr h')
    simp only [hx, hx', ↓reduceIte]

/-- **Stable⁺ and Stable⁻ are sound.** Given `σ ≤_D τ`, a created node, and a delegated
decision that is monotone on the types in `G`, a Stable⁺ condition that is true at `σ` is true
at `τ`, and a Stable⁻ condition that is false at `σ` is false at `τ`. -/
theorem stab_sound {G : Set T} {σ τ : St} (hle : DLe R D σ τ) (hty : Typed R D σ) {v x : N}
    (hx : R.created σ x) {dσ dτ : N → Prop} (hd : ∀ t, D.ty t ∈ G → dσ t → dτ t) :
    ∀ (c : Cond F E Val) (b : Bool), Stab D G b c →
      eval R σ dσ v x c = some b → eval R τ dτ v x c = some b := by
  intro c
  induction c with
  | tt => intro b _ h; exact h
  | ff => intro b _ h; exact h
  | fieldIs f P =>
    intro b hs h
    rw [← eval_fieldIs_eq (fun a => hle.2.1 x f a hs hx) dσ dτ v]; exact h
  | viewerIn e =>
    intro b hs h
    have : R.edge σ x e v ↔ R.edge τ x e v := hle.2.2.1 x e v hs hx
    simp only [eval] at h ⊢
    simpa [← this] using h
  | can e =>
    intro b hs h
    cases b with
    | false => exact absurd hs (by simp [Stab])
    | true =>
      obtain ⟨hfe, hG⟩ := hs
      simp only [eval, Option.some.injEq, decide_eq_true_eq] at h ⊢
      obtain ⟨t, ht, hdt⟩ := h
      exact ⟨t, (hle.2.2.1 x e t hfe hx).mp ht, hd t (by rw [hty x e t ht]; exact hG) hdt⟩
  | not c ih =>
    intro b hs h
    simp only [eval, oNot_eq] at h ⊢
    exact ih (!b) hs h
  | and c d ihc ihd =>
    intro b hs h
    cases b with
    | true =>
      simp only [eval, oAnd_true] at h ⊢
      exact ⟨ihc true hs.1 h.1, ihd true hs.2 h.2⟩
    | false =>
      simp only [eval, oAnd_false] at h ⊢
      rcases h with h | h
      · exact Or.inl (ihc false hs.1 h)
      · exact Or.inr (ihd false hs.2 h)
  | or c d ihc ihd =>
    intro b hs h
    cases b with
    | true =>
      simp only [eval, oOr_true] at h ⊢
      rcases h with h | h
      · exact Or.inl (ihc true hs.1 h)
      · exact Or.inr (ihd true hs.2 h)
    | false =>
      simp only [eval, oOr_false] at h ⊢
      exact ⟨ihc false hs.1 h.1, ihd false hs.2 h.2⟩

/-- **Unchanging conditions have the same value**, errors included. -/
theorem unch_sound {σ τ : St} (hle : DLe R D σ τ) {v x : N} (hx : R.created σ x)
    (dσ dτ : N → Prop) : ∀ c : Cond F E Val, Unch D c →
      eval R σ dσ v x c = eval R τ dτ v x c := by
  intro c
  induction c with
  | tt => intro _; rfl
  | ff => intro _; rfl
  | fieldIs f P => intro hs; exact eval_fieldIs_eq (fun a => hle.2.1 x f a hs hx) dσ dτ v
  | viewerIn e =>
    intro hs
    have : R.edge σ x e v ↔ R.edge τ x e v := hle.2.2.1 x e v hs hx
    simp only [eval, this]
  | can e => intro hs; exact absurd hs (by simp [Unch])
  | not c ih => intro hs; simp only [eval, ih hs]
  | and c d ihc ihd => intro hs; simp only [eval, ihc hs.1, ihd hs.2]
  | or c d ihc ihd => intro hs; simp only [eval, ihc hs.1, ihd hs.2]

/-- **Error-free conditions never error.** -/
theorem errFree_sound (σ : St) (dv : N → Prop) (v x : N) :
    ∀ c : Cond F E Val, ErrFree c → eval R σ dv v x c ≠ none := by
  intro c
  induction c with
  | tt => intro _; simp [eval]
  | ff => intro _; simp [eval]
  | fieldIs f P => intro h; exact absurd h (by simp [ErrFree])
  | viewerIn e => intro _; simp [eval]
  | can e => intro _; simp [eval]
  | not c ih =>
    intro h
    have := ih h
    simp only [eval, oNot]
    cases hc : eval R σ dv v x c <;> simp_all
  | and c d ihc ihd =>
    intro h
    have h1 := ihc h.1
    have h2 := ihd h.2
    simp only [eval]
    rcases hc : eval R σ dv v x c with _ | _ | _ <;> rcases hd : eval R σ dv v x d with _ | _ | _ <;>
      simp_all [oAnd]
  | or c d ihc ihd =>
    intro h
    have h1 := ihc h.1
    have h2 := ihd h.2
    simp only [eval]
    rcases hc : eval R σ dv v x c with _ | _ | _ <;> rcases hd : eval R σ dv v x d with _ | _ | _ <;>
      simp_all [oOr]

/-- Safe rule lists admit at `τ` whatever they admit at `σ`. -/
theorem rules_mono {G : Set T} {mode : ErrMode} {σ τ : St} (hle : DLe R D σ τ)
    (hty : Typed R D σ) {v x : N} (hx : R.created σ x) {dσ dτ : N → Prop}
    (hd : ∀ t, D.ty t ∈ G → dσ t → dτ t) :
    ∀ rs : List (Rule F E Val), (∀ r ∈ rs, SafeRule D G mode r) →
      evalRules mode (eval R σ dσ v x) rs → evalRules mode (eval R τ dτ v x) rs := by
  intro rs
  induction rs with
  | nil => intro _ h; exact h
  | cons r rs ih =>
    intro hs h
    have ih := ih (fun r' hr' => hs r' (List.mem_cons_of_mem _ hr'))
    have hr := hs r List.mem_cons_self
    cases r with
    | deny c =>
      obtain ⟨h1, h2⟩ := h
      exact ⟨stab_sound hle hty hx hd c false hr h1, ih h2⟩
    | allow c =>
      obtain ⟨hst, hmode⟩ := hr
      simp only [evalRules] at h ⊢
      rcases h with h | ⟨h1, h2⟩ | ⟨h1, hm, h2⟩
      · exact Or.inl (stab_sound hle hty hx hd c true hst h)
      · rcases hτ : eval R τ dτ v x c with _ | _ | _
        · rcases hmode with hm | hm | hm
          · exact Or.inr (Or.inr ⟨rfl, hm, ih h2⟩)
          · exact absurd hτ (errFree_sound τ dτ v x c hm)
          · rw [← unch_sound hle hx dσ dτ c hm, h1] at hτ; cases hτ
        · exact Or.inr (Or.inl ⟨rfl, ih h2⟩)
        · exact Or.inl rfl
      · rcases hτ : eval R τ dτ v x c with _ | _ | _
        · exact Or.inr (Or.inr ⟨rfl, hm, ih h2⟩)
        · exact Or.inr (Or.inl ⟨rfl, ih h2⟩)
        · exact Or.inl rfl

/-- **Polarity never over-grants.** If every type in `G` has a non-sensitive decision
(`Closed G`), a node of a type in `G` that is visible at `σ` is visible at every `τ` with
`σ ≤_D τ`, for every viewer, delegation depth, and `include_deleted`. -/
theorem vis_mono {pol : T → Policy F E Val} {mode : ErrMode} {G : Set T}
    (hG : Closed D pol mode G) {σ τ : St} (hle : DLe R D σ τ) (hty : Typed R D σ) :
    ∀ k v x incl, D.ty x ∈ G → vis R D pol mode k σ v x incl → vis R D pol mode k τ v x incl := by
  intro k
  induction k with
  | zero => intro _ _ _ _ h; exact h
  | succ k ih =>
    intro v x incl hx h
    obtain ⟨hperm, hcont, hrules⟩ := hG _ hx
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
    · exact rules_mono hle hty hc (fun t ht h => ih v t false ht h) _ hrules hr

/-- **The decision-level statement**: a type whose read decision isn't RevocationSensitive. -/
theorem notRS_mono {pol : T → Policy F E Val} {mode : ErrMode} {σ τ : St}
    (hle : DLe R D σ τ) (hty : Typed R D σ) {k : ℕ} {v x : N} {incl : Bool}
    (hrs : NotRS D pol mode (D.ty x)) (h : vis R D pol mode k σ v x incl) :
    vis R D pol mode k τ v x incl := by
  obtain ⟨G, hG, hx⟩ := hrs
  exact vis_mono hG hle hty k v x incl hx h

/-- Placeholders and absent nodes are never visible. -/
theorem vis_not_created {pol : T → Policy F E Val} {mode : ErrMode} {σ : St} {k : ℕ}
    {v x : N} {incl : Bool} (h : ¬ R.created σ x) : ¬ vis R D pol mode k σ v x incl := by
  cases k with
  | zero => exact id
  | succ k => exact fun hv => h hv.1

/-! ## Histories that pass the leader's checks give the discipline order -/

section History

variable {Seg Dot : Type*}

/-- Policies' reads of a fact state (§10.1): `@type`, `@deleted_at`, field facts
`(n, fpath f, v)`, and edge facts `(n, epath e t, sortKey)`. -/
def factReads (sch : Disc.Schema N Seg) (fpath : F → Path Seg) (epath : E → N → Path Seg) :
    Reads (ZSet (Fact N Seg Val)) N F E Val where
  created σ n := Disc.Created sch σ n
  deleted σ n := Disc.Deleted sch σ n
  field σ n f v := σ (n, fpath f, v) ≠ 0
  edge σ n e t := ∃ s, σ (n, epath e t, s) ≠ 0

/-- **Discipline-respecting histories are ordered by `≤_D`.** If every entry passes the
leader's frozen and permanent checks, and the declared field, edge, and type disciplines match
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
    (hperm : ∀ n, D.permT (D.ty n) → sch.perm n) {pol : T → Policy F E Val} {mode : ErrMode}
    {X X' : Finset Dot} (hXX : X ⊆ X')
    (hty : Typed (factReads sch fpath epath) D (stateAt delta X)) {k : ℕ} {v x : N} {incl : Bool}
    (hrs : NotRS D pol mode (D.ty x))
    (h : vis (factReads sch fpath epath) D pol mode k (stateAt delta X) v x incl) :
    vis (factReads sch fpath epath) D pol mode k (stateAt delta X') v x incl :=
  notRS_mono (history_dle hH hF hP hfF hfE hperm hXX) hty hrs h

end History

/-! ## Counterexamples

Each uses two states, `false` (stale, `σ`) and `true` (fresh, `τ`), with `σ ≤_D τ`, a decision
that meets every condition but one, admission at `σ`, and denial at `τ`. -/

/-- One node, which is also the viewer. A `blocked` edge from the node to the viewer appears
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

/-- `deny if viewer in node.blocked; allow if true`. -/
def blkPol : Unit → Policy Unit Unit Unit := fun _ =>
  { contained := none, rules := [.deny (.viewerIn ()), .allow .tt] }

theorem denyMutable_overgrants :
    DLe blkReads blkSch false true ∧ Typed blkReads blkSch false ∧
    -- the type is permanent and the Allow is Stable⁺; the Deny guard isn't Stable⁻
    blkSch.permT () ∧ Stab blkSch (∅ : Set Unit) true (Cond.tt : Cond Unit Unit Unit) ∧
    ¬ Stab blkSch (∅ : Set Unit) false (Cond.viewerIn () : Cond Unit Unit Unit) ∧
    vis blkReads blkSch blkPol .deny 1 false () () false ∧
    ¬ vis blkReads blkSch blkPol .deny 1 true () () false := by
  refine ⟨⟨fun _ h => h, fun _ _ _ _ _ => Iff.rfl, fun _ _ _ h => absurd h (by simp [blkSch]),
    fun _ _ h => h⟩, fun _ _ _ _ => rfl, trivial, trivial, by simp [Stab, blkSch], ?_, ?_⟩
  · simp [vis, blkReads, blkPol, evalRules, eval]
  · simp [vis, blkReads, blkPol, evalRules, eval]

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

def allowPol : Unit → Policy Unit Unit Unit := fun _ =>
  { contained := none, rules := [.allow .tt] }

theorem ignoreVisible_overgrants :
    DLe delReads delSch false true ∧ Typed delReads delSch false ∧
    -- every rule is safe: a rules-only classifier would call the decision non-sensitive
    (∀ r ∈ (allowPol ()).rules, SafeRule delSch (Set.univ : Set Unit) .deny r) ∧
    ¬ delSch.permT () ∧
    vis delReads delSch allowPol .deny 1 false () () false ∧
    ¬ vis delReads delSch allowPol .deny 1 true () () false := by
  refine ⟨⟨fun _ h => h, fun _ _ _ _ _ => Iff.rfl, fun _ _ _ _ _ => Iff.rfl,
    fun _ h => absurd h (by simp [delSch])⟩, fun _ _ _ h => absurd h (by simp [delReads]),
    ?_, by simp [delSch], ?_, ?_⟩
  · intro r hr
    simp only [allowPol, List.mem_singleton] at hr
    subst hr
    exact ⟨trivial, Or.inr (Or.inl trivial)⟩
  · simp [vis, delReads, allowPol, evalRules, eval]
  · simp [vis, delReads]

/-- Two nodes: `false` (the row) and `true` (its parent), typed by themselves. The frozen
`parent` edge goes from the row to the parent. The parent is a placeholder at `σ` and filled
at `τ`. The row has a frozen field `code`, unset everywhere. -/
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

/-- The row: `deny if delegate(node.parent); allow if true`. The parent: `allow if true`. -/
def ddPol : Bool → Policy Unit Unit Unit
  | false => { contained := none, rules := [.deny (.can ()), .allow .tt] }
  | true => { contained := none, rules := [.allow .tt] }

/-- The parent's type has a non-sensitive decision. -/
theorem ddPol_parent_notRS : NotRS phSch ddPol .deny true := by
  refine ⟨{true}, ?_, rfl⟩
  intro T₀ hT
  rw [Set.mem_singleton_iff] at hT
  subst hT
  refine ⟨trivial, fun ce h => by simp [ddPol] at h, ?_⟩
  intro r hr
  simp only [ddPol, List.mem_singleton] at hr
  subst hr
  exact ⟨trivial, Or.inr (Or.inl trivial)⟩

theorem denyDelegate_overgrants :
    DLe phReads phSch false true ∧ Typed phReads phSch false ∧
    -- the delegated decision isn't sensitive, the row's type is permanent, its Allow is
    -- Stable⁺, but the delegation sits inside a Deny
    NotRS phSch ddPol .deny true ∧ phSch.permT false ∧
    ¬ Stab phSch ({true} : Set Bool) false (Cond.can () : Cond Unit Unit Unit) ∧
    vis phReads phSch ddPol .deny 2 false false false false ∧
    ¬ vis phReads phSch ddPol .deny 2 true false false false := by
  refine ⟨phSch_dle.1, phSch_dle.2, ddPol_parent_notRS, trivial, by simp [Stab], ?_, ?_⟩
  · simp [vis, phReads, phSch, ddPol, evalRules, eval]
  · simp [vis, phReads, phSch, ddPol, evalRules, eval]

/-- The row: `allow if delegate(node.parent) && node.code == x; allow if true`. -/
def aePol : Bool → Policy Unit Unit Unit
  | false => { contained := none,
               rules := [.allow (.and (.can ()) (.fieldIs () (fun _ => True))), .allow .tt] }
  | true => { contained := none, rules := [.allow .tt] }

theorem allowError_overgrants :
    DLe phReads phSch false true ∧ Typed phReads phSch false ∧
    -- the design's conditions hold: the type is permanent, there is no Deny and no
    -- `contained_in`, and every Allow guard is Stable⁺ (with a non-sensitive delegate)
    phSch.permT false ∧ phSch.permT true ∧
    (∀ r ∈ (aePol false).rules, ∃ c, r = .allow c ∧ Stab phSch (Set.univ : Set Bool) true c) ∧
    -- under errors-as-Deny: admitted at σ, denied at τ
    vis phReads phSch aePol .deny 2 false false false false ∧
    ¬ vis phReads phSch aePol .deny 2 true false false false ∧
    -- with errors in Allow guards as Skip: admitted at both
    vis phReads phSch aePol .skipAllow 2 false false false false ∧
    vis phReads phSch aePol .skipAllow 2 true false false false := by
  refine ⟨phSch_dle.1, phSch_dle.2, trivial, trivial, ?_, ?_, ?_, ?_, ?_⟩
  · intro r hr
    simp only [aePol, List.mem_cons, List.not_mem_nil, or_false] at hr
    rcases hr with rfl | rfl
    · exact ⟨_, rfl, ⟨trivial, Set.mem_univ _⟩, trivial⟩
    · exact ⟨_, rfl, trivial⟩
  · simp [vis, phReads, phSch, aePol, evalRules, eval, oAnd]
  · simp [vis, phReads, phSch, aePol, evalRules, eval, oAnd]
  · simp [vis, phReads, phSch, aePol, evalRules, eval, oAnd]
  · simp [vis, phReads, phSch, aePol, evalRules, eval, oAnd]

end PolarityCEL
end Shep
