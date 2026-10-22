/-
# Search: hidden fields neither match nor rank (design.md §16.6)

A row's relevance score is a sum of per-field scores. When a field-level policy hides a field
from an actor, the indexer scores the row using only the fields visible to that actor in that
row, and ranking, the page cut, and the cursor all use that score.

## The model

* `c : R → F → T` is the content of every row's fields.
* `fvis c a r f`: field `f` of row `r` is visible to actor `a`. It may depend on the content
  (a field policy can read fields), but see `FieldPolicyStable`.
* A `Scorer` gives each field a score and a match predicate that depend **only on that field's
  content and the query** (and fixed per-field weights).

**Corpus statistics are outside this model.** BM25's IDF and average field length are computed
over the whole index, including content hidden from the actor, so in BM25 a field's score also
depends on other rows' hidden fields. The design accepts that (open question 12). The results
below hold for the per-row part of the score with corpus statistics held fixed;
`idf_outside_model` shows that with live corpus statistics, changing hidden content can reorder
rows that match only in visible fields.

## Results

* `score_invariant`, `matches_invariant`: changing the content of fields hidden from the actor
  changes neither any row's score nor whether it matches, for that actor.
* `ranking_invariant`, `before_invariant`: so any ranking, cut, or cursor computed from the
  match set and the scores (in particular "higher score first, ties by key") is invariant.
* Hypothesis `FieldPolicyStable`: whether a field is visible doesn't depend on the content of
  fields hidden from the actor. It holds for field policies that read no field content
  (`fieldPolicyStable_of_const`). `fieldPolicy_reading_hidden_leaks` shows it is needed.
-/
import ShepZSet.Basic
import Mathlib.Data.Fintype.Prod

noncomputable section
open Classical

namespace Shep
namespace Search

variable {R F T Qy A S : Type*}

/-- Per-field scoring: depends only on the field, its content, and the query. -/
structure Scorer (F T Qy S : Type*) where
  fieldScore : F → T → Qy → S
  fieldMatch : F → T → Qy → Prop

variable [Fintype F] [AddCommMonoid S]

/-- A row's score for an actor: the sum over the fields visible to that actor in that row. -/
def score (sc : Scorer F T Qy S) (fvis : (R → F → T) → A → R → F → Prop) (q : Qy) (a : A)
    (c : R → F → T) (r : R) : S :=
  ∑ f ∈ Finset.univ.filter (fvis c a r), sc.fieldScore f (c r f) q

/-- A row matches for an actor if the query matches one of its visible fields. -/
def Matches (sc : Scorer F T Qy S) (fvis : (R → F → T) → A → R → F → Prop) (q : Qy) (a : A)
    (c : R → F → T) (r : R) : Prop :=
  ∃ f, fvis c a r f ∧ sc.fieldMatch f (c r f) q

