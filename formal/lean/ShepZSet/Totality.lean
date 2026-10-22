/-
# Totality inference is sound (design.md §4.0 item 1, §6.1, §21)

Restatement-round obligation 4 (§18). ShepQL decisions (policy guards, `assert` conditions, `if`
conditions) must have inferred cardinality exactly One and call no partial function. We prove
that a decision the checker accepts evaluates to **exactly one boolean** in every state that
respects the schema, so decisions are two-valued.

## The model

* **Set-valued semantics.** An expression of type `t` evaluates to a list of values (EdgeQL's
  sets, as bags), or `none` for a runtime error (a partial function failing).
  - Element-wise operators (`+`, `/`, `<`, `=`, `and`, `or`, `not`) range over the Cartesian
    product of their arguments, so `{}` in any argument gives `{}`.
  - Paths (`e.l`, `e.p`) are unions over the elements of `e`.
  - `??`, `exists`, `?=`, `count`, `any`, and `shep::visible` take their arguments whole
    (`set of` / `optional`). `a in s` is element-wise in `a` only. `a if c else b` is
    element-wise in `c` and takes its branches whole (EdgeQL's `if … else`).
  - `global viewer` is `{}` or one node. `viewerIn s` is the **per-atom rule** (§4.0 item 1):
    `global viewer in s`, false with no viewer.
  - Partial functions: division (by zero) and `assert_single` (more than one element).
* **Cardinality inference** (`card`): a cardinality is a pair of flags, "at least one" and "at
  most one", so One, AtMostOne, AtLeastOne, and Many. Element-wise operators multiply, `??`
  takes "at least one" from either side, `if` multiplies all three, and the aggregates are One.
  A path's cardinality is the schema's, per link and property (`Schema`): what the checker
  assigns in the read context at hand.
* **`Total e`**: `e` calls no partial function.
* **Well-formed states** (`WF`): every link and property read returns as many values as the
  schema's cardinality allows.

## Results

* `card_sound`: **cardinality inference is sound**: in a well-formed state, an expression that
  evaluates yields a number of values that fits its inferred cardinality.
* `total_eval`: a total expression never errors.
* `decision_two_valued`: **a total expression typed One evaluates to exactly one boolean.**
* `guard_keep_if_true`, `guard_ite_one`: the doc's fixes type One (`(…) ?? false`,
  `exists`), and `if` needs its condition and both branches One.
* `viewerIn_one`, `inViewer_opt`, `viewerIn_eq_coalesce`, `viewerIn_not_differs`: under the
  per-atom rule `global viewer in s` is One; read as plain EdgeQL it is AtMostOne and needs
  `?? false`. The two readings agree on the atom and differ under `not`, so the doc must say
  which one the checker uses (**doc finding**).
* `placeholder_hop_not_total` (**doc finding**): in a policy (raw reads), a path read through
  a link to a `shep::Placeholders` type is `{}` when the target is a placeholder (§3), even if
  the link is `required`. A checker that typed that hop One would accept a guard that evaluates
  to `{}`.
-/
import ShepZSet.Basic

noncomputable section
open Classical

namespace Shep
namespace Totality

/-- Value types. -/
inductive Ty
  | bool
  | int
  | node

/-- The values of a type, with nodes `N`. -/
@[reducible] def Ty.denote (N : Type) : Ty → Type
  | .bool => Bool
  | .int => ℤ
  | .node => N

/-- A cardinality: `lo` is "at least one", `hi` is "at most one". -/
structure Card where
  lo : Bool
  hi : Bool
  deriving DecidableEq

namespace Card
def one : Card := ⟨true, true⟩
def opt : Card := ⟨false, true⟩
def many : Card := ⟨false, false⟩
/-- Element-wise combination. -/
def mul (a b : Card) : Card := ⟨a.lo && b.lo, a.hi && b.hi⟩
end Card

