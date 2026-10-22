/-
# Checks and the SMT option-lifting (design.md §4.5)

Restatement-round obligation 6 (§18). A check holds when its expression is `{}` or `true`
("empty means satisfied", as SQL `CHECK`), which the design writes `¬defined ∨ value`. The SMT
backend encodes each optional term of the subset as a pair `(defined, value)`. We prove the
encoding agrees with ShepQL's set semantics on the subset, and so decides checks and guards
exactly.

## The model

* A **row** gives each single scalar property an optional value (`P → Option ℤ`).
* **ShepQL semantics** (`sem`): each term is a set, here a list. A property read is `{}` or one
  value; element-wise operators (`+`, `<`, `=`, `not`, `and`, `or`, `in` on its left) range over
  the product of their arguments, so `{}` anywhere gives `{}`; `??`, `?=`, and `exists` take
  their arguments whole; `a if c else b` is element-wise in `c` and takes its branches whole.
* **The option-lifting** (`enc`): §4.5's rules. An element-wise operator is defined when all its
  inputs are; `??`, `?=`, and `exists` are total; the value of an undefined term is a default.
  `if … else` is defined when its condition is and **the chosen branch** is.
* **Checks and guards.** A check holds when no element of its set is `false` (`checkHolds`); a
  guard or filter holds when some element is `true` (`guardHolds`).

## Results

* `sem_atMostOne`: every term of the subset is `{}` or one value.
* `enc_agrees`: **the option-lifting agrees with the set semantics** on every term:
  `sem e = if defined then [value] else []`.
* `check_iff`: **a check holds iff `¬defined ∨ value`**, and `guard_iff`: a guard holds iff
  `defined ∧ value`.
* `check_sem_iff`: the same, stated over the set alone: a check's set is `{}` or `{true}`.
* `ite_naive_disagrees` (**doc finding**): §4.5's general rule, "an element-wise operator is
  defined when all its inputs are", is wrong for `if … else` if read as covering it: EdgeQL's
  `if … else` takes its branches whole, so `(.a if true else .b) > 0` with `.a = -1` and `.b`
  unset is `false` (the check fails), while the naive lifting calls it undefined (the check
  passes).
-/
import ShepZSet.Basic

noncomputable section
open Classical

namespace Shep
namespace Checks

/-- Scalar types of the subset. -/
inductive STy
  | bool
  | int

@[reducible] def STy.denote : STy → Type
  | .bool => Bool
  | .int => ℤ

/-- The default value of an undefined term. -/
def STy.dflt : (t : STy) → t.denote
  | .bool => false
  | .int => 0

variable {P : Type}

/-- The encodable subset, over single scalar properties `P`. -/
inductive SExpr (P : Type) : STy → Type
  | prop (p : P) : SExpr P .int
  | lit (z : ℤ) : SExpr P .int
  | blit (b : Bool) : SExpr P .bool
  | add (a b : SExpr P .int) : SExpr P .int
  | lt (a b : SExpr P .int) : SExpr P .bool
  | eq (a b : SExpr P .int) : SExpr P .bool
  | not (a : SExpr P .bool) : SExpr P .bool
  | and (a b : SExpr P .bool) : SExpr P .bool
  | or (a b : SExpr P .bool) : SExpr P .bool
  /-- `a ?? b` -/
  | coalesce {t : STy} (a b : SExpr P t) : SExpr P t
  /-- `a ?= b` -/
  | qeq (a b : SExpr P .int) : SExpr P .bool
  /-- `exists a` -/
  | ex {t : STy} (a : SExpr P t) : SExpr P .bool
  /-- `a if c else b` -/
  | ite {t : STy} (c : SExpr P .bool) (a b : SExpr P t) : SExpr P t
  /-- `a in {z₁, …}`, a set of literals -/
  | inLits (a : SExpr P .int) (zs : List ℤ) : SExpr P .bool

/-- Element-wise application over the product. -/
def prod {α β γ : Type} (f : α → β → γ) (xs : List α) (ys : List β) : List γ :=
  xs.flatMap (fun x => ys.map (f x))

