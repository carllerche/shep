/-
# Index backfill and ghost completeness (design.md §3, §10.3, §11.7)

A derived index is a function of the facts and the catalog (§3: "every index entry is a
function of the facts and the registry"). A new index is built at its **barrier** `b` in each
partition: the backfill writes the index of the partition's state at `b`, with the stamps of
the facts it read, and every later entry's facts are maintained incrementally, under the
catalog in force at that entry (§10.3).

## Results

* `backfill_correct`: for a linear index (one entry per fact, filtered and mapped: a local
  index), the stored index at **every later version `V`** (one containing everything the
  backfill read) is the index of the state at `V`.
* Ghost completeness, in the freshness model (`Fresh.History`). The *ideal* history stamps every
  index-entry change since the beginning; the stored index has versions only from the
  backfill on (`actualStamp`).
  * `actualStamp_some`, `actualStamp_none`: a stored stamp is the ideal one; a missing stamp
    hides only a change made before the barrier.
  * `backfilled_check_sound`: a freshness check against the stored index is sound for every
    `V` that contains everything the partition applied before the barrier.
  * `ghosts_incomplete`: **counterexample**. For an earlier `V`, one without the index's
    creation dot, the check answers "unchanged" falsely. **It also fails for a causally
    closed `V` that contains the creation dot**, if `V` lacks an entry the partition applied
    before its barrier. So "contains the creation dot" isn't the right test; "contains the
    partition's log up to its barrier" is.
-/
import ShepZSet.Stamps
import ShepZSet.Freshness

noncomputable section
open Classical

namespace Shep
namespace Backfill

/-! ## Backfill plus maintenance is the index of the state -/

variable {Fct E Pt Dot Cat : Type*}

/-- **Backfill is correct at every later version.** Let `B` be the dots the backfill read (the
partition's state at its barrier) and `L` the later dots, maintained incrementally. The
backfill's entries are the index of the state at `B`, under catalog `c`, each stamped with a
dot of `B` (the stamp of a fact it read). Each later dot's entries are the index of its delta
under the catalog in force at it, which is `c` after the barrier. Then for every version `V`
with `B ⊆ V ⊆ B ∪ L`, the stored index at `V` (backfill entries with stamp in `V`, plus the
entries of the later dots in `V`) is the index of the state at `V`. -/
theorem backfill_correct (ix : Cat → ZSet Fct → ZSet E) (c : Cat) (hlin : IsLinear (ix c))
    (delta : Dot → ZSet Fct) {B L : Finset Dot} (hBL : Disjoint B L)
    (bf : Finset Pt) (bstamp : Pt → Dot) (bval : Pt → ZSet E)
    (hbf_stamp : ∀ x ∈ bf, bstamp x ∈ B) (hbf : ∑ x ∈ bf, bval x = ix c (stateAt delta B))
    (catAt : Dot → Cat) (hcat : ∀ d ∈ L, catAt d = c)
    {V : Finset Dot} (hBV : B ⊆ V) (hVL : V ⊆ B ∪ L) :
    Stamps.stateAtS bf bstamp bval V + ∑ d ∈ L.filter (· ∈ V), ix (catAt d) (delta d) =
      ix c (stateAt delta V) := by
  have h1 : Stamps.stateAtS bf bstamp bval V = ix c (stateAt delta B) := by
    unfold Stamps.stateAtS
    rw [Finset.filter_true_of_mem (fun x hx => hBV (hbf_stamp x hx)), hbf]
  have h2 : ∑ d ∈ L.filter (· ∈ V), ix (catAt d) (delta d) =
      ix c (stateAt delta (L.filter (· ∈ V))) := by
    rw [Finset.sum_congr rfl (fun d hd => by rw [hcat d (Finset.mem_filter.mp hd).1])]
    exact (map_sum hlin.toHom _ _).symm
  have hV : V = B ∪ L.filter (· ∈ V) := by
    ext d
    simp only [Finset.mem_union, Finset.mem_filter]
    constructor
    · intro hd
      rcases Finset.mem_union.mp (hVL hd) with h | h
      · exact Or.inl h
      · exact Or.inr ⟨h, hd⟩
    · rintro (h | ⟨_, h⟩)
      · exact hBV h
      · exact h
  have hdis : Disjoint B (L.filter (· ∈ V)) :=
    hBL.mono_right (Finset.filter_subset _ _)
  have hsplit : stateAt delta V = stateAt delta B + stateAt delta (L.filter (· ∈ V)) := by
    unfold stateAt
    conv_lhs => rw [hV]
    rw [Finset.sum_union hdis]
  rw [h1, h2, ← hlin, hsplit]

/-! ## Ghost completeness -/

open Fresh

variable {K V' : Type*}

/-- The stored stamp of key `k` at `H`. Fact keys (`¬ isIx k`) carry their ideal stamps. An
index key carries the stamp of its newest maintained version if some dot of `H` after the
barrier (not in `Pre`) changed it; otherwise its backfilled version, if it was live at the
barrier, stamped with what the backfill read; otherwise nothing at all. -/
def actualStamp (h : History K (Option V') Dot) (isIx : K → Prop) (Pre H : Finset Dot)
    (k : K) : Option Dot :=
  if isIx k then
    if ∃ d ∈ H, d ∉ Pre ∧ h.Writes d k then h.stamp H k
    else if (h.valAt Pre k).isSome then h.stamp Pre k
    else none
  else h.stamp H k

variable {h : History K (Option V') Dot} {isIx : K → Prop} {Pre H : Finset Dot}

/-- If every writer of `k` in `H` is in `Pre ⊆ H`, `k`'s stamp at `H` is its stamp at `Pre`. -/
theorem stamp_eq_of_writers (hw : h.WF) (hPH : Pre ⊆ H) {k : K}
    (hno : ∀ d ∈ H, h.Writes d k → d ∈ Pre) : h.stamp H k = h.stamp Pre k := by
  cases hs : h.stamp Pre k with
  | none =>
    apply History.stamp_eq_none
    intro d hd hwd
    exact History.not_writes_of_stamp_none hw hs d (hno d hd hwd) hwd
  | some d =>
    have hl := History.isLast_of_stamp hs
    exact History.stamp_eq_some hw ⟨hPH hl.1, hl.2.1, fun e he hwe => hl.2.2 e (hno e he hwe) hwe⟩

theorem actualStamp_fact {k : K} (hk : ¬ isIx k) :
    actualStamp h isIx Pre H k = h.stamp H k := by
  unfold actualStamp; simp [hk]

/-- **A stored stamp is the ideal stamp.** -/
theorem actualStamp_some (hw : h.WF) (hPH : Pre ⊆ H) {k : K} {d : Dot}
    (hs : actualStamp h isIx Pre H k = some d) : h.stamp H k = some d := by
  by_cases hix : isIx k
  · unfold actualStamp at hs
    by_cases hpost : ∃ e ∈ H, e ∉ Pre ∧ h.Writes e k
    · simpa [hix, hpost] using hs
    · by_cases hlive : (h.valAt Pre k).isSome
      · simp only [hix, hpost, hlive, ite_true, ite_false] at hs
        push Not at hpost
        rw [stamp_eq_of_writers hw hPH (fun e he hwe => by
          by_contra hn; exact hpost e he hn hwe)]
        exact hs
      · simp [hix, hpost, hlive] at hs
  · rwa [actualStamp_fact hix] at hs

/-- **A missing stamp hides only a change made before the barrier.** -/
theorem actualStamp_none {k : K} {d : Dot}
    (hs : actualStamp h isIx Pre H k = none) (hd : h.stamp H k = some d) : d ∈ Pre := by
  by_cases hix : isIx k
  · by_cases hpost : ∃ e ∈ H, e ∉ Pre ∧ h.Writes e k
    · unfold actualStamp at hs
      simp only [hix, hpost, ite_true] at hs
      rw [hs] at hd; cases hd
    · have hl := History.isLast_of_stamp hd
      by_contra hn; exact hpost ⟨d, hl.1, hn, hl.2.1⟩
  · rw [actualStamp_fact hix, hd] at hs; cases hs

/-- **Freshness checks against a backfilled index are sound for every `V` that contains
everything the partition applied before the index's barrier.** If the query's visited set is
sound and every stored stamp it visits at `H` is in `V`, the answer at `V` is the answer at
`H`. -/
theorem backfilled_check_sound {β : Type*} (hw : h.WF) {V : Finset Dot} (hPV : Pre ⊆ V)
    (hVH : V ⊆ H) {Q : (K → Option V') → β} {visit : (K → Option V') → Finset K}
    (hQ : ReadSetSound Q visit)
    (hc : Check (actualStamp h isIx Pre H) V (visit (h.valAt H))) :
    Q (h.valAt V) = Q (h.valAt H) := by
  apply fresh_sound hw hVH hQ
  intro k hk d hd
  cases ha : actualStamp h isIx Pre H k with
  | none => exact hPV (actualStamp_none ha hd)
  | some d' =>
    have := actualStamp_some hw (hPV.trans hVH) ha
    rw [hd] at this
    cases this
    exact hc k hk _ ha

/-! ### Counterexample: a `V` that lacks a pre-barrier entry

One row in one bucket. In partition `p`, dot `0` creates the row (field `1`, a live entry) and
dot `1` (context `{0}`) moves it out of the range (a ghost, in the ideal history). Dot `2` is
the index's creation in the meta partition (context `∅`), and dot `3` is `p`'s barrier
(context `{0, 1, 2}`). So `Pre = {0, 1}`. At the barrier the row isn't in the range, so the
backfill writes nothing for it, and nothing after the barrier touches it: the stored index has
no version of the entry.

Both `V₁ = {0}` (earlier than the index) and `V₂ = {0, 2}` (containing the creation dot) are
causally closed and lack dot `1`. The check at `H = {0, 1, 2, 3}` passes for both, but the row
is returned at `V` and not at `H`. -/

def gCtx : ℕ → Finset ℕ
  | 1 => {0}
  | 3 => {0, 1, 2}
  | _ => ∅

def gWrite : ℕ → Item Unit Unit Unit Unit → Option (Option ℕ)
  | 0, .entry _ _ => some (some 0)
  | 0, .fact _ _ => some (some 1)
  | 1, .entry _ _ => some none
  | _, _ => none

def gHist : History (Item Unit Unit Unit Unit) (Option ℕ) ℕ := ⟨gCtx, gWrite, fun _ => none⟩

/-- Index keys are the entries. -/
def gIx : Item Unit Unit Unit Unit → Prop
  | .entry _ _ => True
  | _ => False

theorem gWrites {d : ℕ} {k : Item Unit Unit Unit Unit} (hw : gHist.Writes d k) :
    (d = 0 ∧ (k = .entry () () ∨ k = .fact () ())) ∨ (d = 1 ∧ k = .entry () ()) := by
  simp only [History.Writes, gHist] at hw
  rcases d with _ | _ | d <;> cases k <;> simp_all [gWrite]

theorem gHist_wf : gHist.WF where
  irrefl d := by rcases d with _ | _ | _ | _ | d <;> simp [gHist, gCtx]
  closed d := by
    intro e he x hx
    simp only [gHist] at he hx ⊢
    rcases d with _ | _ | _ | _ | d
    · simp [gCtx] at he
    · simp only [gCtx, Finset.mem_singleton] at he; subst he; simp [gCtx] at hx
    · simp [gCtx] at he
    · simp only [gCtx, Finset.mem_insert, Finset.mem_singleton] at he ⊢
      rcases he with rfl | rfl | rfl
      · simp [gCtx] at hx
      · simp only [gCtx, Finset.mem_singleton] at hx; omega
      · simp [gCtx] at hx
    · simp [gCtx] at he
  ordered d e k hne hd he := by
    have h1 := gWrites hd
    have h2 := gWrites he
    simp only [gHist]
    rcases h1 with ⟨rfl, -⟩ | ⟨rfl, -⟩ <;> rcases h2 with ⟨rfl, -⟩ | ⟨rfl, -⟩ <;>
      first | exact absurd rfl hne | simp [gCtx]

theorem ghosts_incomplete :
    let Pre : Finset ℕ := {0, 1}
    let H : Finset ℕ := {0, 1, 2, 3}
    ∀ V ∈ ({{0}, {0, 2}} : Finset (Finset ℕ)),
      gHist.WF ∧ V ⊆ H ∧ CausallyClosed gHist.ctx V ∧ ¬ Pre ⊆ V ∧
      Check (actualStamp gHist gIx Pre H) V (cexScan.visit () (gHist.valAt H)) ∧
      cexScan.eval () (gHist.valAt V) ≠ cexScan.eval () (gHist.valAt H) := by
  intro Pre H V hVmem
  have hw := gHist_wf
  have hV : V = {0} ∨ V = {0, 2} := by simpa using hVmem
  have hV0 : 0 ∈ V := by rcases hV with rfl | rfl <;> simp
  have hV1 : 1 ∉ V := by rcases hV with rfl | rfl <;> simp
  have hVs : ∀ e ∈ V, e = 0 ∨ e = 2 := by
    intro e he; rcases hV with rfl | rfl <;> simp at he <;> omega
  -- values at `V`
  have eF : gHist.valAt V (.fact () ()) = some 1 := by
    rw [History.valAt_of_isLast hw (d := 0)]
    · rfl
    · refine ⟨hV0, by simp [History.Writes, gHist, gWrite], fun e he hwe => ?_⟩
      rcases gWrites hwe with ⟨rfl, _⟩ | ⟨rfl, _⟩
      · exact Or.inl rfl
      · exact absurd he hV1
  have eE : gHist.valAt V (.entry () ()) = some 0 := by
    rw [History.valAt_of_isLast hw (d := 0)]
    · rfl
    · refine ⟨hV0, by simp [History.Writes, gHist, gWrite], fun e he hwe => ?_⟩
      rcases gWrites hwe with ⟨rfl, _⟩ | ⟨rfl, _⟩
      · exact Or.inl rfl
      · exact absurd he hV1
  -- values at `H`
  have eEH : gHist.valAt H (.entry () ()) = none := by
    rw [History.valAt_of_isLast hw (d := 1)]
    · rfl
    · refine ⟨by simp [H], by simp [History.Writes, gHist, gWrite], fun e he hwe => ?_⟩
      rcases gWrites hwe with ⟨rfl, _⟩ | ⟨rfl, _⟩
      · right; simp [gHist, gCtx]
      · exact Or.inl rfl
  have hcH : cexScan.cand (gHist.valAt H) = ∅ := by
    ext r; cases r; simp [Scan.cand, cexScan, eEH]
  have eVal : cexScan.eval () (gHist.valAt V) () = some () := by
    have hm : () ∈ cexScan.cand (gHist.valAt V) := by simp [Scan.cand, cexScan, eE]
    simp only [Scan.eval, Scan.Returned, hm, true_and]
    simp [cexScan, restrict, eF]
  have eHal : cexScan.eval () (gHist.valAt H) () = none := by
    simp [Scan.eval, Scan.Returned, hcH]
  refine ⟨hw, ?_, ?_, fun h => hV1 (h (by simp [Pre])), ?_, ?_⟩
  · intro x hx; rcases hVs x hx with rfl | rfl <;> simp [H]
  · intro d hd x hx
    rcases hVs d hd with rfl | rfl <;> simp [gHist, gCtx] at hx
  · intro k hk d hd
    exfalso
    simp only [Scan.visit, hcH, Finset.biUnion_empty, Finset.union_empty, Finset.mem_union,
      Finset.mem_insert, Finset.mem_singleton] at hk
    rcases hk with (rfl | rfl) | hk
    · rw [actualStamp_fact (by simp [gIx]), History.stamp_eq_none (fun e _ hwe => by
        rcases gWrites hwe with ⟨_, h | h⟩ | ⟨_, h⟩ <;> cases h)] at hd
      cases hd
    · rw [actualStamp_fact (by simp [gIx]), History.stamp_eq_none (fun e _ hwe => by
        rcases gWrites hwe with ⟨_, h | h⟩ | ⟨_, h⟩ <;> cases h)] at hd
      cases hd
    · simp only [Scan.entries, cexScan, Finset.mem_image] at hk
      obtain ⟨⟨b, r⟩, -, rfl⟩ := hk
      cases b; cases r
      -- the entry: no writer of it after the barrier, and not live at the barrier
      have hpost : ¬ ∃ e ∈ H, e ∉ Pre ∧ gHist.Writes e (.entry () ()) := by
        rintro ⟨e, -, hn, hwe⟩
        rcases gWrites hwe with ⟨rfl, _⟩ | ⟨rfl, _⟩ <;> simp [Pre] at hn
      have hPre : gHist.valAt Pre (.entry () ()) = none := by
        rw [History.valAt_of_isLast hw (d := 1)]
        · rfl
        · refine ⟨by simp [Pre], by simp [History.Writes, gHist, gWrite], fun e he hwe => ?_⟩
          rcases gWrites hwe with ⟨rfl, _⟩ | ⟨rfl, _⟩
          · right; simp [gHist, gCtx]
          · exact Or.inl rfl
      simp [actualStamp, gIx, hpost, hPre] at hd
  · intro heq
    have := congrFun heq ()
    rw [eVal, eHal] at this
    cases this

end Backfill
end Shep