/-- A count fits a cardinality. -/
def Fits (c : Card) (n : ℕ) : Prop := (c.lo = true → 1 ≤ n) ∧ (c.hi = true → n ≤ 1)

/-- The checker's cardinality for each link hop and property read, in the read context at
hand. -/
structure Schema (L P : Type) where
  lcard : L → Card
  pcard : P → Card

variable {N L P : Type}

/-- ShepQL expressions, intrinsically typed. -/
inductive Expr (L P : Type) : Ty → Type
  /-- `.`, the node -/
  | self : Expr L P .node
  /-- `global viewer` -/
  | viewer : Expr L P .node
  | bool (b : Bool) : Expr L P .bool
  | int (z : ℤ) : Expr L P .int
  /-- `e.l` -/
  | hop (l : L) (e : Expr L P .node) : Expr L P .node
  /-- `e.p` -/
  | prop (p : P) (e : Expr L P .node) : Expr L P .int
  | add (a b : Expr L P .int) : Expr L P .int
  /-- partial: division by zero -/
  | div (a b : Expr L P .int) : Expr L P .int
  | lt (a b : Expr L P .int) : Expr L P .bool
  | eq {t : Ty} (a b : Expr L P t) : Expr L P .bool
  | not (a : Expr L P .bool) : Expr L P .bool
  | and (a b : Expr L P .bool) : Expr L P .bool
  | or (a b : Expr L P .bool) : Expr L P .bool
  /-- `a ?? b` -/
  | coalesce {t : Ty} (a b : Expr L P t) : Expr L P t
  /-- `exists a` -/
  | ex {t : Ty} (a : Expr L P t) : Expr L P .bool
  /-- `a ?= b` -/
  | qeq {t : Ty} (a b : Expr L P t) : Expr L P .bool
  /-- `a in s`, as in EdgeQL -/
  | inN (a s : Expr L P .node) : Expr L P .bool
  /-- `global viewer in s`, under the per-atom rule -/
  | viewerIn (s : Expr L P .node) : Expr L P .bool
  /-- `a if c else b` -/
  | ite {t : Ty} (c : Expr L P .bool) (a b : Expr L P t) : Expr L P t
  | count {t : Ty} (a : Expr L P t) : Expr L P .int
  | any (a : Expr L P .bool) : Expr L P .bool
  /-- `shep::visible(a)`: false on `{}` -/
  | visible (a : Expr L P .node) : Expr L P .bool
  /-- partial: more than one element -/
  | assertSingle {t : Ty} (a : Expr L P t) : Expr L P t

/-- A state, as one evaluation sees it. -/
structure Env (N L P : Type) where
  self : N
  viewer : Option N
  link : L → N → List N
  prop : P → N → List ℤ
  vis : N → Bool

/-- Element-wise application over the Cartesian product. -/
def prod {α β γ : Type} (f : α → β → γ) (xs : List α) (ys : List β) : List γ :=
  xs.flatMap (fun x => ys.map (f x))