/-- **ShepQL's set semantics.** -/
def sem (row : P → Option ℤ) : {t : STy} → SExpr P t → List t.denote
  | _, .prop p => (row p).toList
  | _, .lit z => [z]
  | _, .blit b => [b]
  | _, .add a b => prod (fun (x y : ℤ) => x + y) (sem row a) (sem row b)
  | _, .lt a b => prod (fun (x y : ℤ) => decide (x < y)) (sem row a) (sem row b)
  | _, .eq a b => prod (fun (x y : ℤ) => decide (x = y)) (sem row a) (sem row b)
  | _, .not a => (sem row a).map (fun (x : Bool) => !x)
  | _, .and a b => prod (fun (x y : Bool) => x && y) (sem row a) (sem row b)
  | _, .or a b => prod (fun (x y : Bool) => x || y) (sem row a) (sem row b)
  | _, .coalesce a b => if sem row a = [] then sem row b else sem row a
  | _, .qeq a b => [decide ((sem row a).head? = (sem row b).head?)]
  | _, .ex a => [!(sem row a).isEmpty]
  | _, .ite c a b => (sem row c).flatMap (fun (x : Bool) => if x then sem row a else sem row b)
  | _, .inLits a zs => (sem row a).map (fun x => decide (x ∈ zs))

/-- **The SMT option-lifting**: `(defined, value)`. -/
def enc (row : P → Option ℤ) : {t : STy} → SExpr P t → Bool × t.denote
  | _, .prop p => ((row p).isSome, (row p).getD 0)
  | _, .lit z => (true, z)
  | _, .blit b => (true, b)
  | _, .add a b => ((enc row a).1 && (enc row b).1, (enc row a).2 + (enc row b).2)
  | _, .lt a b => ((enc row a).1 && (enc row b).1, decide ((enc row a).2 < (enc row b).2))
  | _, .eq a b => ((enc row a).1 && (enc row b).1, decide ((enc row a).2 = (enc row b).2))
  | _, .not a => ((enc row a).1, !(enc row a).2)
  | _, .and a b => ((enc row a).1 && (enc row b).1, (enc row a).2 && (enc row b).2)
  | _, .or a b => ((enc row a).1 && (enc row b).1, (enc row a).2 || (enc row b).2)
  | _, .coalesce a b =>
    ((enc row a).1 || (enc row b).1, if (enc row a).1 then (enc row a).2 else (enc row b).2)
  | _, .qeq a b =>
    (true, ((enc row a).1 && (enc row b).1 && decide ((enc row a).2 = (enc row b).2)) ||
      (!(enc row a).1 && !(enc row b).1))
  | _, .ex a => (true, (enc row a).1)
  | _, .ite c a b =>
    ((enc row c).1 && (if (enc row c).2 then (enc row a).1 else (enc row b).1),
      if (enc row c).2 then (enc row a).2 else (enc row b).2)
  | _, .inLits a zs => ((enc row a).1, decide ((enc row a).2 ∈ zs))

/-- The pair as a set. -/
def asSet {α : Type} (p : Bool × α) : List α := if p.1 then [p.2] else []

@[simp] theorem asSet_true {α : Type} (a : α) : asSet (true, a) = [a] := rfl
@[simp] theorem asSet_false {α : Type} (a : α) : asSet (false, a) = [] := rfl

theorem prod_asSet {α β γ : Type} (f : α → β → γ) (p : Bool × α) (q : Bool × β) :
    prod f (asSet p) (asSet q) = asSet (p.1 && q.1, f p.2 q.2) := by
  obtain ⟨d, x⟩ := p
  obtain ⟨e, y⟩ := q
  cases d <;> cases e <;> rfl

/-- **The option-lifting agrees with ShepQL's set semantics** on every term of the subset. -/
theorem enc_agrees (row : P → Option ℤ) :
    ∀ {t : STy} (e : SExpr P t), sem row e = asSet (enc row e) := by
  intro t e
  induction e with
  | prop p => simp only [sem, enc]; cases row p <;> rfl
  | lit z => rfl
  | blit b => rfl
  | add a b iha ihb => simp only [sem, enc, iha, ihb, prod_asSet]
  | lt a b iha ihb => simp only [sem, enc, iha, ihb, prod_asSet]
  | eq a b iha ihb => simp only [sem, enc, iha, ihb, prod_asSet]
  | not a ih =>
    simp only [sem, enc, ih]
    obtain ⟨d, x⟩ := enc row a
    cases d <;> rfl
  | and a b iha ihb => simp only [sem, enc, iha, ihb, prod_asSet]
  | or a b iha ihb => simp only [sem, enc, iha, ihb, prod_asSet]
  | coalesce a b iha ihb =>
    simp only [sem, enc, iha, ihb]
    obtain ⟨d, x⟩ := enc row a
    obtain ⟨e, y⟩ := enc row b
    cases d <;> cases e <;> rfl
  | qeq a b iha ihb =>
    simp only [sem, enc, iha, ihb]
    obtain ⟨d, x⟩ := enc row a
    obtain ⟨e, y⟩ := enc row b
    cases d <;> cases e <;> simp [asSet]
  | ex a ih =>
    simp only [sem, enc, ih]
    obtain ⟨d, x⟩ := enc row a
    cases d <;> rfl
  | ite c a b ihc iha ihb =>
    simp only [sem, enc, ihc, iha, ihb]
    obtain ⟨dc, vc⟩ := enc row c
    obtain ⟨d, x⟩ := enc row a
    obtain ⟨e, y⟩ := enc row b
    cases dc <;> cases vc <;> cases d <;> cases e <;> rfl
  | inLits a zs ih =>
    simp only [sem, enc, ih]
    obtain ⟨d, x⟩ := enc row a
    cases d <;> rfl

