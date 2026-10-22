/-
# Leader-resolved field operations and the context rule (design.md §8, §9.5, §10.3)

Planner obligation 2. A field operation (`increment`, `add`, `push`, `max`, …) has no read of
its target. The leader resolves it into ordinary facts with old values and extends the entry's
context so that writes to one key stay causally ordered.

## The model (one partition; dots are log indices)

Entry `e` has a read version `S e`, a set of written keys `W e` (a key is `(node, path)`), and a
delta. Keys **conflict** (`Conf`) when they are on one node and their paths are
prefix-comparable.

* **Resolution.** The leader resolves against the newest write to the key that is *not in
  `S`*, in its window, and falls back to the base value from `S` (`lv`). It answers `too_old`
  iff `S`'s prefix is below the window floor. `window_complete` shows that above the floor,
  "newest in-window write not in `S`" is "newest write not in `S`".
* **`prevₖ`** (`prevK`) is that newest write not in `S`, or else the base's stamp: the newest
  write to the key in `S`.
* **The context rule** (`CtxRule`): `ctx(e) = S ∪ ⋃ₖ part(k)` over every key `k` that a
  conflicting earlier write wrote. `part(k)` is `{prevₖ} ∪ ctx(prevₖ)`, or, when `prevₖ` is
  still pending and has no index (`pend e k`), the **self-prefix** `{p: e−1}`, which in one
  partition expands to `range e`. The rule consults every earlier key that conflicts with a
  key `e` writes. When no two written keys are strictly prefix-related (scalar field leaves,
  the usual targets of field operations), that is the design's per-key rule. Otherwise the
  rule must also consult each conflicting key's last writer, as here: `ctx_ordered` needs it.
* **Deltas** are a function of the state at the context: `delta e = res e (stateAt (ctx e))`.

## Results

* `ctx_lt`, `ctx_irrefl`, `ctx_closed`, `selfPrefix_closed`: with either form of context,
  contexts are strictly earlier and causally closed (conditions 1 of `wf_stateAt_closed`).
* `ctx_ordered`: an earlier write to a conflicting key is in the context.
  `ctx_pathDisjoint`: concurrent dots write prefix-incomparable paths (condition 3).
* `ctxOf`, `ctxOf_rule`: the leader can compute such a context by recursion on the log.
* `leader_hist`: the same facts as the hypotheses of `Disciplines.lean`.
* `leader_wf`: **every causally closed version is valid**, given that each resolution is a
  valid transition from a valid state. Condition 2 is proved here, by induction over the log.
* `ctx_agrees_logpos`, `lv_eq_logpos`, `lv_eq_ctx`: the leader's value ("the newest write
  not in `S`, else the base from `S`") is the value at the entry's log position and at its
  context. So resolving against the context is resolving at the log position.
* `leaderVal_eq_serial`, `bounded_leader`: in the key-value log, the same resolution gives
  the serial value. A bounded operation resolved this way never crosses its bound, loses no
  update, and writes true old values, however proposals are pipelined.
* Counterexamples:
  * `ctxOmitPrev_not_set`: if the context omits `prevₖ`, two increments are concurrent, and the
    closed version holding both isn't a set.
  * `unrecordedRemoval_not_set`: if the last-value map doesn't record a removal, the leader
    resolves against the stale pre-removal value, and the result isn't a set. Recording
    `{key, absent}` fixes it.
  * `committedResolve_crosses_bound`: resolving against committed state only, with
    pipelining, accepts two `subtract(1)`s on a stock of 1. The net effect is `−1`, the log
    records `0` (a lost update), and the second delta's old value is false.
* **Each op equals the evaluator** (section `FieldOps`, restatement-round obligation 8): for every
  form in §8's table and every base value, including `{}`, the leader's op gives the ShepQL
  evaluator's result (`addOpt_eq`, `max_eq`, `pushOpt_eq`, …), and so, with
  `leaderVal_eq_serial`, the evaluator's result at the entry's log position
  (`resolved_eq_eval_at_logpos`). `optionalArg_unsets`: `e` must be One. `add_max_not_comm`:
  mixed op kinds on one field don't commute.
-/
import ShepZSet.Disciplines
import ShepZSet.Validation

noncomputable section
open Classical

namespace Shep
namespace LeaderOps

/-! ## The newest index below `e` with a property -/

/-- The greatest `j < e` with `P j`, if any. -/
def lastP (P : ℕ → Prop) (e : ℕ) : Option ℕ :=
  if ∃ j < e, P j then some (Nat.findGreatest (fun j => j < e ∧ P j) e) else none

theorem lastP_some {P : ℕ → Prop} {e j : ℕ} (h : lastP P e = some j) :
    j < e ∧ P j ∧ ∀ j', j < j' → j' < e → ¬ P j' := by
  unfold lastP at h
  split_ifs at h with hex
  obtain ⟨m, hm, hPm⟩ := hex
  have hj : Nat.findGreatest (fun j => j < e ∧ P j) e = j := Option.some_inj.mp h
  have hspec := Nat.findGreatest_spec (P := fun j => j < e ∧ P j) (m := m) hm.le ⟨hm, hPm⟩
  rw [hj] at hspec
  refine ⟨hspec.1, hspec.2, fun j' hjj hj' hP => ?_⟩
  exact Nat.findGreatest_is_greatest (P := fun j => j < e ∧ P j) (n := e) (by rw [hj]; exact hjj)
    hj'.le ⟨hj', hP⟩

theorem lastP_none {P : ℕ → Prop} {e : ℕ} (h : lastP P e = none) : ∀ j < e, ¬ P j := by
  intro j hj hP
  unfold lastP at h
  rw [(ite_eq_left_of_eq_true _ _ (eq_true ⟨j, hj, hP⟩))] at h
  cases h

theorem lastP_ge {P : ℕ → Prop} {e j : ℕ} (hj : j < e) (hP : P j) :
    ∃ j', lastP P e = some j' ∧ j ≤ j' := by
  refine ⟨_, (ite_eq_left_of_eq_true _ _ (eq_true ⟨j, hj, hP⟩)), ?_⟩
  exact Nat.le_findGreatest (P := fun j => j < e ∧ P j) hj.le ⟨hj, hP⟩

theorem lastP_eq_some {P : ℕ → Prop} {e j : ℕ} (hj : j < e) (hP : P j)
    (hmax : ∀ j', j < j' → j' < e → ¬ P j') : lastP P e = some j := by
  obtain ⟨j', h', hle⟩ := lastP_ge hj hP
  obtain ⟨hj'e, hPj', -⟩ := lastP_some h'
  rcases Nat.eq_or_lt_of_le hle with rfl | hlt
  · exact h'
  · exact absurd hPj' (hmax j' hlt hj'e)