/-- Evaluation: a list of values, or `none` for a runtime error. -/
def eval (ρ : Env N L P) : {t : Ty} → Expr L P t → Option (List (t.denote N))
  | _, .self => some [ρ.self]
  | _, .viewer => some ρ.viewer.toList
  | _, .bool b => some [b]
  | _, .int z => some [z]
  | _, .hop l e => (eval ρ e).map (fun xs => xs.flatMap (ρ.link l))
  | _, .prop p e => (eval ρ e).map (fun xs => xs.flatMap (ρ.prop p))
  | _, .add a b => do
    let xs ← eval ρ a; let ys ← eval ρ b
    return prod (fun (x y : ℤ) => x + y) xs ys
  | _, .div a b => do
    let xs ← eval ρ a; let ys ← eval ρ b
    if xs ≠ [] ∧ (0 : ℤ) ∈ ys then none else some (prod (fun (x y : ℤ) => x / y) xs ys)
  | _, .lt a b => do
    let xs ← eval ρ a; let ys ← eval ρ b
    return prod (fun (x y : ℤ) => decide (x < y)) xs ys
  | _, .eq a b => do
    let xs ← eval ρ a; let ys ← eval ρ b
    return prod (fun x y => decide (x = y)) xs ys
  | _, .not a => (eval ρ a).map (fun xs => xs.map (fun (x : Bool) => !x))
  | _, .and a b => do
    let xs ← eval ρ a; let ys ← eval ρ b
    return prod (fun (x y : Bool) => x && y) xs ys
  | _, .or a b => do
    let xs ← eval ρ a; let ys ← eval ρ b
    return prod (fun (x y : Bool) => x || y) xs ys
  | _, .coalesce a b => do
    let xs ← eval ρ a; let ys ← eval ρ b
    return if xs = [] then ys else xs
  | _, .ex a => (eval ρ a).map (fun xs => [!xs.isEmpty])
  | _, .qeq a b => do
    let xs ← eval ρ a; let ys ← eval ρ b
    return [decide (xs.head? = ys.head?)]
  | _, .inN a s => do
    let xs ← eval ρ a; let ys ← eval ρ s
    return xs.map (fun x => decide (x ∈ ys))
  | _, .viewerIn s => (eval ρ s).map (fun ys =>
      match ρ.viewer with
      | none => [false]
      | some v => [decide (v ∈ ys)])
  | _, .ite c a b => do
    let cs ← eval ρ c; let xs ← eval ρ a; let ys ← eval ρ b
    return cs.flatMap (fun (x : Bool) => if x then xs else ys)
  | _, .count a => (eval ρ a).map (fun xs => [(xs.length : ℤ)])
  | _, .any a => (eval ρ a).map (fun xs => [decide (true ∈ xs)])
  | _, .visible a => (eval ρ a).map (fun xs => [decide (∃ x ∈ xs, ρ.vis x = true)])
  | _, .assertSingle a => do
    let xs ← eval ρ a
    if xs.length ≤ 1 then some xs else none

/-- **Cardinality inference.** -/
def card (S : Schema L P) : {t : Ty} → Expr L P t → Card
  | _, .self => .one
  | _, .viewer => .opt
  | _, .bool _ => .one
  | _, .int _ => .one
  | _, .hop l e => (card S e).mul (S.lcard l)
  | _, .prop p e => (card S e).mul (S.pcard p)
  | _, .add a b => (card S a).mul (card S b)
  | _, .div a b => (card S a).mul (card S b)
  | _, .lt a b => (card S a).mul (card S b)
  | _, .eq a b => (card S a).mul (card S b)
  | _, .not a => card S a
  | _, .and a b => (card S a).mul (card S b)
  | _, .or a b => (card S a).mul (card S b)
  | _, .coalesce a b => ⟨(card S a).lo || (card S b).lo, (card S a).hi && (card S b).hi⟩
  | _, .ex _ => .one
  | _, .qeq _ _ => .one
  | _, .inN a _ => card S a
  | _, .viewerIn _ => .one
  | _, .ite c a b => ((card S c).mul (card S a)).mul (card S b)
  | _, .count _ => .one
  | _, .any _ => .one
  | _, .visible _ => .one
  | _, .assertSingle a => ⟨(card S a).lo, true⟩

/-- No partial functions. -/
def Total : {t : Ty} → Expr L P t → Prop
  | _, .self => True
  | _, .viewer => True
  | _, .bool _ => True
  | _, .int _ => True
  | _, .hop _ e => Total e
  | _, .prop _ e => Total e
  | _, .add a b => Total a ∧ Total b
  | _, .div _ _ => False
  | _, .lt a b => Total a ∧ Total b
  | _, .eq a b => Total a ∧ Total b
  | _, .not a => Total a
  | _, .and a b => Total a ∧ Total b
  | _, .or a b => Total a ∧ Total b
  | _, .coalesce a b => Total a ∧ Total b
  | _, .ex a => Total a
  | _, .qeq a b => Total a ∧ Total b
  | _, .inN a s => Total a ∧ Total s
  | _, .viewerIn s => Total s
  | _, .ite c a b => Total c ∧ Total a ∧ Total b
  | _, .count a => Total a
  | _, .any a => Total a
  | _, .visible a => Total a
  | _, .assertSingle _ => False