/-- `c` and `c'` agree on every field visible to `a` (under `c`): they differ only in the
content of fields hidden from `a`. -/
def AgreeVisible (fvis : (R → F → T) → A → R → F → Prop) (a : A) (c c' : R → F → T) : Prop :=
  ∀ r f, fvis c a r f → c r f = c' r f

/-- Field visibility doesn't depend on the content of fields hidden from the actor. -/
def FieldPolicyStable (fvis : (R → F → T) → A → R → F → Prop) : Prop :=
  ∀ a c c', AgreeVisible fvis a c c' → fvis c' a = fvis c a

omit [Fintype F] [AddCommMonoid S] in
/-- Field policies that read no field content (they read edges, the viewer, other nodes) are
stable. -/
theorem fieldPolicyStable_of_const (p : A → R → F → Prop) :
    FieldPolicyStable (fun (_ : R → F → T) => p) :=
  fun _ _ _ _ => rfl

variable {sc : Scorer F T Qy S} {fvis : (R → F → T) → A → R → F → Prop} {q : Qy} {a : A}
  {c c' : R → F → T}

/-- **Scores ignore hidden fields.** Changing only the content of fields hidden from `a`
leaves every row's score for `a` unchanged. -/
theorem score_invariant (hst : FieldPolicyStable fvis) (hag : AgreeVisible fvis a c c') :
    score sc fvis q a c = score sc fvis q a c' := by
  funext r
  simp only [score, hst a c c' hag]
  apply Finset.sum_congr rfl
  intro f hf
  rw [hag r f (Finset.mem_filter.mp hf).2]

omit [Fintype F] [AddCommMonoid S] in
/-- **Matches ignore hidden fields.** -/
theorem matches_invariant (hst : FieldPolicyStable fvis) (hag : AgreeVisible fvis a c c') :
    Matches sc fvis q a c = Matches sc fvis q a c' := by
  funext r
  simp only [Matches, hst a c c' hag]
  apply propext
  constructor
  · rintro ⟨f, hv, hm⟩; exact ⟨f, hv, by rwa [← hag r f hv]⟩
  · rintro ⟨f, hv, hm⟩; exact ⟨f, hv, by rwa [hag r f hv]⟩

/-- **Ranking ignores hidden fields.** Any ranking, page cut, or cursor computed from the match
set and the scores is invariant under any change to the content of fields hidden from the
actor. -/
theorem ranking_invariant {β : Type*} (hst : FieldPolicyStable fvis)
    (hag : AgreeVisible fvis a c c') (rank : (R → Prop) → (R → S) → β) :
    rank (Matches sc fvis q a c) (score sc fvis q a c) =
      rank (Matches sc fvis q a c') (score sc fvis q a c') := by
  rw [matches_invariant hst hag, score_invariant hst hag]

/-- The search order: higher score first, ties broken by key. -/
def Before [LinearOrder S] {K : Type*} [LinearOrder K] (key : R → K) (sc : Scorer F T Qy S)
    (fvis : (R → F → T) → A → R → F → Prop) (q : Qy) (a : A) (c : R → F → T) (r₁ r₂ : R) :
    Prop :=
  score sc fvis q a c r₂ < score sc fvis q a c r₁ ∨
    (score sc fvis q a c r₁ = score sc fvis q a c r₂ ∧ key r₁ < key r₂)

theorem before_invariant [LinearOrder S] {K : Type*} [LinearOrder K] (key : R → K)
    (hst : FieldPolicyStable fvis) (hag : AgreeVisible fvis a c c') :
    Before key sc fvis q a c = Before key sc fvis q a c' := by
  unfold Before
  rw [score_invariant hst hag]

/-! ## The hypotheses are needed -/

/-- **A field policy that reads a hidden field leaks it.** Field `true` is always hidden;
field `false` is visible only while field `true` is `0`. Changing only the hidden field
`true` from `0` to `1` hides field `false`, which drops the row's score from `5` to `0` and
removes it from the matches. -/
theorem fieldPolicy_reading_hidden_leaks :
    let fv : (Unit → Bool → ℕ) → Unit → Unit → Bool → Prop :=
      fun c _ _ f => f = false ∧ c () true = 0
    let sc : Scorer Bool ℕ Unit ℕ := ⟨fun _ t _ => t, fun _ t _ => 0 < t⟩
    let c : Unit → Bool → ℕ := fun _ f => if f then 0 else 5
    let c' : Unit → Bool → ℕ := fun _ f => if f then 1 else 5
    AgreeVisible fv () c c' ∧ score sc fv () () c () = 5 ∧ score sc fv () () c' () = 0 ∧
      Matches sc fv () () c () ∧ ¬ Matches sc fv () () c' () ∧ ¬ FieldPolicyStable fv := by
  intro fv sc c c'
  have hag : AgreeVisible fv () c c' := by
    rintro ⟨⟩ f ⟨rfl, -⟩; rfl
  have h1 : score sc fv () () c () = 5 := by
    have : Finset.univ.filter (fv c () ()) = {false} := by
      ext f; cases f <;> simp [fv, c]
    simp [score, this, sc, c]
  have h2 : score sc fv () () c' () = 0 := by
    apply Finset.sum_eq_zero
    intro f hf
    simp only [Finset.mem_filter] at hf
    cases f <;> simp [fv, c'] at hf
  refine ⟨hag, h1, h2, ⟨false, by simp [fv, c], by simp [sc, c]⟩, ?_, ?_⟩
  · rintro ⟨f, ⟨-, h⟩, -⟩; simp [c'] at h
  · intro hst
    have := score_invariant (sc := sc) (q := ()) hst hag
    have := congrFun this ()
    rw [h1, h2] at this
    cases this

/-- Document frequency of term `t` over **every** field of every row, hidden or not. -/
def df (c : Fin 3 → Bool → ℕ) (t : ℕ) : ℕ :=
  (Finset.univ.filter (fun x : Fin 3 × Bool => c x.1 x.2 = t)).card

/-- An IDF-weighted score of a row's visible field `false` (term `0` means no match). -/
def idfScore (c : Fin 3 → Bool → ℕ) (r : Fin 3) : ℕ :=
  if c r false = 0 then 0 else 6 - df c (c r false)

/-- **Corpus statistics are outside the model.** Field `false` is visible and field `true`
hidden, in every row. Rows 0 and 1 match the query in their visible fields (terms 1 and 2)
and tie. Adding term 1 to row 2's hidden field makes term 1 more common, so row 1 now ranks
strictly above row 0, though no visible content changed. -/
theorem idf_outside_model :
    let c : Fin 3 → Bool → ℕ := fun r f => if f = false ∧ r = 0 then 1
      else if f = false ∧ r = 1 then 2 else 0
    let c' : Fin 3 → Bool → ℕ := fun r f => if f = true ∧ r = 2 then 1 else c r f
    (∀ r, c r false = c' r false) ∧ idfScore c 0 = idfScore c 1 ∧
      idfScore c' 0 < idfScore c' 1 := by
  intro c c'
  refine ⟨fun r => by simp [c'], ?_, ?_⟩ <;> decide

end Search
end Shep
