/-
# Change queries (design.md §11.1, §11.6)

`changes Q S₁ S₂ = Q S₂ - Q S₁`, for an arbitrary (not necessarily linear) query `Q`.
-/
import ShepZSet.Integrate

noncomputable section
open Classical

namespace Shep

variable {α β Dot : Type*}

/-- §11.1: `changes(Q, V1, V2) = Q(V2) − Q(V1)`, on the states at the two versions. -/
def changes (Q : ZSet α → ZSet β) (S₁ S₂ : ZSet α) : ZSet β := Q S₂ - Q S₁

/-- Applying a change delta to the old result gives the new result (exactness, §11.6). -/
theorem changes_apply (Q : ZSet α → ZSet β) (S₁ S₂ : ZSet α) :
    Q S₁ + changes Q S₁ S₂ = Q S₂ := by
  simp [changes]

/-- **Chaining** (§11.6): deltas compose. -/
theorem changes_chain (Q : ZSet α → ZSet β) (S₀ S₁ S₂ : ZSet α) :
    changes Q S₀ S₁ + changes Q S₁ S₂ = changes Q S₀ S₂ := by
  simp only [changes]; abel

theorem changes_self (Q : ZSet α → ZSet β) (S : ZSet α) : changes Q S S = 0 := by
  simp [changes]

/-- Reversal: going back is the negation. -/
theorem changes_symm (Q : ZSet α → ZSet β) (S₁ S₂ : ZSet α) :
    changes Q S₂ S₁ = -changes Q S₁ S₂ := by
  simp [changes]

/-- One step of a holder (an app or a view): it keeps `(cache, lastState)` and, given the next
state, adds the change delta. -/
def holderStep (Q : ZSet α → ZSet β) (st : ZSet β × ZSet α) (S : ZSet α) :
    ZSet β × ZSet α :=
  (st.1 + changes Q st.2 S, S)

/-- **The holder invariant** (§11.6 "an app that applies … holds exactly Q(V2)", §13): starting
from `Q S₀` and applying change deltas along any sequence of versions keeps `cache = Q S`
for the latest state `S`. -/
theorem holder_invariant (Q : ZSet α → ZSet β) (S₀ : ZSet α) (Ss : List (ZSet α)) :
    let st := Ss.foldl (holderStep Q) (Q S₀, S₀)
    st.1 = Q st.2 := by
  intro st
  have key : ∀ (Ss : List (ZSet α)) (c : ZSet β) (S : ZSet α), c = Q S →
      (Ss.foldl (holderStep Q) (c, S)).1 = Q (Ss.foldl (holderStep Q) (c, S)).2 := by
    intro Ss
    induction Ss with
    | nil => intro c S h; simpa using h
    | cons S' Ss ih =>
      intro c S h
      simp only [List.foldl_cons]
      apply ih
      simp [changes, h]
  exact key Ss _ _ rfl

/-- The same invariant in terms of versions: a change delta computed between the states at
versions `V₁` and `V₂` is applied to `Q (stateAt V₁)`. Nothing about causality or about
`V₁ ⊆ V₂` is needed for the algebra; `V₁ ⊆ V₂` is needed only for the §11.5 computation
`state(V₁) = state(V₂) − ΔI` with `ΔI` the deltas of `V₂ \ V₁`. -/
theorem state_sub_of_subset (delta : Dot → ZSet α) {V₁ V₂ : Finset Dot} (h : V₁ ⊆ V₂) :
    stateAt delta V₁ = stateAt delta V₂ - stateAt delta (V₂ \ V₁) := by
  unfold stateAt
  rw [← Finset.sum_sdiff h]
  abel

/-- Without `V₁ ⊆ V₂`, the input delta also has a negative part: `V₁ \ V₂`. -/
theorem state_diff_general (delta : Dot → ZSet α) (V₁ V₂ : Finset Dot) :
    stateAt delta V₂ - stateAt delta V₁ =
      stateAt delta (V₂ \ V₁) - stateAt delta (V₁ \ V₂) := by
  unfold stateAt
  rw [← Finset.sum_sdiff (Finset.inter_subset_left : V₂ ∩ V₁ ⊆ V₂),
      ← Finset.sum_sdiff (Finset.inter_subset_left : V₁ ∩ V₂ ⊆ V₁),
      Finset.inter_comm V₁ V₂]
  have h1 : V₂ \ (V₂ ∩ V₁) = V₂ \ V₁ := by ext x; simp only [Finset.mem_sdiff, Finset.mem_inter]; tauto
  have h2 : V₁ \ (V₂ ∩ V₁) = V₁ \ V₂ := by ext x; simp only [Finset.mem_sdiff, Finset.mem_inter]; tauto
  rw [h1, h2]
  abel

end Shep