/-- A state respects the schema's cardinalities. -/
def WF (S : Schema L P) (ρ : Env N L P) : Prop :=
  (∀ l n, Fits (S.lcard l) (ρ.link l n).length) ∧ (∀ p n, Fits (S.pcard p) (ρ.prop p n).length)

/-! ## Counting lemmas -/

theorem fits_one : Fits Card.one 1 := ⟨fun _ => le_rfl, fun _ => le_rfl⟩

theorem fits_flatMap {α β : Type} {a b : Card} {xs : List α} {f : α → List β}
    (hx : Fits a xs.length) (hf : ∀ x ∈ xs, Fits b (f x).length) :
    Fits (a.mul b) (xs.flatMap f).length := by
  match xs, hx, hf with
  | [], hx, _ =>
    refine ⟨fun h => ?_, fun _ => by simp⟩
    simp only [Card.mul, Bool.and_eq_true] at h
    have := hx.1 h.1; simp at this
  | [x], _, hf =>
    have := hf x List.mem_cons_self
    simp only [List.flatMap_cons, List.flatMap_nil, List.append_nil]
    refine ⟨fun h => ?_, fun h => ?_⟩
    · simp only [Card.mul, Bool.and_eq_true] at h; exact this.1 h.2
    · simp only [Card.mul, Bool.and_eq_true] at h; exact this.2 h.2
  | x :: y :: rest, hx, hf =>
    refine ⟨fun h => ?_, fun h => ?_⟩
    · simp only [Card.mul, Bool.and_eq_true] at h
      have h1 := (hf x List.mem_cons_self).1 h.2
      simp only [List.flatMap_cons, List.length_append]
      omega
    · simp only [Card.mul, Bool.and_eq_true] at h
      have := hx.2 h.1; simp at this

theorem fits_prod {α β γ : Type} {a b : Card} {xs : List α} {ys : List β} (g : α → β → γ)
    (hx : Fits a xs.length) (hy : Fits b ys.length) : Fits (a.mul b) (prod g xs ys).length :=
  fits_flatMap hx (fun x _ => by simpa using hy)

/-! ## Soundness -/