theorem lastP_congr {P Q : ℕ → Prop} {e : ℕ} (h : ∀ j < e, P j ↔ Q j) : lastP P e = lastP Q e := by
  rcases hP : lastP P e with _ | j
  · rcases hQ : lastP Q e with _ | j'
    · rfl
    · obtain ⟨hj', hQj', -⟩ := lastP_some hQ
      exact absurd ((h j' hj').mpr hQj') (lastP_none hP j' hj')
  · obtain ⟨hj, hPj, hmax⟩ := lastP_some hP
    exact (lastP_eq_some hj ((h j hj).mp hPj)
      (fun j' h1 h2 hq => hmax j' h1 h2 ((h j' h2).mpr hq))).symm

/-! ## Keys, conflicts, and the context rule -/

variable {N Seg V : Type*}

/-- A key: a node and a path. -/
abbrev Key (N Seg : Type*) := N × Path Seg

/-- Two keys conflict: same node, prefix-comparable paths. -/
def Conf (k k' : Key N Seg) : Prop := k.1 = k'.1 ∧ (k.2 <+: k'.2 ∨ k'.2 <+: k.2)

theorem conf_refl (k : Key N Seg) : Conf k k := ⟨rfl, Or.inl (List.prefix_refl _)⟩

theorem conf_symm {k k' : Key N Seg} (h : Conf k k') : Conf k' k := ⟨h.1.symm, h.2.symm⟩

variable (S : ℕ → Finset ℕ) (W : ℕ → Finset (Key N Seg)) (pend : ℕ → Key N Seg → Prop)

/-- **`prevₖ`**: the newest write to `k` before `e` that is not in `S e`, or else the base's
stamp, the newest write to `k` in `S e`. -/
def prevK (e : ℕ) (k : Key N Seg) : Option ℕ :=
  (lastP (fun j => k ∈ W j ∧ j ∉ S e) e).or (lastP (fun j => k ∈ W j ∧ j ∈ S e) e)

/-- The keys whose last writer the rule consults: keys written before `e` that conflict with
a key `e` writes. -/
def keysBefore (e : ℕ) : Finset (Key N Seg) :=
  ((Finset.range e).biUnion W).filter (fun k' => ∃ k ∈ W e, Conf k k')

/-- What key `k'` adds to `ctx e`: the self-prefix if `prevₖ` is pending, else `prevₖ` and its
context. -/
def part (ctx : ℕ → Finset ℕ) (e : ℕ) (k' : Key N Seg) : Finset ℕ :=
  if pend e k' then Finset.range e else (prevK S W e k').elim ∅ (fun j => insert j (ctx j))

/-- **The leader's context rule.** -/
def CtxRule (ctx : ℕ → Finset ℕ) : Prop :=
  ∀ e, ctx e = S e ∪ (keysBefore W e).biUnion (part S W pend ctx e)

variable {S W pend} {ctx : ℕ → Finset ℕ}

theorem prevK_lt {e j : ℕ} {k : Key N Seg} (h : prevK S W e k = some j) : j < e := by
  unfold prevK at h
  rcases h₁ : lastP (fun j => k ∈ W j ∧ j ∉ S e) e with _ | j₁
  · rw [h₁, Option.none_or] at h; exact (lastP_some h).1
  · rw [h₁, Option.some_or] at h; cases h; exact (lastP_some h₁).1

/-- **A context satisfying the rule exists**: the leader computes it by recursion on the log
(`prevₖ` is strictly earlier). So `CtxRule` isn't vacuous. -/
def ctxOf (S : ℕ → Finset ℕ) (W : ℕ → Finset (Key N Seg)) (pend : ℕ → Key N Seg → Prop)
    (e : ℕ) : Finset ℕ :=
  S e ∪ (keysBefore W e).attach.biUnion (fun k' =>
    if pend e k'.1 then Finset.range e else
    match _h : prevK S W e k'.1 with
    | some j => insert j (ctxOf S W pend j)
    | none => ∅)
termination_by e
decreasing_by exact prevK_lt _h

theorem ctxOf_rule (S : ℕ → Finset ℕ) (W : ℕ → Finset (Key N Seg))
    (pend : ℕ → Key N Seg → Prop) : CtxRule S W pend (ctxOf S W pend) := by
  intro e
  rw [ctxOf]
  congr 1
  ext d
  simp only [Finset.mem_biUnion, Finset.mem_attach, true_and, Subtype.exists]
  apply exists_congr; intro k'
  rw [exists_prop]
  apply and_congr_right; intro _
  unfold part
  split_ifs with hp
  · rfl
  · split <;> simp_all

theorem S_sub_ctx (hr : CtxRule S W pend ctx) (e : ℕ) : S e ⊆ ctx e := by
  rw [hr e]; exact Finset.subset_union_left

theorem part_sub_ctx (hr : CtxRule S W pend ctx) {e : ℕ} {k' : Key N Seg}
    (hk : k' ∈ keysBefore W e) : part S W pend ctx e k' ⊆ ctx e := by
  rw [hr e]
  exact (Finset.subset_biUnion_of_mem _ hk).trans Finset.subset_union_right

theorem mem_ctx (hr : CtxRule S W pend ctx) {e d : ℕ} (h : d ∈ ctx e) :
    d ∈ S e ∨ ∃ k' ∈ keysBefore W e, d ∈ part S W pend ctx e k' := by
  rw [hr e, Finset.mem_union, Finset.mem_biUnion] at h; exact h

theorem mem_part {e d : ℕ} {k' : Key N Seg} (h : d ∈ part S W pend ctx e k') :
    d < e ∨ ∃ j, prevK S W e k' = some j ∧ (d = j ∨ d ∈ ctx j) := by
  unfold part at h
  split_ifs at h
  · exact Or.inl (Finset.mem_range.mp h)
  · right
    rcases hp : prevK S W e k' with _ | j
    · rw [hp] at h; simp at h
    · rw [hp] at h
      simp only [Option.elim, Finset.mem_insert] at h
      exact ⟨j, rfl, h⟩

/-- **Contexts are strictly earlier**, given that `S` names no entry at or after `e`. -/
theorem ctx_lt (hr : CtxRule S W pend ctx) (hS : ∀ e, ∀ j ∈ S e, j < e) :
    ∀ e, ∀ d ∈ ctx e, d < e := by
  intro e
  induction e using Nat.strong_induction_on with
  | _ e ih =>
  intro d hd
  rcases mem_ctx hr hd with h | ⟨k', -, h⟩
  · exact hS e d h
  · rcases mem_part h with h | ⟨j, hj, rfl | h⟩
    · exact h
    · exact prevK_lt hj
    · exact (ih j (prevK_lt hj) d h).trans (prevK_lt hj)

theorem ctx_irrefl (hr : CtxRule S W pend ctx) (hS : ∀ e, ∀ j ∈ S e, j < e) (e : ℕ) :
    e ∉ ctx e := fun h => lt_irrefl e (ctx_lt hr hS e e h)

/-- **The self-prefix is causally closed**: in one partition, `{p: e−1}` expands to
`range e`, which holds the context of each of its entries. -/
theorem selfPrefix_closed (hr : CtxRule S W pend ctx) (hS : ∀ e, ∀ j ∈ S e, j < e) (e : ℕ) :
    CausallyClosed ctx (Finset.range e) := by
  intro d hd x hx
  exact Finset.mem_range.mpr ((ctx_lt hr hS d x hx).trans (Finset.mem_range.mp hd))

/-- **Contexts are causally closed**, with either form of `part`. -/
theorem ctx_closed (hr : CtxRule S W pend ctx) (hS : ∀ e, ∀ j ∈ S e, j < e)
    (hSc : ∀ e, CausallyClosed ctx (S e)) : ∀ e, CausallyClosed ctx (ctx e) := by
  intro e
  induction e using Nat.strong_induction_on with
  | _ e ih =>
  intro d hd
  rcases mem_ctx hr hd with h | ⟨k', hk', h⟩
  · exact (hSc e d h).trans (S_sub_ctx hr e)
  · have hsub := part_sub_ctx hr hk'
    unfold part at h hsub
    split_ifs at h hsub
    · exact (selfPrefix_closed hr hS e d h).trans hsub
    · rcases hp : prevK S W e k' with _ | j
      · rw [hp] at h; simp at h
      · rw [hp] at h hsub
        simp only [Option.elim, Finset.mem_insert] at h hsub
        rcases h with rfl | h
        · exact (Finset.subset_insert _ _).trans hsub
        · exact (ih j (prevK_lt hp) d h).trans ((Finset.subset_insert _ _).trans hsub)

/-- **Writes to conflicting keys are causally ordered**: an earlier entry that writes a key
conflicting with one that `e` writes is in `e`'s context. -/
theorem ctx_ordered (hr : CtxRule S W pend ctx) :
    ∀ e d, d < e → ∀ k ∈ W e, ∀ k' ∈ W d, Conf k k' → d ∈ ctx e := by
  intro e
  induction e using Nat.strong_induction_on with
  | _ e ih =>
  intro d hde k hk k' hk' hc
  have hkb : k' ∈ keysBefore W e := by
    simp only [keysBefore, Finset.mem_filter, Finset.mem_biUnion, Finset.mem_range]
    exact ⟨⟨d, hde, hk'⟩, k, hk, hc⟩
  have hsub := part_sub_ctx hr hkb
  by_cases hp : pend e k'
  · apply hsub
    simp only [part, hp, ↓reduceIte, Finset.mem_range]
    exact hde
  by_cases hdS : d ∈ S e
  · exact S_sub_ctx hr e hdS
  obtain ⟨j, hj, hdj⟩ := lastP_ge (P := fun j => k' ∈ W j ∧ j ∉ S e) hde ⟨hk', hdS⟩
  have hprev : prevK S W e k' = some j := by simp [prevK, hj]
  have hjsub : insert j (ctx j) ⊆ ctx e := by
    simpa [part, hp, hprev] using hsub
  obtain ⟨hje, ⟨hk'j, -⟩, -⟩ := lastP_some hj
  rcases Nat.eq_or_lt_of_le hdj with rfl | hlt
  · exact hjsub (Finset.mem_insert_self _ _)
  · exact hjsub (Finset.mem_insert_of_mem (ih j hje d hlt k' hk'j k' hk' (conf_refl k')))

variable {delta : ℕ → ZSet (Fact N Seg V)}

/-- **Concurrent dots write prefix-incomparable paths** (condition 3 of `wf_stateAt_closed`),
given that each delta touches only the keys its entry writes. -/
theorem ctx_pathDisjoint (hr : CtxRule S W pend ctx)
    (hfp : ∀ i x, delta i x ≠ 0 → (x.1, x.2.1) ∈ W i) :
    ∀ d e, d ≠ e → d ∉ ctx e → e ∉ ctx d → PathDisjoint (delta d) (delta e) := by
  intro d e hne hde hed n p q v w hp hq hcmp
  have hpW := hfp d _ hp
  have hqW := hfp e _ hq
  rcases lt_trichotomy d e with hlt | rfl | hgt
  · exact hde (ctx_ordered hr e d hlt _ hqW _ hpW ⟨rfl, hcmp.symm⟩)
  · exact hne rfl
  · exact hed (ctx_ordered hr d e hgt _ hpW _ hqW ⟨rfl, hcmp⟩)

/-- **Leader-resolved logs have valid states at every causally closed version.** Each delta
is the leader's resolution of the state at its context, each resolution is a valid transition
from a valid state, and each delta touches only its written keys. -/
theorem leader_wf {kind : Path Seg → Kind} (hr : CtxRule S W pend ctx)
    (hS : ∀ e, ∀ j ∈ S e, j < e) (hSc : ∀ e, CausallyClosed ctx (S e))
    (res : ℕ → ZSet (Fact N Seg V) → ZSet (Fact N Seg V))
    (hdelta : ∀ i, delta i = res i (stateAt delta (ctx i)))
    (hres : ∀ i σ, WF kind σ → WF kind (σ + res i σ))
    (hfp : ∀ i x, delta i x ≠ 0 → (x.1, x.2.1) ∈ W i) :
    ∀ X, CausallyClosed ctx X → WF kind (stateAt delta X) := by
  have hlt := ctx_lt hr hS
  have key : ∀ m X, CausallyClosed ctx X → (∀ d ∈ X, d < m) → WF kind (stateAt delta X) := by
    intro m
    induction m with
    | zero =>
      intro X _ hX
      have : X = ∅ := Finset.eq_empty_of_forall_notMem (fun d hd => absurd (hX d hd) (by omega))
      subst this
      simpa [stateAt] using (wf_zero : WF kind (0 : ZSet (Fact N Seg V)))
    | succ m ih =>
      intro X hXc hX
      let δ' : ℕ → ZSet (Fact N Seg V) := fun d => if d < m + 1 then delta d else 0
      let c' : ℕ → Finset ℕ := fun d => if d < m + 1 then ctx d else ∅
      have hst : ∀ Y : Finset ℕ, (∀ d ∈ Y, d < m + 1) → stateAt δ' Y = stateAt delta Y :=
        fun Y hY => Finset.sum_congr rfl (fun d hd => (ite_eq_left_of_eq_true _ _ (eq_true (hY d hd))))
      have hWF := wf_stateAt_closed (kind := kind) δ' c' ?_ ?_ ?_ ?_ X ?_
      · rwa [hst X hX] at hWF
      · intro d
        by_cases hd : d < m + 1
        · simp only [c', hd, ↓reduceIte]; exact ctx_irrefl hr hS d
        · simp [c', hd]
      · intro d x hx
        by_cases hd : d < m + 1
        · simp only [c', hd, ↓reduceIte] at hx ⊢
          have hxd := hlt d x hx
          simp only [show x < m + 1 by omega, ↓reduceIte]
          exact ctx_closed hr hS hSc d x hx
        · simp [c', hd] at hx
      · intro d
        by_cases hd : d < m + 1
        · have hc : c' d = ctx d := (ite_eq_left_of_eq_true _ _ (eq_true hd))
          have hδ : δ' d = delta d := (ite_eq_left_of_eq_true _ _ (eq_true hd))
          rw [hc, hδ, hst (ctx d) (fun x hx => by have := hlt d x hx; omega)]
          rw [hdelta d]
          exact hres d _ (ih (ctx d) (ctx_closed hr hS hSc d)
            (fun x hx => by have := hlt d x hx; omega))
        · have hc : c' d = ∅ := (ite_eq_right_of_eq_false _ _ (eq_false hd))
          have hδ : δ' d = 0 := (ite_eq_right_of_eq_false _ _ (eq_false hd))
          rw [hc, hδ]
          simpa [stateAt] using (wf_zero : WF kind (0 : ZSet (Fact N Seg V)))
      · intro d e hne hde hed
        by_cases hd : d < m + 1
        · by_cases he : e < m + 1
          · have hδd : δ' d = delta d := (ite_eq_left_of_eq_true _ _ (eq_true hd))
            have hδe : δ' e = delta e := (ite_eq_left_of_eq_true _ _ (eq_true he))
            have hcd : c' d = ctx d := (ite_eq_left_of_eq_true _ _ (eq_true hd))
            have hce : c' e = ctx e := (ite_eq_left_of_eq_true _ _ (eq_true he))
            rw [hcd] at hed; rw [hce] at hde
            rw [hδd, hδe]
            exact ctx_pathDisjoint hr hfp d e hne hde hed
          · have hδe : δ' e = 0 := (ite_eq_right_of_eq_false _ _ (eq_false he))
            rw [hδe]; intro _ _ _ _ _ _ h; exact absurd rfl h
        · have hδd : δ' d = 0 := (ite_eq_right_of_eq_false _ _ (eq_false hd))
          rw [hδd]; intro _ _ _ _ _ h; exact absurd rfl h
      · intro d hd x hx
        have hd' := hX d hd
        simp only [c', hd', ↓reduceIte] at hx
        exact hXc d hd hx
  intro X hX
  exact key (X.sup id + 1) X hX (fun d hd => Nat.lt_succ_of_le (Finset.le_sup (f := id) hd))

/-- **The leader's contexts satisfy the causal hypotheses of `Disciplines.lean`** (`Disc.Hist`),
so the discipline results apply to leader-resolved logs. -/
theorem leader_hist (hr : CtxRule S W pend ctx) (hS : ∀ e, ∀ j ∈ S e, j < e)
    (hSc : ∀ e, CausallyClosed ctx (S e)) (hfp : ∀ i x, delta i x ≠ 0 → (x.1, x.2.1) ∈ W i) :
    Disc.Hist delta ctx :=
  ⟨ctx_irrefl hr hS, ctx_closed hr hS hSc, ctx_pathDisjoint hr hfp⟩

/-! ## The leader's value is the value at the log position -/

/-- On the keys an entry writes (and keys conflicting with them), **the state at its context
equals the state at its log position**: every earlier entry, committed or not. -/
theorem ctx_agrees_logpos (hr : CtxRule S W pend ctx) (hS : ∀ e, ∀ j ∈ S e, j < e)
    (hfp : ∀ i x, delta i x ≠ 0 → (x.1, x.2.1) ∈ W i) {e : ℕ} {k : Key N Seg} (hk : k ∈ W e)
    {x : Fact N Seg V} (hx : Conf k (x.1, x.2.1)) :
    stateAt delta (ctx e) x = stateAt delta (Finset.range e) x := by
  simp only [stateAt, Finsupp.finsetSum_apply]
  apply Finset.sum_subset
  · intro d hd; exact Finset.mem_range.mpr (ctx_lt hr hS e d hd)
  · intro d hd hdc
    by_contra hne
    exact hdc (ctx_ordered hr e d (Finset.mem_range.mp hd) k hk _ (hfp d x hne) hx)

/-- **The leader's resolution value** for key `k` at entry `e`: the state after the newest
write to `k` not in `S e`, or else the state at `S e` (the base). -/
def lv (S : ℕ → Finset ℕ) (W : ℕ → Finset (Key N Seg)) (ctx : ℕ → Finset ℕ)
    (delta : ℕ → ZSet (Fact N Seg V)) (e : ℕ) (k : Key N Seg) (v : V) : ℤ :=
  match lastP (fun j => k ∈ W j ∧ j ∉ S e) e with
  | some j => stateAt delta (insert j (ctx j)) (k.1, k.2, v)
  | none => stateAt delta (S e) (k.1, k.2, v)

/-- **The leader's value is the value at the log position.** -/
theorem lv_eq_logpos (hr : CtxRule S W pend ctx) (hS : ∀ e, ∀ j ∈ S e, j < e)
    (hSc : ∀ e, CausallyClosed ctx (S e))
    (hfp : ∀ i x, delta i x ≠ 0 → (x.1, x.2.1) ∈ W i) (e : ℕ) (k : Key N Seg) (v : V) :
    lv S W ctx delta e k v = stateAt delta (Finset.range e) (k.1, k.2, v) := by
  have hwr : ∀ d, delta d (k.1, k.2, v) ≠ 0 → k ∈ W d := fun d h => hfp d _ h
  have hord : ∀ d j, d < j → k ∈ W j → k ∈ W d → d ∈ ctx j :=
    fun d j hdj hj hd => ctx_ordered hr j d hdj k hj k hd (conf_refl k)
  unfold lv
  rcases hl : lastP (fun j => k ∈ W j ∧ j ∉ S e) e with _ | j
  · simp only [stateAt, Finsupp.finsetSum_apply]
    apply Finset.sum_subset
    · intro d hd; exact Finset.mem_range.mpr (hS e d hd)
    · intro d hd hdS
      by_contra hne
      exact lastP_none hl d (Finset.mem_range.mp hd) ⟨hwr d hne, hdS⟩
  · obtain ⟨hje, ⟨hkj, hjS⟩, hmax⟩ := lastP_some hl
    simp only [stateAt, Finsupp.finsetSum_apply]
    apply Finset.sum_subset
    · intro d hd
      rw [Finset.mem_insert] at hd
      rcases hd with rfl | hd
      · exact Finset.mem_range.mpr hje
      · exact Finset.mem_range.mpr ((ctx_lt hr hS j d hd).trans hje)
    · intro d hd hdn
      by_contra hne
      have hdk := hwr d hne
      have hde := Finset.mem_range.mp hd
      rw [Finset.mem_insert, not_or] at hdn
      rcases lt_trichotomy d j with hlt | rfl | hgt
      · exact hdn.2 (hord d j hlt hkj hdk)
      · exact hdn.1 rfl
      · by_cases hdS : d ∈ S e
        · exact hjS (hSc e d hdS (hord j d hgt hdk hkj))
        · exact hmax d hgt hde ⟨hdk, hdS⟩

/-- **Resolving against the context is resolving at the log position**, for the keys the entry
writes. -/
theorem lv_eq_ctx (hr : CtxRule S W pend ctx) (hS : ∀ e, ∀ j ∈ S e, j < e)
    (hSc : ∀ e, CausallyClosed ctx (S e))
    (hfp : ∀ i x, delta i x ≠ 0 → (x.1, x.2.1) ∈ W i) {e : ℕ} {k : Key N Seg} (hk : k ∈ W e)
    (v : V) : lv S W ctx delta e k v = stateAt delta (ctx e) (k.1, k.2, v) := by
  rw [lv_eq_logpos hr hS hSc hfp, ctx_agrees_logpos hr hS hfp hk (conf_refl k)]

/-- **The window is complete when `S` isn't too old.** If `S`'s prefix `p` is at or above the
window floor, the newest in-window write not in `S` is the newest write not in `S`. -/
theorem window_complete {e p floor : ℕ} {k : Key N Seg} (hpre : ∀ j < p, j ∈ S e)
    (hfl : floor ≤ p) :
    lastP (fun j => floor ≤ j ∧ k ∈ W j ∧ j ∉ S e) e = lastP (fun j => k ∈ W j ∧ j ∉ S e) e := by
  apply lastP_congr
  intro j _
  constructor
  · exact fun h => h.2
  · intro h
    refine ⟨?_, h⟩
    by_contra hj
    exact h.2 (hpre j (by omega))

/-! ## Counterexample: a context that omits `prevₖ`

A counter on key `c = ((), [])`. Entry `0` creates it at `0`. Entries `1` and `2` each increment
it, read at `S = {0}`, and are resolved against the state at their context (`incr`). The rule
puts entry `1` (`prevₖ` for entry `2`) in entry `2`'s context. If the context omits it, both
increments resolve `0 → 1`. The closed version `{0, 1, 2}` then has weight `−1` on `(c, 0)`. -/

/-- A counter fact. -/
def cf (v : ℕ) : Fact Unit Unit ℕ := ((), [], v)

/-- The leader's increment: each live `(c, x)` becomes `(c, x + 1)`. -/
def incr (σ : ZSet (Fact Unit Unit ℕ)) : ZSet (Fact Unit Unit ℕ) :=
  Finsupp.mapDomain (fun f : Fact Unit Unit ℕ => (f.1, f.2.1, f.2.2 + 1)) σ - σ

theorem incr_single (v : ℕ) : incr (Finsupp.single (cf v) 1) =
    Finsupp.single (cf (v + 1)) 1 - Finsupp.single (cf v) 1 := by
  simp [incr, Finsupp.mapDomain_single, cf]

/-- The bad context: `S = {0}` only. -/
def badCtx : ℕ → Finset ℕ
  | 0 => ∅
  | _ => {0}

def badDelta : ℕ → ZSet (Fact Unit Unit ℕ)
  | 0 => Finsupp.single (cf 0) 1
  | 1 => Finsupp.single (cf 1) 1 - Finsupp.single (cf 0) 1
  | 2 => Finsupp.single (cf 1) 1 - Finsupp.single (cf 0) 1
  | _ => 0

/-- The rule's context: entry `2`'s context holds `prevₖ = 1` and its context. -/
def goodCtx : ℕ → Finset ℕ
  | 0 => ∅
  | 1 => {0}
  | _ => {0, 1}

def goodDelta : ℕ → ZSet (Fact Unit Unit ℕ)
  | 0 => Finsupp.single (cf 0) 1
  | 1 => Finsupp.single (cf 1) 1 - Finsupp.single (cf 0) 1
  | 2 => Finsupp.single (cf 2) 1 - Finsupp.single (cf 1) 1
  | _ => 0

theorem ctxOmitPrev_not_set :
    -- each increment is resolved against the state at its (bad) context
    badDelta 1 = incr (stateAt badDelta (badCtx 1)) ∧
    badDelta 2 = incr (stateAt badDelta (badCtx 2)) ∧
    -- the bad context omits prevₖ = 1, so entries 1 and 2 are concurrent
    1 ∉ badCtx 2 ∧ 2 ∉ badCtx 1 ∧
    -- `{0, 1, 2}` is causally closed, but its state isn't a set
    CausallyClosed badCtx {0, 1, 2} ∧
    stateAt badDelta {0, 1, 2} (cf 0) = -1 ∧ ¬ IsSet (stateAt badDelta {0, 1, 2}) ∧
    -- with the rule's context, entry 2 resolves `1 → 2`, and the state is `{(c, 2)}`
    goodDelta 2 = incr (stateAt goodDelta (goodCtx 2)) ∧
    stateAt goodDelta {0, 1, 2} = Finsupp.single (cf 2) 1 := by
  have h0 : stateAt badDelta {0} = Finsupp.single (cf 0) 1 := by simp [stateAt, badDelta]
  have hm : stateAt badDelta {0, 1, 2} (cf 0) = -1 := by
    simp [stateAt, badDelta, cf]
  refine ⟨?_, ?_, by simp [badCtx], by simp [badCtx], ?_, hm, ?_, ?_, ?_⟩
  · simp only [badCtx]; rw [h0, incr_single]; rfl
  · simp only [badCtx]; rw [h0, incr_single]; rfl
  · intro d hd x hx
    simp only [Finset.mem_insert, Finset.mem_singleton] at hd
    rcases hd with rfl | rfl | rfl <;> simp_all [badCtx]
  · intro h; rcases h (cf 0) with h | h <;> rw [hm] at h <;> omega
  · have : stateAt goodDelta {0, 1} = Finsupp.single (cf 1) 1 := by
      simp [stateAt, goodDelta]
    simp only [goodCtx]; rw [this, incr_single]; rfl
  · simp [stateAt, goodDelta]

/-! ## Counterexample: a removal missing from the last-value map

Entry `0` sets the counter to `5`. Entry `1` removes it (`−(c, 5)`, no new value). Entry `2`
increments it, read at `S = ∅`. The newest write not in `S` is entry `1`, whose value is
"absent". A last-value map that records only present values has no entry for `1`, so it
resolves entry `2` against entry `0`'s `5`, and the closed version `{0, 1, 2}` has weight `−1`
on `(c, 5)`. With an `{key, absent}` record the leader resolves against entry `1`. The counter is
absent there, so `incr` writes nothing, and the state is empty. -/

def rmCtxBad : ℕ → Finset ℕ
  | 0 => ∅
  | _ => {0}

def rmDelta : ℕ → ZSet (Fact Unit Unit ℕ)
  | 0 => Finsupp.single (cf 5) 1
  | 1 => -Finsupp.single (cf 5) 1
  | 2 => Finsupp.single (cf 6) 1 - Finsupp.single (cf 5) 1
  | _ => 0

def rmDeltaGood : ℕ → ZSet (Fact Unit Unit ℕ)
  | 0 => Finsupp.single (cf 5) 1
  | 1 => -Finsupp.single (cf 5) 1
  | _ => 0

def rmCtxGood : ℕ → Finset ℕ
  | 0 => ∅
  | 1 => {0}
  | _ => {0, 1}

def rmW : ℕ → Finset (Key Unit Unit) := fun _ => {((), [])}

/-- An entry's value of key `k` is **recorded** in a present-values-only map if the key holds a
live fact after the entry. -/
def Recorded (delta : ℕ → ZSet (Fact Unit Unit ℕ)) (ctx : ℕ → Finset ℕ) (j : ℕ)
    (k : Key Unit Unit) : Prop :=
  ∃ v, stateAt delta (insert j (ctx j)) (k.1, k.2, v) ≠ 0

theorem unrecordedRemoval_not_set :
    let k : Key Unit Unit := ((), [])
    -- with `S = ∅`, the newest write not in `S` is entry 1 (the removal)
    lastP (fun j => k ∈ rmW j ∧ j ∉ (∅ : Finset ℕ)) 2 = some 1 ∧
    -- a present-values-only map skips it and returns entry 0
    lastP (fun j => k ∈ rmW j ∧ j ∉ (∅ : Finset ℕ) ∧ Recorded rmDelta rmCtxBad j k) 2 = some 0 ∧
    -- resolving against entry 0's state gives a delta whose closed state isn't a set
    rmDelta 2 = incr (stateAt rmDelta (insert 0 (rmCtxBad 0))) ∧
    CausallyClosed rmCtxBad {0, 1, 2} ∧
    stateAt rmDelta {0, 1, 2} (cf 5) = -1 ∧ ¬ IsSet (stateAt rmDelta {0, 1, 2}) ∧
    -- with absent-value records it resolves against entry 1: nothing to increment
    incr (stateAt rmDeltaGood (insert 1 (rmCtxGood 1))) = 0 ∧
    stateAt rmDeltaGood {0, 1, 2} = 0 := by
  intro k
  have hs01 : stateAt rmDelta {0, 1} = 0 := by simp [stateAt, rmDelta]
  have hm : stateAt rmDelta {0, 1, 2} (cf 5) = -1 := by
    simp [stateAt, rmDelta, cf]
  refine ⟨?_, ?_, ?_, ?_, hm, ?_, ?_, ?_⟩
  · apply lastP_eq_some (by omega) (by simp [rmW, k])
    intro j h1 h2; omega
  · apply lastP_eq_some (by omega)
    · refine ⟨by simp [rmW, k], by simp, 5, ?_⟩
      simp [rmCtxBad, stateAt, rmDelta, k, cf]
    · intro j h1 h2 ⟨_, _, v, hv⟩
      have : j = 1 := by omega
      subst this
      have hins : insert 1 (rmCtxBad 1) = {0, 1} := by decide
      rw [hins, hs01] at hv
      exact hv rfl
  · have : stateAt rmDelta (insert 0 (rmCtxBad 0)) = Finsupp.single (cf 5) 1 := by
      simp [stateAt, rmCtxBad, rmDelta]
    rw [this, incr_single]; rfl
  · intro d hd x hx
    simp only [Finset.mem_insert, Finset.mem_singleton] at hd
    rcases hd with rfl | rfl | rfl <;> simp_all [rmCtxBad]
  · intro h; rcases h (cf 5) with h | h <;> rw [hm] at h <;> omega
  · have : stateAt rmDeltaGood (insert 1 (rmCtxGood 1)) = 0 := by
      simp [stateAt, rmCtxGood, rmDeltaGood]
    rw [this]; simp [incr]
  · simp [stateAt, rmDeltaGood]

/-! ## Bounded operations in the key-value log

The log of `Validation.lean`. Entry `j` writes key `k` with `(old, new)`. The leader resolves
an operation on `k` at entry `e` against `leaderVal`: the value written by the newest entry
before `e` whose stamp is not in `S` and that writes `k`, or else the value at `S`. -/

section Bounded

open Validation

variable {K Val Dot : Type*} {L : ℕ → Entry K Val Dot} {init : K → Val}

/-- The newest entry before `e` that writes `k` and is not in `S`. -/
def lastNotIn (L : ℕ → Entry K Val Dot) (S : Finset Dot) (e : ℕ) (k : K) : Option ℕ :=
  lastP (fun j => (L j).stamp ∉ S ∧ Writes (L j) k) e

/-- The value entry `j` wrote to `k`. -/
def newVal (L : ℕ → Entry K Val Dot) (init : K → Val) (j : ℕ) (k : K) : Val :=
  ((L j).wr k).elim (init k) Prod.snd

/-- **The leader's resolution value**: the newest write not in `S`, else the base from `S`. -/
def leaderVal (L : ℕ → Entry K Val Dot) (init : K → Val) (S : Finset Dot) (e : ℕ) (k : K) : Val :=
  match lastNotIn L S e k with
  | some j => newVal L init j k
  | none => readAt L init S e k

theorem serial_noWrite {k : K} {a : ℕ} (h : ∀ m, a ≤ m → ¬ Writes (L m) k) :
    ∀ b, a ≤ b → serial L init b k = serial L init a k := by
  intro b hab
  induction b with
  | zero => have : a = 0 := by omega
            subst this; rfl
  | succ b ih =>
    rcases Nat.eq_or_lt_of_le hab with heq | hlt
    · rw [heq]
    · have hw : (L b).wr k = none := by
        cases hx : (L b).wr k with
        | none => rfl
        | some ov => exact absurd ⟨ov, hx⟩ (h b (by omega))
      simp only [serial, apply_none hw]
      exact ih (by omega)

/-- **The leader's value is the serial value at the entry's position**, given that writes to
`k` are causally ordered in log order (`ctx_ordered`, `validated_writes_ordered`) and `S` is
causally closed. -/
theorem leaderVal_eq_serial {ctx : Dot → Finset Dot} {S : Finset Dot} {e : ℕ} {k : K}
    (hord : ∀ j j', j < j' → j' < e → Writes (L j) k → Writes (L j') k →
      (L j).stamp ∈ ctx (L j').stamp)
    (hSc : CausallyClosed ctx S) : leaderVal L init S e k = serial L init e k := by
  unfold leaderVal lastNotIn
  rcases hl : lastP (fun j => (L j).stamp ∉ S ∧ Writes (L j) k) e with _ | j
  · have hall : ∀ j < e, Writes (L j) k → (L j).stamp ∈ S := by
      intro j hj hw
      by_contra hS
      exact lastP_none hl j hj ⟨hS, hw⟩
    show readAt L init S e k = serial L init e k
    suffices H : ∀ m, m ≤ e → readAt L init S m k = serial L init m k from H e le_rfl
    intro m hm
    induction m with
    | zero => rfl
    | succ m ih =>
      have ih := ih (by omega)
      cases hw : (L m).wr k with
      | none =>
        have hr : readAt L init S (m + 1) k = readAt L init S m k := by
          simp only [readAt]; split_ifs <;> simp [apply_none hw]
        simp only [hr, serial, apply_none hw, ih]
      | some ov =>
        obtain ⟨o, v⟩ := ov
        have hS := hall m (by omega) ⟨_, hw⟩
        simp [readAt, serial, hS, apply_some hw]
  · obtain ⟨hje, ⟨hjS, hjw⟩, hmax⟩ := lastP_some hl
    show newVal L init j k = serial L init e k
    have hno : ∀ m, j + 1 ≤ m → m < e → ¬ Writes (L m) k := by
      intro m hm hme hw
      by_cases hmS : (L m).stamp ∈ S
      · exact hjS (hSc _ hmS (hord j m (by omega) hme hjw hw))
      · exact hmax m (by omega) hme ⟨hmS, hw⟩
    obtain ⟨⟨o, v⟩, hw⟩ := hjw
    have : serial L init e k = serial L init (j + 1) k := by
      -- truncate the log after `e` so that `serial_noWrite` applies
      suffices H : ∀ b, j + 1 ≤ b → b ≤ e → serial L init b k = serial L init (j + 1) k from
        H e (by omega) le_rfl
      intro b hb hbe
      induction b with
      | zero => omega
      | succ b ih =>
        rcases Nat.eq_or_lt_of_le hb with heq | hlt
        · rw [heq]
        · have hwb : (L b).wr k = none := by
            cases hx : (L b).wr k with
            | none => rfl
            | some ov => exact absurd ⟨ov, hx⟩ (hno b (by omega) (by omega))
          simp only [serial, apply_none hwb]
          exact ih (by omega) (by omega)
    rw [this]
    simp [newVal, serial, apply_some hw, hw]

/-- A bounded additive operation on `k` (`add(a e)`, with the schema bound `lo ≤ · ≤ hi`),
resolved against `cur e`: accepted iff the result stays in bounds. A rejected operation writes
nothing. -/
def Resolved (L : ℕ → Entry K ℤ Dot) (k : K) (a : ℕ → ℤ) (lo hi : ℤ) (cur : ℕ → ℤ) (n : ℕ) :
    Prop :=
  ∀ m < n, (L m).wr k =
    if lo ≤ cur m + a m ∧ cur m + a m ≤ hi then some (cur m, cur m + a m) else none

/-- The net effect of the accepted operations among the first `m` entries. -/
def accSum (a : ℕ → ℤ) (lo hi : ℤ) (cur : ℕ → ℤ) : ℕ → ℤ
  | 0 => 0
  | m + 1 => accSum a lo hi cur m +
      if lo ≤ cur m + a m ∧ cur m + a m ≤ hi then a m else 0

/-- **A bounded operation resolved at its log position never crosses its bound.** The value
at every position is the initial value plus the net effect of the accepted operations (no lost
update), it stays within bounds, and every resolved delta's old value is the true value. -/
theorem bounded_at_position {L : ℕ → Entry K ℤ Dot} {init : K → ℤ} {k : K} {a : ℕ → ℤ}
    {lo hi : ℤ} {n : ℕ} (hinit : lo ≤ init k ∧ init k ≤ hi)
    (hres : Resolved L k a lo hi (fun m => serial L init m k) n) :
    ∀ m ≤ n, serial L init m k = init k + accSum a lo hi (fun m => serial L init m k) m ∧
      lo ≤ serial L init m k ∧ serial L init m k ≤ hi := by
  intro m hm
  induction m with
  | zero => simp [serial, accSum, hinit]
  | succ m ih =>
    obtain ⟨ih1, ih2, ih3⟩ := ih (by omega)
    have hw := hres m (by omega)
    by_cases hacc : lo ≤ serial L init m k + a m ∧ serial L init m k + a m ≤ hi
    · rw [(ite_eq_left_of_eq_true _ _ (eq_true hacc))] at hw
      have : serial L init (m + 1) k = serial L init m k + a m := by
        simp [serial, apply_some hw]
      rw [this]
      refine ⟨?_, hacc.1, hacc.2⟩
      simp only [accSum, hacc, and_self, ↓reduceIte]
      rw [ih1]; ring
    · rw [(ite_eq_right_of_eq_false _ _ (eq_false hacc))] at hw
      have : serial L init (m + 1) k = serial L init m k := by simp [serial, apply_none hw]
      rw [this]
      simp only [accSum, hacc, ↓reduceIte, add_zero]
      exact ⟨ih1, ih2, ih3⟩

/-- **The leader's resolution keeps bounds under pipelining.** If each entry resolves against
the leader's value (the newest write not in its `S`, else the base), writes to `k` are causally
ordered in log order, and each `S` is causally closed, the bound holds at every position. -/
theorem bounded_leader {L : ℕ → Entry K ℤ Dot} {init : K → ℤ} {k : K} {a : ℕ → ℤ}
    {lo hi : ℤ} {n : ℕ} {ctx : Dot → Finset Dot} (hinit : lo ≤ init k ∧ init k ≤ hi)
    (hres : Resolved L k a lo hi (fun m => leaderVal L init (L m).S m k) n)
    (hord : ∀ j j', j < j' → j' < n → Writes (L j) k → Writes (L j') k →
      (L j).stamp ∈ ctx (L j').stamp)
    (hSc : ∀ m < n, CausallyClosed ctx (L m).S) :
    ∀ m ≤ n, lo ≤ serial L init m k ∧ serial L init m k ≤ hi := by
  have heq : ∀ m < n, leaderVal L init (L m).S m k = serial L init m k := fun m hm =>
    leaderVal_eq_serial (fun j j' h1 h2 => hord j j' h1 (by omega)) (hSc m hm)
  have hres' : Resolved L k a lo hi (fun m => serial L init m k) n := by
    intro m hm
    have := hres m hm
    simp only [heq m hm] at this
    exact this
  intro m hm
  exact (bounded_at_position hinit hres' m hm).2

/-! ### Counterexample: resolving against committed state only

A stock of `1` with bound `≥ 0`, and two `subtract(1)` operations. With pipelining, both are
resolved while the commit index is `0`, against the committed value `1`. Both are accepted and
write `1 → 0`. Their net effect is `−2`, which crosses the bound. The log records `0`, losing an
update, and entry `1`'s old value `1` isn't the value `0` at its position. Resolving at the log
position rejects entry `1`. -/

def subLog : ℕ → Entry Unit ℤ ℕ := fun i =>
  { stamp := i, S := ∅, hits := fun _ _ => False, wr := fun _ => some (1, 0) }

theorem committedResolve_crosses_bound :
    -- both resolved against the committed value 1, both accepted
    Resolved subLog () (fun _ => -1) 0 1000 (fun _ => 1) 2 ∧
    -- the net effect crosses the bound
    1 + accSum (fun _ => -1) 0 1000 (fun _ => 1) 2 = -1 ∧
    -- the log records 0: a lost update
    serial subLog (fun _ => 1) 2 () = 0 ∧
    -- entry 1's old value isn't the value at its position
    serial subLog (fun _ => 1) 1 () = 0 ∧ ¬ Consistent subLog (fun _ => 1) 1 ∧
    -- resolved at its log position, entry 1 is rejected
    ¬ (0 ≤ serial subLog (fun _ => 1) 1 () + -1) := by
  have h1 : serial subLog (fun _ => 1) 1 () = 0 := by simp [serial, apply, subLog]
  refine ⟨?_, by simp [accSum], by simp [serial, apply, subLog], h1, ?_, by rw [h1]; omega⟩
  · intro m _; simp [subLog]
  · intro h
    have := h () 1 0 (by simp [subLog])
    rw [h1] at this
    omega

end Bounded

/-! ## Each leader operation equals the evaluator (§8's table)

Restatement-round obligation 8 (§18). For each form in §8's table, the ShepQL evaluator
computes the new field value from the form, at the base value `b`, which may be `{}` (unset);
the leader computes it from the op record. We prove they agree for every base value, including
`{}`, whenever `e` is One (a single value).

* **The evaluator.** A field's value is a set: `b.toList` for a single field (`none` is `{}`).
  `+`, `-`, `++`, and the array constructor `[v]` are element-wise (product over their
  arguments), `??` takes its left side unless it is `{}`, and `max` over a set is `{}` on `{}`.
  Assigning a set to a single field (`assign`) unsets it on `{}`, sets it on one value, and is
  an error on more. `tags += V` and `tags -= V` on a `multi` property are set union and
  difference (`multi` properties are sets, §4.0 item 7).
* **The leader.** `add n`, `sub n`: `(b ?? 0) ± n`; `max v`: `max({b, v})`, `v` when `b` is
  unset; `insert v`, `remove v`: set insert and erase; `push x`, `extend xs`: `(b ?? []) ++ …`.

Results: `addReq_eq`, `subReq_eq`, `addOpt_eq`, `subOpt_eq`, `max_eq`, `insert_eq`,
`remove_eq`, `pushReq_eq`, `pushOpt_eq`, `extendReq_eq`, `extendOpt_eq`, and
`resolved_eq_eval_at_logpos` (with `leaderVal_eq_serial`: the leader's resolved write is the
evaluator's result at the entry's log position). For `for` loops: `add_add`, `sub_add`,
`max_max`, `push_push`, `insert_seq`. Boundaries:

* `addReq_unset`: `.x + e` on an unset `x` unsets it, which no `add n` does: why the optional
  forms need `?? 0` (the design's own argument).
* `optionalArg_unsets`, `max_optionalArg_unset` (**doc finding**): with `e` optional and absent,
  `(.x ?? 0) + e` unsets the field and `max({.x, e})` keeps an unset field unset; no op record
  produces either. §8's "when it qualifies" should require `e` to be One.
* `add_max_not_comm` (**doc finding**): `add` and `max` on one field don't commute, so "in a
  `for`, operations on one field sum" needs a rule for mixed kinds (apply in statement order,
  or reject the mix).
-/

namespace FieldOps

/-- Element-wise application over the product. -/
def prod {α β γ : Type*} (f : α → β → γ) (xs : List α) (ys : List β) : List γ :=
  xs.flatMap (fun x => ys.map (f x))

/-- `a ?? b` on sets. -/
def coalesceL {α : Type*} (xs ys : List α) : List α := if xs = [] then ys else xs

/-- EdgeQL's `max` over a set: `{}` on `{}`. -/
def maxL (xs : List ℤ) : List ℤ := match xs.max? with
  | none => []
  | some m => [m]

/-- Assigning a set to a single field: `{}` unsets, one value sets, more is an error (`none`). -/
def assign {α : Type*} : List α → Option (Option α)
  | [] => some none
  | [v] => some (some v)
  | _ :: _ :: _ => none

/-! ### Counters -/

/-- `x := .x + e` (`x` required). -/
def evalAddReq (b : Option ℤ) (es : List ℤ) : Option (Option ℤ) :=
  assign (prod (· + ·) b.toList es)

/-- `x := .x - e` (`x` required). -/
def evalSubReq (b : Option ℤ) (es : List ℤ) : Option (Option ℤ) :=
  assign (prod (· - ·) b.toList es)

/-- `x := (.x ?? 0) + e`. -/
def evalAddOpt (b : Option ℤ) (es : List ℤ) : Option (Option ℤ) :=
  assign (prod (· + ·) (coalesceL b.toList [0]) es)

/-- `x := (.x ?? 0) - e`. -/
def evalSubOpt (b : Option ℤ) (es : List ℤ) : Option (Option ℤ) :=
  assign (prod (· - ·) (coalesceL b.toList [0]) es)

/-- `x := max({.x, e})`. -/
def evalMax (b : Option ℤ) (es : List ℤ) : Option (Option ℤ) := assign (maxL (b.toList ++ es))

/-- The leader's `add n`: `(b ?? 0) + n`. -/
def opAdd (n : ℤ) (b : Option ℤ) : Option ℤ := some (b.getD 0 + n)

/-- The leader's `sub n`: `(b ?? 0) - n`. -/
def opSub (n : ℤ) (b : Option ℤ) : Option ℤ := some (b.getD 0 - n)

/-- The leader's `max v`: `v` when `b` is unset. -/
def opMax (v : ℤ) : Option ℤ → Option ℤ
  | none => some v
  | some b => some (max b v)

theorem addReq_eq (b n : ℤ) : evalAddReq (some b) [n] = some (opAdd n (some b)) := rfl

theorem subReq_eq (b n : ℤ) : evalSubReq (some b) [n] = some (opSub n (some b)) := rfl

theorem addOpt_eq (b : Option ℤ) (n : ℤ) : evalAddOpt b [n] = some (opAdd n b) := by
  cases b <;> rfl

theorem subOpt_eq (b : Option ℤ) (n : ℤ) : evalSubOpt b [n] = some (opSub n b) := by
  cases b <;> rfl

theorem max_eq (b : Option ℤ) (v : ℤ) : evalMax b [v] = some (opMax v b) := by
  cases b with
  | none => rfl
  | some b =>
    simp only [evalMax, Option.toList_some, List.cons_append, List.nil_append, maxL, opMax]
    rw [List.max?_cons]
    simp [assign]

/-- **`.x + e` on an unset `x` unsets it**, which no `add n` does. -/
theorem addReq_unset (n : ℤ) : evalAddReq none [n] = some none ∧ ∀ m, opAdd m none ≠ none :=
  ⟨rfl, fun _ h => by simp [opAdd] at h⟩

/-- **With `e` optional and absent**, `(.x ?? 0) + e` and `(.x ?? 0) - e` unset the field, which
no op record does. -/
theorem optionalArg_unsets (b : Option ℤ) :
    evalAddOpt b [] = some none ∧ evalSubOpt b [] = some none ∧
      ∀ m, opAdd m b ≠ none ∧ opSub m b ≠ none := by
  refine ⟨?_, ?_, fun m => ⟨by simp [opAdd], by simp [opSub]⟩⟩ <;>
    cases b <;> rfl

/-- With `e` absent, `max({.x, e})` keeps an unset field unset, which no `max v` does. -/
theorem max_optionalArg_unset : evalMax none [] = some none ∧ ∀ v, opMax v none ≠ none :=
  ⟨rfl, fun _ h => by simp [opMax] at h⟩

/-! ### Sets (`multi` properties) -/

section Sets
variable {α : Type*} [DecidableEq α]

/-- `tags += V`: union. -/
def evalInsert (b : Finset α) (vs : List α) : Finset α := b ∪ vs.toFinset

/-- `tags -= V`: difference. -/
def evalRemove (b : Finset α) (vs : List α) : Finset α := b \ vs.toFinset

def opInsert (v : α) (b : Finset α) : Finset α := insert v b
def opRemove (v : α) (b : Finset α) : Finset α := b.erase v

theorem insert_eq (b : Finset α) (v : α) : evalInsert b [v] = opInsert v b := by
  ext x; simp [evalInsert, opInsert]

theorem remove_eq (b : Finset α) (v : α) : evalRemove b [v] = opRemove v b := by
  ext x; simp [evalRemove, opRemove, and_comm]

/-- `tags += V` with several values is the inserts in order (`{}` adds nothing). -/
theorem insert_seq (b : Finset α) (vs : List α) :
    evalInsert b vs = vs.foldl (fun s v => opInsert v s) b := by
  induction vs generalizing b with
  | nil => simp [evalInsert]
  | cons v vs ih =>
    rw [List.foldl_cons, ← ih]
    ext x; simp [evalInsert, opInsert]

end Sets

/-! ### Arrays -/

section Arrays
variable {α : Type*}

/-- The array constructor `[v]`, element-wise in `v`. -/
def arr1 (es : List α) : List (List α) := es.map (fun v => [v])

/-- `arr := .arr ++ [v]` (`arr` required). -/
def evalPushReq (b : Option (List α)) (es : List α) : Option (Option (List α)) :=
  assign (prod (· ++ ·) b.toList (arr1 es))

/-- `arr := (.arr ?? []) ++ [v]`. -/
def evalPushOpt (b : Option (List α)) (es : List α) : Option (Option (List α)) :=
  assign (prod (· ++ ·) (coalesceL b.toList [[]]) (arr1 es))

/-- `arr := .arr ++ e` (`arr` required). -/
def evalExtendReq (b : Option (List α)) (xss : List (List α)) : Option (Option (List α)) :=
  assign (prod (· ++ ·) b.toList xss)

/-- `arr := (.arr ?? []) ++ e`. -/
def evalExtendOpt (b : Option (List α)) (xss : List (List α)) : Option (Option (List α)) :=
  assign (prod (· ++ ·) (coalesceL b.toList [[]]) xss)

def opPush (x : α) (b : Option (List α)) : Option (List α) := some (b.getD [] ++ [x])
def opExtend (xs : List α) (b : Option (List α)) : Option (List α) := some (b.getD [] ++ xs)

theorem pushReq_eq (b : List α) (x : α) : evalPushReq (some b) [x] = some (opPush x (some b)) :=
  rfl

theorem pushOpt_eq (b : Option (List α)) (x : α) : evalPushOpt b [x] = some (opPush x b) := by
  cases b <;> rfl

theorem extendReq_eq (b xs : List α) :
    evalExtendReq (some b) [xs] = some (opExtend xs (some b)) := rfl

theorem extendOpt_eq (b : Option (List α)) (xs : List α) :
    evalExtendOpt b [xs] = some (opExtend xs b) := by
  cases b <;> rfl

theorem push_push (x y : α) (b : Option (List α)) :
    opPush y (opPush x b) = opExtend [x, y] b := by
  simp [opPush, opExtend]

end Arrays

/-! ### Operations on one field in a `for` -/

theorem add_add (n m : ℤ) (b : Option ℤ) : opAdd m (opAdd n b) = opAdd (n + m) b := by
  simp [opAdd, add_assoc]

theorem sub_add (n m : ℤ) (b : Option ℤ) : opSub m (opAdd n b) = opAdd (n - m) b := by
  simp [opAdd, opSub]; ring

theorem max_max (v w : ℤ) (b : Option ℤ) : opMax w (opMax v b) = opMax (max v w) b := by
  cases b <;> simp [opMax, max_assoc]

/-- **`add` and `max` don't commute.** -/
theorem add_max_not_comm : opMax 3 (opAdd 5 (some 0)) ≠ opAdd 5 (opMax 3 (some 0)) := by
  decide

/-! ### At the log position -/

section AtLogPos

open Validation

variable {K Val Dot : Type*} {L : ℕ → Entry K Val Dot} {init : K → Val}

/-- **The leader's resolved write is the evaluator's result at the entry's log position**, for
any op whose result equals the evaluator's form at every base value (the theorems above). -/
theorem resolved_eq_eval_at_logpos {ctx : Dot → Finset Dot} {S : Finset Dot} {e : ℕ} {k : K}
    (op : Val → Val) (form : Val → Option Val) (hof : ∀ b, form b = some (op b))
    (hord : ∀ j j', j < j' → j' < e → Writes (L j) k → Writes (L j') k →
      (L j).stamp ∈ ctx (L j').stamp)
    (hSc : CausallyClosed ctx S) :
    form (serial L init e k) = some (op (leaderVal L init S e k)) := by
  rw [leaderVal_eq_serial hord hSc, hof]

/-- For example, `x := (.x ?? 0) + n`. -/
theorem addOpt_at_logpos {L : ℕ → Entry K (Option ℤ) Dot} {init : K → Option ℤ}
    {ctx : Dot → Finset Dot} {S : Finset Dot} {e : ℕ} {k : K} (n : ℤ)
    (hord : ∀ j j', j < j' → j' < e → Writes (L j) k → Writes (L j') k →
      (L j).stamp ∈ ctx (L j').stamp)
    (hSc : CausallyClosed ctx S) :
    evalAddOpt (serial L init e k) [n] = some (opAdd n (leaderVal L init S e k)) :=
  resolved_eq_eval_at_logpos (opAdd n) (fun b => evalAddOpt b [n]) (fun b => addOpt_eq b n)
    hord hSc

end AtLogPos

end FieldOps

end LeaderOps
end Shep