/-- Every term of the subset is `{}` or one value. -/
theorem sem_atMostOne (row : P → Option ℤ) {t : STy} (e : SExpr P t) :
    (sem row e).length ≤ 1 := by
  rw [enc_agrees]; unfold asSet; split_ifs <;> simp

/-- A check holds when no element of its set is `false` ("empty means satisfied"). -/
def checkHolds (xs : List Bool) : Prop := false ∉ xs

/-- A guard or filter holds when some element is `true`. -/
def guardHolds (xs : List Bool) : Prop := true ∈ xs

/-- **A check holds iff `¬defined ∨ value`.** -/
theorem check_iff (row : P → Option ℤ) (e : SExpr P .bool) :
    checkHolds (sem row e) ↔ ((enc row e).1 = false ∨ (enc row e).2 = true) := by
  rw [enc_agrees]
  obtain ⟨d, v⟩ := enc row e
  cases d <;> cases v <;> simp [checkHolds, asSet]

/-- **A guard holds iff `defined ∧ value`.** -/
theorem guard_iff (row : P → Option ℤ) (e : SExpr P .bool) :
    guardHolds (sem row e) ↔ ((enc row e).1 = true ∧ (enc row e).2 = true) := by
  rw [enc_agrees]
  obtain ⟨d, v⟩ := enc row e
  cases d <;> cases v <;> simp [guardHolds, asSet]

/-- Over the set alone: a check's set is `{}` or `{true}`. -/
theorem check_sem_iff (row : P → Option ℤ) (e : SExpr P .bool) :
    checkHolds (sem row e) ↔ (sem row e = [] ∨ sem row e = [true]) := by
  rw [enc_agrees]
  obtain ⟨d, v⟩ := enc row e
  cases d <;> cases v <;> simp [checkHolds, asSet]

/-! ## Counterexample: lifting `if … else` as if it were element-wise in its branches -/

/-- The naive lifting: `if` defined only when all three inputs are. -/
def encNaiveIte (row : P → Option ℤ) {t : STy} (c : SExpr P .bool) (a b : SExpr P t) :
    Bool × t.denote :=
  ((enc row c).1 && (enc row a).1 && (enc row b).1,
    if (enc row c).2 then (enc row a).2 else (enc row b).2)

/-- Properties `a` (`= -1`) and `b` (unset). -/
def iteRow : Bool → Option ℤ := fun p => if p then some (-1) else none

/-- `(.a if true else .b) > 0`, as `0 < (.a if true else .b)`. -/
def iteCheck : SExpr Bool .bool := .lt (.lit 0) (.ite (.blit true) (.prop true) (.prop false))

theorem ite_naive_disagrees :
    -- ShepQL: the check's expression is `{false}`, so the check fails
    sem iteRow iteCheck = [false] ∧ ¬ checkHolds (sem iteRow iteCheck) ∧
    -- the naive lifting calls the `if` undefined, and the check vacuous
    (encNaiveIte iteRow (.blit true) (.prop true) (.prop false)).1 = false ∧
    -- the lifting above (defined when the chosen branch is) agrees with ShepQL
    (enc iteRow iteCheck).1 = true ∧ (enc iteRow iteCheck).2 = false := by
  refine ⟨?_, ?_, ?_, ?_, ?_⟩ <;> simp [sem, enc, encNaiveIte, iteCheck, iteRow, prod, checkHolds]

end Checks
end Shep