/-- **Cardinality inference is sound.** -/
theorem card_sound (S : Schema L P) (ρ : Env N L P) (hwf : WF S ρ) :
    ∀ {t : Ty} (e : Expr L P t) (xs : List (t.denote N)), eval ρ e = some xs →
      Fits (card S e) xs.length := by
  intro t e
  induction e with
  | self => intro xs h; simp only [eval, Option.some.injEq] at h; subst h; exact fits_one
  | viewer =>
    intro xs h
    simp only [eval] at h
    have h := Option.some_inj.mp h; subst h
    have hle : ∀ o : Option N, o.toList.length ≤ 1 := fun o => by cases o <;> simp
    exact ⟨fun h => by simp [card, Card.opt] at h, fun _ => hle ρ.viewer⟩
  | bool b => intro xs h; simp only [eval, Option.some.injEq] at h; subst h; exact fits_one
  | int z => intro xs h; simp only [eval, Option.some.injEq] at h; subst h; exact fits_one
  | hop l e ih =>
    intro xs h
    simp only [eval, Option.map_eq_some_iff] at h
    obtain ⟨ys, hys, rfl⟩ := h
    exact fits_flatMap (ih ys hys) (fun x _ => hwf.1 l x)
  | prop p e ih =>
    intro xs h
    simp only [eval, Option.map_eq_some_iff] at h
    obtain ⟨ys, hys, rfl⟩ := h
    exact fits_flatMap (ih ys hys) (fun x _ => hwf.2 p x)
  | add a b iha ihb =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff, Option.pure_def,
      Option.some.injEq] at h
    obtain ⟨as, ha, bs, hb, rfl⟩ := h
    exact fits_prod _ (iha as ha) (ihb bs hb)
  | div a b iha ihb =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff] at h
    obtain ⟨as, ha, bs, hb, h⟩ := h
    split_ifs at h
    simp only [Option.some.injEq] at h; subst h
    exact fits_prod _ (iha as ha) (ihb bs hb)
  | lt a b iha ihb =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff, Option.pure_def,
      Option.some.injEq] at h
    obtain ⟨as, ha, bs, hb, rfl⟩ := h
    exact fits_prod _ (iha as ha) (ihb bs hb)
  | eq a b iha ihb =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff, Option.pure_def,
      Option.some.injEq] at h
    obtain ⟨as, ha, bs, hb, rfl⟩ := h
    exact fits_prod _ (iha as ha) (ihb bs hb)
  | not a ih =>
    intro xs h
    simp only [eval, Option.map_eq_some_iff] at h
    obtain ⟨ys, hys, rfl⟩ := h
    show Fits (card S a) _
    rw [List.length_map]; exact ih ys hys
  | and a b iha ihb =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff, Option.pure_def,
      Option.some.injEq] at h
    obtain ⟨as, ha, bs, hb, rfl⟩ := h
    exact fits_prod _ (iha as ha) (ihb bs hb)
  | or a b iha ihb =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff, Option.pure_def,
      Option.some.injEq] at h
    obtain ⟨as, ha, bs, hb, rfl⟩ := h
    exact fits_prod _ (iha as ha) (ihb bs hb)
  | coalesce a b iha ihb =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff, Option.pure_def,
      Option.some.injEq] at h
    obtain ⟨as, ha, bs, hb, rfl⟩ := h
    have fa := iha as ha
    have fb := ihb bs hb
    unfold Fits; dsimp only [card]
    split_ifs with he
    · subst he
      refine ⟨fun hl => ?_, fun hh => ?_⟩
      · simp only [Bool.or_eq_true] at hl
        rcases hl with hl | hl
        · have := fa.1 hl; simp at this
        · exact fb.1 hl
      · simp only [Bool.and_eq_true] at hh; exact fb.2 hh.2
    · refine ⟨fun _ => ?_, fun hh => ?_⟩
      · exact List.length_pos_iff.mpr he
      · simp only [Bool.and_eq_true] at hh; exact fa.2 hh.1
  | ex a _ =>
    intro xs h
    simp only [eval, Option.map_eq_some_iff] at h
    obtain ⟨_, _, rfl⟩ := h
    exact fits_one
  | qeq a b _ _ =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff, Option.pure_def,
      Option.some.injEq] at h
    obtain ⟨_, _, _, _, rfl⟩ := h
    exact fits_one
  | inN a s iha _ =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff, Option.pure_def,
      Option.some.injEq] at h
    obtain ⟨as, ha, _, _, rfl⟩ := h
    show Fits (card S a) _
    rw [List.length_map]; exact iha as ha
  | viewerIn s _ =>
    intro xs h
    simp only [eval, Option.map_eq_some_iff] at h
    obtain ⟨_, _, rfl⟩ := h
    cases ρ.viewer <;> exact fits_one
  | ite c a b ihc iha ihb =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff, Option.pure_def,
      Option.some.injEq] at h
    obtain ⟨cs, hc, as, ha, bs, hb, rfl⟩ := h
    have fa := iha as ha
    have fb := ihb bs hb
    have key : ∀ x : Bool, Fits ((card S a).mul (card S b))
        (if x then as else bs).length := by
      intro x
      cases x
      · exact ⟨fun h => by simp only [Card.mul, Bool.and_eq_true] at h; exact fb.1 h.2,
          fun h => by simp only [Card.mul, Bool.and_eq_true] at h; exact fb.2 h.2⟩
      · exact ⟨fun h => by simp only [Card.mul, Bool.and_eq_true] at h; exact fa.1 h.1,
          fun h => by simp only [Card.mul, Bool.and_eq_true] at h; exact fa.2 h.1⟩
    have := fits_flatMap (ihc cs hc) (fun x _ => key x)
    have e : ((card S c).mul (card S a)).mul (card S b) =
        (card S c).mul ((card S a).mul (card S b)) := by
      simp only [Card.mul, Bool.and_assoc]
    show Fits (((card S c).mul (card S a)).mul (card S b)) _
    rw [e]; exact this
  | count a _ =>
    intro xs h
    simp only [eval, Option.map_eq_some_iff] at h
    obtain ⟨_, _, rfl⟩ := h
    exact fits_one
  | any a _ =>
    intro xs h
    simp only [eval, Option.map_eq_some_iff] at h
    obtain ⟨_, _, rfl⟩ := h
    exact fits_one
  | visible a _ =>
    intro xs h
    simp only [eval, Option.map_eq_some_iff] at h
    obtain ⟨_, _, rfl⟩ := h
    exact fits_one
  | assertSingle a ih =>
    intro xs h
    simp only [eval, Option.bind_eq_bind, Option.bind_eq_some_iff] at h
    obtain ⟨ys, hys, h⟩ := h
    split_ifs at h with hl
    simp only [Option.some.injEq] at h; subst h
    exact ⟨(ih _ hys).1, fun _ => hl⟩

