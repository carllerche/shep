/-
# Z-sets (design.md §10.1, DBSP §2)

A Z-set over `α` is a finitely supported function `α → ℤ`. We use Mathlib's
`Finsupp α ℤ` (`α →₀ ℤ`), which is an abelian group under pointwise addition.
-/
import Mathlib.Algebra.BigOperators.Finsupp.Basic
import Mathlib.Data.Finsupp.Basic
import Mathlib.Tactic.Abel
import Mathlib.Tactic.Ring
import Mathlib.Tactic.Linarith

noncomputable section
open Classical

namespace Shep

/-- A Z-set over `α`: finitely supported integer weights. -/
abbrev ZSet (α : Type*) := α →₀ ℤ

variable {α β : Type*}

/-! ## Abelian group laws (property 1)

These are inherited from Mathlib's `AddCommGroup (α →₀ ℤ)` instance; we restate them
under Shep-specific names so the correspondence with the design is explicit. -/

theorem zset_add_assoc (a b c : ZSet α) : a + b + c = a + (b + c) := add_assoc a b c
theorem zset_add_comm (a b : ZSet α) : a + b = b + a := add_comm a b
theorem zset_zero_add (a : ZSet α) : 0 + a = a := zero_add a
theorem zset_add_zero (a : ZSet α) : a + 0 = a := add_zero a
theorem zset_neg_add_cancel (a : ZSet α) : -a + a = 0 := neg_add_cancel a
theorem zset_sub_eq_add_neg (a b : ZSet α) : a - b = a + -b := sub_eq_add_neg a b

instance : AddCommGroup (ZSet α) := inferInstance

/-- The "is a set" predicate: every weight is `0` or `1`. -/
def IsSet (S : ZSet α) : Prop := ∀ x, S x = 0 ∨ S x = 1

/-- A Z-set is *positive* if all weights are non-negative. -/
def IsPositive (S : ZSet α) : Prop := ∀ x, 0 ≤ S x

theorem IsSet.isPositive {S : ZSet α} (h : IsSet S) : IsPositive S := by
  intro x; rcases h x with h | h <;> omega

theorem isSet_zero : IsSet (0 : ZSet α) := fun _ => Or.inl rfl

theorem isSet_single (a : α) : IsSet (Finsupp.single a (1 : ℤ)) := by
  intro x
  by_cases h : a = x
  · subst h; simp
  · simp [h]

/-- Sets are not closed under `+` (relevant to SQL `UNION` vs `UNION ALL`, §11.5). -/
theorem isSet_not_closed_add :
    ∃ (a b : ZSet Unit), IsSet a ∧ IsSet b ∧ ¬ IsSet (a + b) := by
  refine ⟨Finsupp.single () 1, Finsupp.single () 1, isSet_single _, isSet_single _, ?_⟩
  intro h
  rcases h () with h | h <;> simp at h

/-- Sets are not closed under `-`: subtracting a fact that is absent gives weight `-1`. -/
theorem isSet_not_closed_sub :
    ∃ (a b : ZSet Unit), IsSet a ∧ IsSet b ∧ ¬ IsSet (a - b) := by
  refine ⟨0, Finsupp.single () 1, isSet_zero, isSet_single _, ?_⟩
  intro h
  rcases h () with h | h <;> simp at h

end Shep