/-- **A total expression never errors.** -/
theorem total_eval (ρ : Env N L P) :
    ∀ {t : Ty} (e : Expr L P t), Total e → ∃ xs, eval ρ e = some xs := by
  intro t e
  induction e with
  | self => intro _; exact ⟨_, rfl⟩
  | viewer => intro _; exact ⟨_, rfl⟩
  | bool b => intro _; exact ⟨_, rfl⟩
  | int z => intro _; exact ⟨_, rfl⟩
  | hop l e ih => intro h; obtain ⟨xs, hx⟩ := ih h; exact ⟨_, by simp only [eval, hx]; rfl⟩
  | prop p e ih => intro h; obtain ⟨xs, hx⟩ := ih h; exact ⟨_, by simp only [eval, hx]; rfl⟩
  | add a b iha ihb =>
    intro h; obtain ⟨xs, hx⟩ := iha h.1; obtain ⟨ys, hy⟩ := ihb h.2
    exact ⟨_, by simp only [eval, hx, hy]; rfl⟩
  | div a b _ _ => intro h; exact h.elim
  | lt a b iha ihb =>
    intro h; obtain ⟨xs, hx⟩ := iha h.1; obtain ⟨ys, hy⟩ := ihb h.2
    exact ⟨_, by simp only [eval, hx, hy]; rfl⟩
  | eq a b iha ihb =>
    intro h; obtain ⟨xs, hx⟩ := iha h.1; obtain ⟨ys, hy⟩ := ihb h.2
    exact ⟨_, by simp only [eval, hx, hy]; rfl⟩
  | not a ih => intro h; obtain ⟨xs, hx⟩ := ih h; exact ⟨_, by simp only [eval, hx]; rfl⟩
  | and a b iha ihb =>
    intro h; obtain ⟨xs, hx⟩ := iha h.1; obtain ⟨ys, hy⟩ := ihb h.2
    exact ⟨_, by simp only [eval, hx, hy]; rfl⟩
  | or a b iha ihb =>
    intro h; obtain ⟨xs, hx⟩ := iha h.1; obtain ⟨ys, hy⟩ := ihb h.2
    exact ⟨_, by simp only [eval, hx, hy]; rfl⟩
  | coalesce a b iha ihb =>
    intro h; obtain ⟨xs, hx⟩ := iha h.1; obtain ⟨ys, hy⟩ := ihb h.2
    exact ⟨_, by simp only [eval, hx, hy]; rfl⟩
  | ex a ih => intro h; obtain ⟨xs, hx⟩ := ih h; exact ⟨_, by simp only [eval, hx]; rfl⟩
  | qeq a b iha ihb =>
    intro h; obtain ⟨xs, hx⟩ := iha h.1; obtain ⟨ys, hy⟩ := ihb h.2
    exact ⟨_, by simp only [eval, hx, hy]; rfl⟩
  | inN a s iha ihs =>
    intro h; obtain ⟨xs, hx⟩ := iha h.1; obtain ⟨ys, hy⟩ := ihs h.2
    exact ⟨_, by simp only [eval, hx, hy]; rfl⟩
  | viewerIn s ih => intro h; obtain ⟨xs, hx⟩ := ih h; exact ⟨_, by simp only [eval, hx]; rfl⟩
  | ite c a b ihc iha ihb =>
    intro h
    obtain ⟨cs, hc⟩ := ihc h.1; obtain ⟨xs, hx⟩ := iha h.2.1; obtain ⟨ys, hy⟩ := ihb h.2.2
    exact ⟨_, by simp only [eval, hc, hx, hy]; rfl⟩
  | count a ih => intro h; obtain ⟨xs, hx⟩ := ih h; exact ⟨_, by simp only [eval, hx]; rfl⟩
  | any a ih => intro h; obtain ⟨xs, hx⟩ := ih h; exact ⟨_, by simp only [eval, hx]; rfl⟩
  | visible a ih => intro h; obtain ⟨xs, hx⟩ := ih h; exact ⟨_, by simp only [eval, hx]; rfl⟩
  | assertSingle a _ => intro h; exact h.elim

/-- **Decisions are two-valued.** A total guard typed One evaluates, in every state that
respects the schema, to exactly one boolean. -/
theorem decision_two_valued (S : Schema L P) (ρ : Env N L P) (hwf : WF S ρ)
    (g : Expr L P .bool) (htot : Total g) (hone : card S g = Card.one) :
    ∃ b : Bool, eval ρ g = some [b] := by
  obtain ⟨xs, hx⟩ := total_eval ρ g htot
  have hf := card_sound S ρ hwf g xs hx
  rw [hone] at hf
  have h1 := hf.1 rfl
  have h2 := hf.2 rfl
  match xs, h1, h2 with
  | [b], _, _ => exact ⟨b, hx⟩

/-! ## The checker's fixes -/

/-- `(…) ?? false` makes any AtMostOne guard One (the fix for an allow, an `if`, or an
`assert`); `?? true` does the same for a deny. -/
theorem guard_keep_if_true (S : Schema L P) (g : Expr L P .bool) (d : Bool)
    (h : (card S g).hi = true) : card S (.coalesce g (.bool d)) = Card.one := by
  simp [card, Card.one, h]

/-- `exists` is always One. -/
theorem guard_exists_one (S : Schema L P) {t : Ty} (e : Expr L P t) :
    card S (.ex e) = Card.one := rfl

/-- An `if` is One exactly when its condition and both branches are. -/
theorem guard_ite_one (S : Schema L P) {t : Ty} (c : Expr L P .bool) (a b : Expr L P t) :
    card S (.ite c a b) = Card.one ↔
      card S c = Card.one ∧ card S a = Card.one ∧ card S b = Card.one := by
  show ((card S c).mul (card S a)).mul (card S b) = Card.one ↔ _
  generalize card S c = C
  generalize card S a = A
  generalize card S b = B
  obtain ⟨c1, c2⟩ := C
  obtain ⟨a1, a2⟩ := A
  obtain ⟨b1, b2⟩ := B
  cases c1 <;> cases c2 <;> cases a1 <;> cases a2 <;> cases b1 <;> cases b2 <;> decide

/-! ## Principal atoms: the per-atom rule versus plain EdgeQL -/

/-- Under the per-atom rule, `global viewer in s` is One. -/
theorem viewerIn_one (S : Schema L P) (s : Expr L P .node) :
    card S (.viewerIn s) = Card.one := rfl

/-- Read as plain EdgeQL, `global viewer in s` is AtMostOne (`global viewer` is AtMostOne in
policies, §4.0 item 11), so a guard needs `?? false`. -/
theorem inViewer_opt (S : Schema L P) (s : Expr L P .node) :
    card S (.inN .viewer s) = Card.opt := rfl

/-- The two readings agree on the atom: the per-atom atom is the EdgeQL atom with `?? false`. -/
theorem viewerIn_eq_coalesce (ρ : Env N L P) (s : Expr L P .node) :
    eval ρ (.viewerIn s) = eval ρ (.coalesce (.inN .viewer s) (.bool false)) := by
  simp only [eval]
  cases eval ρ s with
  | none => simp
  | some ys => cases ρ.viewer <;> simp

/-- They differ under `not`: with no viewer, the per-atom `not (global viewer in s)` is true,
and EdgeQL's `(not (global viewer in s)) ?? false` is false. -/
theorem viewerIn_not_differs (ρ : Env N L P) (s : Expr L P .node) (hv : ρ.viewer = none)
    (ys : List N) (hs : eval ρ s = some ys) :
    eval ρ (.not (.viewerIn s)) = some [true] ∧
      eval ρ (.coalesce (.not (.inN .viewer s)) (.bool false)) = some [false] := by
  simp [eval, hs, hv]

/-! ## Counterexample: a required hop to a placeholder-enabled type, in a policy

One node `()`, which is the policy's node and has a required single link `parent` to a
`shep::Placeholders` type. The target is a placeholder, and a raw read of a placeholder is `{}`
(§3: "every path read treats a placeholder as `{}`"), so the hop gives no node. A checker that
typed the hop One, as `required` suggests, would type `.parent.n < 5` One, and it evaluates to
`{}`. The hop has to be AtMostOne in policies; then the guard needs `?? false`. -/

/-- The checker's assumed typing: the hop is One. -/
def phS : Schema Unit Unit := ⟨fun _ => Card.one, fun _ => Card.one⟩

/-- The raw store: the link's target is a placeholder, so the hop reads `{}`. -/
def phEnv : Env Unit Unit Unit := ⟨(), none, fun _ _ => [], fun _ _ => [0], fun _ => false⟩

/-- `.parent.n < 5`. -/
def phGuard : Expr Unit Unit .bool := .lt (.prop () (.hop () .self)) (.int 5)

theorem placeholder_hop_not_total :
    Total phGuard ∧ card phS phGuard = Card.one ∧ eval phEnv phGuard = some [] ∧
      ¬ WF phS phEnv ∧
      -- typed AtMostOne, with `?? false`, the guard is One and well-formed
      card ⟨fun _ => Card.opt, fun _ => Card.one⟩ (.coalesce phGuard (.bool false)) = Card.one ∧
      WF ⟨fun _ => Card.opt, fun _ => Card.one⟩ phEnv ∧
      eval phEnv (.coalesce phGuard (.bool false)) = some [false] := by
  refine ⟨⟨trivial, trivial⟩, rfl, ?_, ?_, rfl, ?_, ?_⟩
  · simp [phGuard, eval, phEnv, prod]
  · intro h
    have := (h.1 () ()).1 rfl
    simp [phEnv] at this
  · refine ⟨fun _ _ => ⟨fun h => by simp [Card.opt] at h, fun _ => by simp [phEnv]⟩,
      fun _ _ => ⟨fun _ => by simp [phEnv], fun _ => by simp [phEnv]⟩⟩
  · simp [phGuard, eval, phEnv, prod]

end Totality
end Shep
