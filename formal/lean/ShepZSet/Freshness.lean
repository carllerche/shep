/-
# Freshness checks: "has this query's answer changed since version V?"

The design under test:

* Every stored item (a field value, an edge, a node lifecycle fact, an index entry) carries
  the dot of the mutation that last changed it. A removed index entry or edge stays behind as
  a **ghost** stamped with the removing dot. Keys are never reused, and nothing is physically
  deleted.
* A version `V` is a set of dots (causally closed where it matters, see compaction below).
* A query is evaluated over a state and an actor, with a deterministic read policy that reads
  only graph facts. Evaluating it **visits** a set of items: the index entries of the scanned
  range *including ghosts*, the facts of *every candidate row* (including rows the residual
  filter or the policy drops), and each candidate's policy inputs; plus the policy and schema
  definitions of the type read.
* The check says "unchanged" iff every visited item's stamp is in `V`.

## The model

* `History`: a finite-or-not set of dots, each with a causal context `ctx` and a partial write
  `write d k : Option Val` (`none` = "doesn't touch `k`"). The state at a set of dots `W`
  gives each key the value written by its **last writer in `W`** (`stamp W k`), or the initial
  value. Writes to the same key are causally ordered (`History.WF.ordered`): this is
  condition 3 of `wf_stateAt_closed`, which Postgres row conflicts and the unique
  `(edge type, src, dst)` key provide, together with "keys are never reused".
* Ghosts need no special case: removing an index entry is a write of the "absent" value
  (`none : Option V`) to that entry's key, and the key keeps that writer as its stamp.
* `Check stamp V visit`: every visited key's stamp is in `V` (an unstamped, never-written key
  passes; a physical scan doesn't see it at all).

## Results

* `valAt_eq_of_stamp`: a key whose stamp at the later history `H ⊇ V` is in `V` has the same
  value at `V` and at `H`.
* `fresh_sound`: the abstract theorem. If the query's visited set is a **sound read set**
  (`ReadSetSound`: two states that agree on the items visited in the first give the same
  answer) and the check passes at `H`, then `Q(state at V) = Q(state at H)`.
* `Scan.eval_readSetSound`: the concrete query model (index-range scan, residual filter,
  projection, read policy) has a sound read set, given that the policy reads only its declared
  inputs (`Scan.PolicyLocal`, the planner's guarantee).
* `freshness_sound`: **the headline theorem** for the concrete model.
* `compaction_refines`, `compaction_sound`: compacting stamps soundly (each replaced dot is
  causally at or below a dot of the replacement summary, and `V` is causally closed) only
  turns "unchanged" into "changed".
* Counterexamples: `returnedOnly_unsound` (checking only returned rows),
  `noGhost_unsound` (checking only live index entries), and
  `counterMax_compaction_unsound` (compacting a bucket's ghosts to the dot with the largest
  counter, which is not a causal maximum when dots are concurrent).
-/
import ShepZSet.Integrate
import Mathlib.Data.Fintype.Basic
import Mathlib.Data.Finset.Prod

noncomputable section
open Classical

namespace Shep
namespace Fresh

/-! ## Histories of stamped keys -/

/-- A history: each dot has a causal context and writes some keys. `init` is the value of a
key nothing has written. -/
structure History (K Val Dot : Type*) where
  ctx : Dot → Finset Dot
  write : Dot → K → Option Val
  init : K → Val

variable {K Val Dot : Type*}

namespace History

variable (h : History K Val Dot)

/-- Dot `d` writes key `k`. -/
def Writes (d : Dot) (k : K) : Prop := (h.write d k).isSome = true

/-- Well-formed histories: causality is a strict order, and writes to the same key are
causally ordered (row conflicts, unique edge keys, keys never reused). -/
structure WF : Prop where
  irrefl : ∀ d, d ∉ h.ctx d
  closed : ∀ d, CausallyClosed h.ctx (h.ctx d)
  ordered : ∀ d e k, d ≠ e → h.Writes d k → h.Writes e k → d ∈ h.ctx e ∨ e ∈ h.ctx d

/-- `d` is the last writer of `k` among the dots of `W`. -/
def IsLast (W : Finset Dot) (k : K) (d : Dot) : Prop :=
  d ∈ W ∧ h.Writes d k ∧ ∀ e ∈ W, h.Writes e k → e = d ∨ e ∈ h.ctx d

/-- The stamp of `k` at `W`: its last writer in `W`, if any. -/
def stamp (W : Finset Dot) (k : K) : Option Dot :=
  if hx : ∃ d, h.IsLast W k d then some hx.choose else none

/-- The state at `W`: each key holds the value its last writer wrote. -/
def valAt (W : Finset Dot) (k : K) : Val :=
  match h.stamp W k with
  | some d => (h.write d k).getD (h.init k)
  | none => h.init k

variable {h}

theorem IsLast.unique (hw : h.WF) {W : Finset Dot} {k : K} {d d' : Dot}
    (h₁ : h.IsLast W k d) (h₂ : h.IsLast W k d') : d = d' := by
  by_contra hne
  rcases h₁.2.2 d' h₂.1 h₂.2.1 with h | h
  · exact hne h.symm
  rcases h₂.2.2 d h₁.1 h₁.2.1 with h' | h'
  · exact hne h'
  exact hw.irrefl d' (hw.closed d' d h' h)

/-- Some dot of `W` writes `k`, so `k` has a last writer in `W`. -/
theorem exists_isLast (hw : h.WF) {W : Finset Dot} {k : K}
    (hex : ∃ d ∈ W, h.Writes d k) : ∃ d, h.IsLast W k d := by
  set D := W.filter (fun d => h.Writes d k) with hD
  have hne : D.Nonempty := by
    obtain ⟨d, hd, hwd⟩ := hex; exact ⟨d, Finset.mem_filter.mpr ⟨hd, hwd⟩⟩
  obtain ⟨m, hmD, hmax⟩ := Finset.exists_max_image D (fun d => (h.ctx d).card) hne
  obtain ⟨hmW, hmk⟩ := Finset.mem_filter.mp hmD
  refine ⟨m, hmW, hmk, fun e he hek => ?_⟩
  by_cases hem : e = m
  · exact Or.inl hem
  right
  rcases hw.ordered e m k hem hek hmk with h1 | h1
  · exact h1
  · exfalso
    have hsub : h.ctx m ⊆ h.ctx e := hw.closed e m h1
    have hss : h.ctx m ⊂ h.ctx e :=
      Finset.ssubset_iff_subset_ne.mpr ⟨hsub, fun heq => hw.irrefl m (by rw [heq]; exact h1)⟩
    have := Finset.card_lt_card hss
    have := hmax e (Finset.mem_filter.mpr ⟨he, hek⟩)
    omega

theorem stamp_eq_some (hw : h.WF) {W : Finset Dot} {k : K} {d : Dot}
    (hl : h.IsLast W k d) : h.stamp W k = some d := by
  have hx : ∃ d, h.IsLast W k d := ⟨d, hl⟩
  simp only [stamp, hx, ↓reduceDIte]
  exact congrArg some (IsLast.unique hw hx.choose_spec hl)

theorem isLast_of_stamp {W : Finset Dot} {k : K} {d : Dot}
    (hs : h.stamp W k = some d) : h.IsLast W k d := by
  unfold stamp at hs
  split_ifs at hs with hx
  rw [← Option.some_inj.mp hs]
  exact hx.choose_spec

theorem stamp_eq_none {W : Finset Dot} {k : K}
    (hno : ∀ d ∈ W, ¬ h.Writes d k) : h.stamp W k = none := by
  unfold stamp
  split_ifs with hx
  · obtain ⟨d, hd, hwd, -⟩ := hx
    exact absurd hwd (hno d hd)
  · rfl

theorem not_writes_of_stamp_none (hw : h.WF) {W : Finset Dot} {k : K}
    (hs : h.stamp W k = none) : ∀ d ∈ W, ¬ h.Writes d k := by
  intro d hd hwd
  obtain ⟨m, hm⟩ := exists_isLast hw ⟨d, hd, hwd⟩
  rw [stamp_eq_some hw hm] at hs
  cases hs

theorem valAt_of_isLast (hw : h.WF) {W : Finset Dot} {k : K} {d : Dot}
    (hl : h.IsLast W k d) : h.valAt W k = (h.write d k).getD (h.init k) := by
  simp [valAt, stamp_eq_some hw hl]

theorem valAt_of_none {W : Finset Dot} {k : K}
    (hno : ∀ d ∈ W, ¬ h.Writes d k) : h.valAt W k = h.init k := by
  simp [valAt, stamp_eq_none hno]

/-- **Stamps certify values.** If `V ⊆ H` and the stamp of `k` at `H` is in `V` (or `k` has
no stamp), then `k` has the same value at `V` as at `H`. Causal closure of `V` isn't needed. -/
theorem valAt_eq_of_stamp (hw : h.WF) {V H : Finset Dot} (hVH : V ⊆ H) {k : K}
    (hk : ∀ d, h.stamp H k = some d → d ∈ V) : h.valAt V k = h.valAt H k := by
  cases hs : h.stamp H k with
  | none =>
    have hno := not_writes_of_stamp_none hw hs
    rw [valAt_of_none (fun d hd => hno d (hVH hd)), valAt_of_none hno]
  | some d =>
    have hl := isLast_of_stamp hs
    have hlV : h.IsLast V k d :=
      ⟨hk d hs, hl.2.1, fun e he hek => hl.2.2 e (hVH he) hek⟩
    rw [valAt_of_isLast hw hlV, valAt_of_isLast hw hl]

end History

/-! ## The check and the abstract soundness theorem -/

/-- The freshness check: every visited key's stamp is in `V`. -/
def Check (stamp : K → Option Dot) (V : Finset Dot) (visit : Finset K) : Prop :=
  ∀ k ∈ visit, ∀ d, stamp k = some d → d ∈ V

/-- **The planner's guarantee.** `visit σ` is a sound read set for `Q` at `σ`: any state
that agrees with `σ` on the items visited when evaluating at `σ` gives the same answer. This
is what "the evaluation visits these items" has to mean for a check to be sound; it is a
hypothesis about the query, discharged below for the concrete scan model. -/
def ReadSetSound {β : Type*} (Q : (K → Val) → β) (visit : (K → Val) → Finset K) : Prop :=
  ∀ σ τ : K → Val, (∀ k ∈ visit σ, σ k = τ k) → Q σ = Q τ

/-- **Freshness soundness, abstract form.** Let `H` be a history that extends `V` (`V ⊆ H`).
If the query's visited set is a sound read set, and the check run at `H` (on the items
visited when evaluating at `H`, with `H`'s stamps) says "unchanged", then the query has the
same answer at `V` as at `H`. -/
theorem fresh_sound {β : Type*} {h : History K Val Dot} (hw : h.WF) {V H : Finset Dot}
    (hVH : V ⊆ H) {Q : (K → Val) → β} {visit : (K → Val) → Finset K}
    (hQ : ReadSetSound Q visit) (hc : Check (h.stamp H) V (visit (h.valAt H))) :
    Q (h.valAt V) = Q (h.valAt H) :=
  (hQ _ _ (fun k hk => (History.valAt_eq_of_stamp hw hVH (hc k hk)).symm)).symm

/-! ## The concrete query model: index-range scan, residual filter, projection, policy -/

/-- Stored items. `entry b r` is row `r`'s entry in index bucket `b` (a scan range is a finite
set of buckets); `fact r f` is a fact of row `r` (a field, an edge out of `r`, a lifecycle
fact); `policy t` and `schema t` are the deployed read policy and schema of type `t`. Every
item's value is `Option V`: `none` is absent, which for an entry or an edge that once existed
means a ghost. -/
inductive Item (Ty B R F : Type*)
  | entry (b : B) (r : R)
  | fact (r : R) (f : F)
  | policy (t : Ty)
  | schema (t : Ty)

variable {Ty B R F V A Out : Type*}

/-- A state, as readers see it. -/
abbrev St (Ty B R F V : Type*) := Item Ty B R F → Option V

/-- Row `r`'s facts at the fields `fs`, and nothing else. -/
def restrict (σ : St Ty B R F V) (r : R) (fs : Finset F) : F → Option V :=
  fun f => if f ∈ fs then σ (.fact r f) else none

/-- A query: scan the index buckets in `range` of type `ty`, keep candidates passing the
residual filter (which reads the schema and the row's `filtFields`) and the read policy, and
return each kept row's projection (which reads the schema and the row's `projFields`). The
policy may read anything (it follows edges, delegates, reads other types' policies), and
`polReads` is its read set, which may itself depend on what it reads. -/
structure Scan (Ty B R F V A Out : Type*) where
  ty : Ty
  range : Finset B
  filtFields : Finset F
  projFields : Finset F
  filt : Option V → (F → Option V) → Bool
  shape : Option V → (F → Option V) → Out
  pol : Option V → A → R → St Ty B R F V → Bool
  polReads : Option V → A → R → St Ty B R F V → Finset (Item Ty B R F)

namespace Scan

variable [Fintype R] (q : Scan Ty B R F V A Out)

/-- Candidates: rows with a live entry in a scanned bucket. -/
def cand (σ : St Ty B R F V) : Finset R :=
  Finset.univ.filter (fun r => ∃ b ∈ q.range, σ (.entry b r) ≠ none)

/-- Row `r` is returned to actor `a`. -/
def Returned (a : A) (σ : St Ty B R F V) (r : R) : Prop :=
  r ∈ q.cand σ ∧ q.filt (σ (.schema q.ty)) (restrict σ r q.filtFields) = true ∧
    q.pol (σ (.policy q.ty)) a r σ = true

/-- The answer: for each row, its projection if it is returned. -/
def eval (a : A) (σ : St Ty B R F V) : R → Option Out := fun r =>
  if q.Returned a σ r then some (q.shape (σ (.schema q.ty)) (restrict σ r q.projFields))
  else none

/-- Every entry in the scanned buckets, live or ghost. -/
def entries : Finset (Item Ty B R F) :=
  (q.range ×ˢ Finset.univ).image (fun x => Item.entry x.1 x.2)

/-- What a candidate row contributes: its filtered and projected facts, and its policy inputs. -/
def rowItems (a : A) (σ : St Ty B R F V) (r : R) : Finset (Item Ty B R F) :=
  (q.filtFields ∪ q.projFields).image (Item.fact r) ∪ q.polReads (σ (.policy q.ty)) a r σ

/-- **The visited set**: the type's policy and schema, every entry of the range including
ghosts, and for every candidate (returned or not) its facts and policy inputs. -/
def visit (a : A) (σ : St Ty B R F V) : Finset (Item Ty B R F) :=
  {.policy q.ty, .schema q.ty} ∪ q.entries ∪ (q.cand σ).biUnion (q.rowItems a σ)

/-- **The planner's guarantee for policies**: the policy (deterministic, reading only graph
facts) depends only on its declared read set. -/
def PolicyLocal : Prop :=
  ∀ p a r (σ τ : St Ty B R F V), (∀ k ∈ q.polReads p a r σ, σ k = τ k) →
    q.pol p a r σ = q.pol p a r τ

theorem mem_entries {b : B} (hb : b ∈ q.range) (r : R) : Item.entry b r ∈ q.entries := by
  simp only [entries, Finset.mem_image, Finset.mem_product, Finset.mem_univ, and_true]
  exact ⟨(b, r), hb, rfl⟩

/-- **The concrete query model has a sound read set.** -/
theorem eval_readSetSound (hp : q.PolicyLocal) (a : A) :
    ReadSetSound (q.eval a) (q.visit a) := by
  intro σ τ hag
  have hpol : σ (.policy q.ty) = τ (.policy q.ty) := hag _ (by simp [visit])
  have hsch : σ (.schema q.ty) = τ (.schema q.ty) := hag _ (by simp [visit])
  have hent : ∀ b ∈ q.range, ∀ r, σ (.entry b r) = τ (.entry b r) := fun b hb r =>
    hag _ (Finset.mem_union_left _ (Finset.mem_union_right _ (q.mem_entries hb r)))
  have hcand : q.cand σ = q.cand τ := by
    ext r
    simp only [cand, Finset.mem_filter, Finset.mem_univ, true_and]
    constructor
    · rintro ⟨b, hb, hne⟩; exact ⟨b, hb, by rwa [← hent b hb r]⟩
    · rintro ⟨b, hb, hne⟩; exact ⟨b, hb, by rwa [hent b hb r]⟩
  have hrow : ∀ r ∈ q.cand σ, ∀ k ∈ q.rowItems a σ r, σ k = τ k := fun r hr k hk =>
    hag k (Finset.mem_union_right _ (Finset.mem_biUnion.mpr ⟨r, hr, hk⟩))
  have hres : ∀ r ∈ q.cand σ, ∀ fs, fs ⊆ q.filtFields ∪ q.projFields →
      restrict σ r fs = restrict τ r fs := by
    intro r hr fs hfs
    funext f
    simp only [restrict]
    split_ifs with hf
    · exact hrow r hr _ (Finset.mem_union_left _ (Finset.mem_image_of_mem _ (hfs hf)))
    · rfl
  funext r
  by_cases hr : r ∈ q.cand σ
  · have hf := hres r hr q.filtFields Finset.subset_union_left
    have hpj := hres r hr q.projFields Finset.subset_union_right
    have hp' : q.pol (σ (.policy q.ty)) a r σ = q.pol (τ (.policy q.ty)) a r τ := by
      rw [← hpol]
      exact hp _ a r σ τ (fun k hk => hrow r hr k (Finset.mem_union_right _ hk))
    have hRet : q.Returned a σ r ↔ q.Returned a τ r := by
      simp only [Returned, hcand, hf, hsch, hp']
    simp only [eval, hRet, hsch, hpj]
  · have hr' : r ∉ q.cand τ := hcand ▸ hr
    have h1 : ¬ q.Returned a σ r := fun h => hr h.1
    have h2 : ¬ q.Returned a τ r := fun h => hr' h.1
    simp only [eval, h1, h2, ite_false]

end Scan

/-- **Freshness soundness (headline).** Let `H ⊇ V` be the history of the later state `S`.
Evaluate the scan query `q` for actor `a` at `S`, and check that every visited item — the
type's policy and schema, every index entry in the scanned range including ghosts, and the
facts and policy inputs of every candidate row, returned or not — carries a stamp in `V`. If
the check says "unchanged", then `q` gives the same answer at `V` as at `S`. The one
hypothesis about the query is `PolicyLocal`: the policy reads only its declared inputs. -/
theorem freshness_sound [Fintype R] {h : History (Item Ty B R F) (Option V) Dot} (hw : h.WF)
    {Vv H : Finset Dot} (hVH : Vv ⊆ H) (q : Scan Ty B R F V A Out) (hp : q.PolicyLocal)
    (a : A) (hc : Check (h.stamp H) Vv (q.visit a (h.valAt H))) :
    q.eval a (h.valAt Vv) = q.eval a (h.valAt H) :=
  fresh_sound hw hVH (q.eval_readSetSound hp a) hc

/-! ## Compaction only turns "unchanged" into "changed" -/

/-- A compaction of stamps: each key `k` is now checked through the stored key `c k` (many
ghost entries of a bucket may share one marker key), and each stored key carries a summary,
a set of dots. The compaction **covers** the original stamps if every original stamp is
causally at or below some dot of its summary: compaction only moves dots later. The check
then requires the whole summary to be in `V`. Leaving a key alone is `c k = k` with summary
`{stamp k}`. -/
def Covers {K' : Type*} (ctx : Dot → Finset Dot) (stamp : K → Option Dot) (c : K → K')
    (summ : K' → Finset Dot) : Prop :=
  ∀ k d, stamp k = some d → ∃ d' ∈ summ (c k), d = d' ∨ d ∈ ctx d'

/-- The check against compacted stamps. -/
def CheckC {K' : Type*} (summ : K' → Finset Dot) (c : K → K') (V : Finset Dot)
    (visit : Finset K) : Prop :=
  ∀ k ∈ visit, summ (c k) ⊆ V

/-- **Compaction can only make the check more conservative.** If `V` is causally closed and
the compaction covers the original stamps, then whenever the compacted check says
"unchanged", so does the original check. Equivalently: compaction only turns "unchanged" into
"changed". -/
theorem compaction_refines {K' : Type*} {ctx : Dot → Finset Dot} {V : Finset Dot}
    (hV : CausallyClosed ctx V) {stamp : K → Option Dot} {c : K → K'}
    {summ : K' → Finset Dot} (hcov : Covers ctx stamp c summ) {visit : Finset K}
    (hc : CheckC summ c V visit) : Check stamp V visit := by
  intro k hk d hd
  obtain ⟨d', hd', hle⟩ := hcov k d hd
  have hd'V := hc k hk hd'
  rcases hle with rfl | hlt
  · exact hd'V
  · exact hV d' hd'V hlt

/-- Not compacting at all is a covering compaction whose check is the original check. -/
theorem covers_id (ctx : Dot → Finset Dot) (stamp : K → Option Dot) :
    Covers ctx stamp id (fun k => (stamp k).toFinset) :=
  fun k d hd => ⟨d, by simp [hd], Or.inl rfl⟩

theorem checkC_id_iff (stamp : K → Option Dot) (V : Finset Dot) (visit : Finset K) :
    CheckC (fun k => (stamp k).toFinset) id V visit ↔ Check stamp V visit := by
  constructor
  · intro h k hk d hd; exact h k hk (by simp [hd])
  · intro h k hk d hd
    cases hs : stamp k with
    | none => simp [hs] at hd
    | some e => simp [hs] at hd; subst hd; exact h k hk _ hs

/-- **Freshness soundness under compaction.** With a covering compaction and a causally
closed `V`, a compacted check that says "unchanged" is still sound. -/
theorem compaction_sound {β K' : Type*} {h : History K Val Dot} (hw : h.WF)
    {V H : Finset Dot} (hVH : V ⊆ H) (hV : CausallyClosed h.ctx V)
    {Q : (K → Val) → β} {visit : (K → Val) → Finset K} (hQ : ReadSetSound Q visit)
    {c : K → K'} {summ : K' → Finset Dot} (hcov : Covers h.ctx (h.stamp H) c summ)
    (hc : CheckC summ c V (visit (h.valAt H))) :
    Q (h.valAt V) = Q (h.valAt H) :=
  fresh_sound hw hVH hQ (compaction_refines hV hcov hc)

/-! ## Counterexamples

All three use a causally closed `V` and a well-formed history, so the failure is the check's
alone. The items are those of one type, one bucket, and either one row or two. -/

/-- A history whose dots are totally ordered (`ctx d = {0, …, d-1}`). -/
def linHist {K Val : Type*} (write : ℕ → K → Option Val) (init : K → Val) :
    History K Val ℕ :=
  ⟨fun d => Finset.range d, write, init⟩

theorem linHist_wf {K Val : Type*} (write : ℕ → K → Option Val) (init : K → Val) :
    (linHist write init).WF where
  irrefl d := by simp [linHist]
  closed d := by
    intro e he x hx
    simp only [linHist, Finset.mem_range] at he hx ⊢
    omega
  ordered d e k hne _ _ := by
    simp only [linHist, Finset.mem_range]
    omega

/-- One row, one field, one bucket. The filter keeps the row iff its field is `1`. The policy
allows everything and reads nothing. -/
def cexScan : Scan Unit Unit Unit Unit ℕ Unit Unit where
  ty := ()
  range := {()}
  filtFields := {()}
  projFields := ∅
  filt := fun _ g => decide (g () = some 1)
  shape := fun _ _ => ()
  pol := fun _ _ _ _ => true
  polReads := fun _ _ _ _ => ∅

theorem cexScan_policyLocal : cexScan.PolicyLocal := fun _ _ _ _ _ _ => rfl

/-- The check that visits only the rows it returns (plus the policy and schema). -/
def Scan.visitReturned [Fintype R] (q : Scan Ty B R F V A Out) (a : A) (σ : St Ty B R F V) :
    Finset (Item Ty B R F) :=
  {.policy q.ty, .schema q.ty} ∪
    ((q.cand σ).filter (q.Returned a σ)).biUnion
      (fun r => q.range.image (fun b => Item.entry b r) ∪ q.rowItems a σ r)

/-- The check that skips ghosts: it visits only the live entries of the range. -/
def Scan.visitLive [Fintype R] (q : Scan Ty B R F V A Out) (a : A) (σ : St Ty B R F V) :
    Finset (Item Ty B R F) :=
  {.policy q.ty, .schema q.ty} ∪ q.entries.filter (fun k => σ k ≠ none) ∪
    (q.cand σ).biUnion (q.rowItems a σ)

/-- Dot 0 creates the row with field `1`; dot 1 changes the field to `2`. -/
def cexWrite1 : ℕ → Item Unit Unit Unit Unit → Option (Option ℕ)
  | 0, .entry _ _ => some (some 0)
  | 0, .fact _ _ => some (some 1)
  | 1, .fact _ _ => some (some 2)
  | _, _ => none

/-- Dot 0 creates the row with field `1`; dot 1 removes its index entry (leaving a ghost). -/
def cexWrite2 : ℕ → Item Unit Unit Unit Unit → Option (Option ℕ)
  | 0, .entry _ _ => some (some 0)
  | 0, .fact _ _ => some (some 1)
  | 1, .entry _ _ => some none
  | _, _ => none

/-- **Checking only returned rows is unsound.** At `V = {0}` the row passes the filter and is
returned. Dot 1 changes the filtered field, so at `H = {0, 1}` the row is dropped. A check
that visits only returned rows visits nothing stamped, says "unchanged", and is wrong. -/
theorem returnedOnly_unsound :
    let h := linHist cexWrite1 (fun _ => none)
    h.WF ∧ ({0} : Finset ℕ) ⊆ {0, 1} ∧ CausallyClosed h.ctx {0} ∧
      cexScan.PolicyLocal ∧
      Check (h.stamp {0, 1}) {0} (cexScan.visitReturned () (h.valAt {0, 1})) ∧
      cexScan.eval () (h.valAt {0}) ≠ cexScan.eval () (h.valAt {0, 1}) := by
  intro h
  have hw : h.WF := linHist_wf _ _
  -- values at `{0}` and `{0, 1}`
  have eF0 : h.valAt {0} (.fact () ()) = some 1 := by
    rw [History.valAt_of_isLast hw (d := 0)]
    · rfl
    · refine ⟨by simp, rfl, ?_⟩
      intro e he _; simp at he; exact Or.inl he
  have eF1 : h.valAt {0, 1} (.fact () ()) = some 2 := by
    rw [History.valAt_of_isLast hw (d := 1)]
    · rfl
    · refine ⟨by simp, rfl, ?_⟩
      intro e he _
      simp at he
      rcases he with rfl | rfl
      · right; simp [h, linHist]
      · left; rfl
  have eE : ∀ W : Finset ℕ, 0 ∈ W → (∀ e ∈ W, e = 0 ∨ e = 1) →
      h.valAt W (.entry () ()) = some 0 := by
    intro W h0 hW
    rw [History.valAt_of_isLast hw (d := 0)]
    · rfl
    · refine ⟨h0, rfl, ?_⟩
      intro e he hwe
      rcases hW e he with rfl | rfl
      · left; rfl
      · exact absurd hwe (by simp [History.Writes, h, linHist, cexWrite1])
  have nP : ∀ W : Finset ℕ, ∀ k : Item Unit Unit Unit Unit,
      (k = .policy () ∨ k = .schema ()) → h.stamp W k = none := by
    intro W k hk
    apply History.stamp_eq_none
    intro d _ hwd
    rcases hk with rfl | rfl <;>
    · rcases d with _ | _ | d <;> simp [History.Writes, h, linHist, cexWrite1] at hwd
  have hcand : ∀ W : Finset ℕ, 0 ∈ W → (∀ e ∈ W, e = 0 ∨ e = 1) →
      cexScan.cand (h.valAt W) = {()} := by
    intro W h0 hW
    ext r; simp [Scan.cand, cexScan, eE W h0 hW]
  have eV : cexScan.eval () (h.valAt {0}) () = some () := by
    have hm : () ∈ cexScan.cand (h.valAt {0}) := by rw [hcand {0} (by simp) (by simp)]; simp
    simp only [Scan.eval, Scan.Returned, hm, true_and]
    simp [cexScan, restrict, eF0]
  have eH : cexScan.eval () (h.valAt {0, 1}) () = none := by
    simp [Scan.eval, Scan.Returned, cexScan, restrict, eF1]
  refine ⟨hw, by simp, ?_, cexScan_policyLocal, ?_, ?_⟩
  · intro d hd; simp at hd; subst hd; simp [h, linHist]
  · intro k hk d hd
    have hc := hcand {0, 1} (by simp) (by simp)
    have hnot : ¬ cexScan.Returned () (h.valAt {0, 1}) () := by
      simp [Scan.Returned, cexScan, restrict, eF1]
    have hfil : (cexScan.cand (h.valAt {0, 1})).filter
        (cexScan.Returned () (h.valAt {0, 1})) = ∅ := by
      rw [hc]; ext r; cases r; simp [hnot]
    simp only [Scan.visitReturned, hfil, Finset.biUnion_empty, Finset.union_empty,
      Finset.mem_insert, Finset.mem_singleton] at hk
    rw [nP _ k (by simpa [cexScan] using hk)] at hd
    cases hd
  · intro heq
    have := congrFun heq ()
    rw [eV, eH] at this
    cases this

/-- **Checking without ghosts is unsound.** At `V = {0}` the row has a live entry and is
returned. Dot 1 removes the entry, leaving a ghost stamped `1`. At `H = {0, 1}` the row is
no longer a candidate. A check that visits only live entries visits nothing stamped, says
"unchanged", and is wrong. -/
theorem noGhost_unsound :
    let h := linHist cexWrite2 (fun _ => none)
    h.WF ∧ ({0} : Finset ℕ) ⊆ {0, 1} ∧ CausallyClosed h.ctx {0} ∧
      cexScan.PolicyLocal ∧
      Check (h.stamp {0, 1}) {0} (cexScan.visitLive () (h.valAt {0, 1})) ∧
      cexScan.eval () (h.valAt {0}) ≠ cexScan.eval () (h.valAt {0, 1}) := by
  intro h
  have hw : h.WF := linHist_wf _ _
  have eF : ∀ W : Finset ℕ, 0 ∈ W → (∀ e ∈ W, e = 0 ∨ e = 1) →
      h.valAt W (.fact () ()) = some 1 := by
    intro W h0 hW
    rw [History.valAt_of_isLast hw (d := 0)]
    · rfl
    · refine ⟨h0, rfl, ?_⟩
      intro e he hwe
      rcases hW e he with rfl | rfl
      · left; rfl
      · exact absurd hwe (by simp [History.Writes, h, linHist, cexWrite2])
  have eE0 : h.valAt {0} (.entry () ()) = some 0 := by
    rw [History.valAt_of_isLast hw (d := 0)]
    · rfl
    · refine ⟨by simp, rfl, ?_⟩
      intro e he _; simp at he; exact Or.inl he
  have eE1 : h.valAt {0, 1} (.entry () ()) = none := by
    rw [History.valAt_of_isLast hw (d := 1)]
    · rfl
    · refine ⟨by simp, rfl, ?_⟩
      intro e he _
      simp at he
      rcases he with rfl | rfl
      · right; simp [h, linHist]
      · left; rfl
  have nP : ∀ W : Finset ℕ, ∀ k : Item Unit Unit Unit Unit,
      (k = .policy () ∨ k = .schema ()) → h.stamp W k = none := by
    intro W k hk
    apply History.stamp_eq_none
    intro d _ hwd
    rcases hk with rfl | rfl <;>
    · rcases d with _ | _ | d <;> simp [History.Writes, h, linHist, cexWrite2] at hwd
  have hc0 : cexScan.cand (h.valAt {0}) = {()} := by
    ext r; simp [Scan.cand, cexScan, eE0]
  have hc1 : cexScan.cand (h.valAt {0, 1}) = ∅ := by
    ext r; cases r; simp [Scan.cand, cexScan, eE1]
  have eV : cexScan.eval () (h.valAt {0}) () = some () := by
    have hm : () ∈ cexScan.cand (h.valAt {0}) := by rw [hc0]; simp
    simp only [Scan.eval, Scan.Returned, hm, true_and]
    simp [cexScan, restrict, eF {0} (by simp) (by simp)]
  have eH : cexScan.eval () (h.valAt {0, 1}) () = none := by
    simp [Scan.eval, Scan.Returned, hc1]
  refine ⟨hw, by simp, ?_, cexScan_policyLocal, ?_, ?_⟩
  · intro d hd; simp at hd; subst hd; simp [h, linHist]
  · intro k hk d hd
    have hlive : cexScan.entries.filter (fun k => h.valAt {0, 1} k ≠ none) = ∅ := by
      ext k
      simp only [Scan.entries, cexScan, Finset.mem_filter, Finset.mem_image,
        Finset.mem_product, Finset.mem_singleton, Finset.mem_univ, and_true,
        Finset.notMem_empty, iff_false, not_and, not_not]
      rintro ⟨⟨b, r⟩, -, rfl⟩
      cases b; cases r; exact eE1
    simp only [Scan.visitLive, hlive, hc1, Finset.biUnion_empty, Finset.union_empty,
      Finset.mem_insert, Finset.mem_singleton] at hk
    rw [nP _ k (by simpa [cexScan] using hk)] at hd
    cases hd
  · intro heq
    have := congrFun heq ()
    rw [eV, eH] at this
    cases this

/-! ### Compacting to the largest counter is unsound

Dots on one PG node come from a counter, but concurrent transactions on that node commit out
of order, so a larger counter is not a causal successor. Here dots 7 and 10 are concurrent
(both have context `{0}`). Dot 7 removes row `false`'s entry; dot 10 removes row `true`'s.
`V = {0, 10}` is causally closed and has an exception (it lacks 7). Compacting the bucket's
two ghosts into one marker stamped `max 7 10 = 10` makes the check pass, but row `false` is
returned at `V` and not at `H`. The compaction doesn't cover dot 7 (7 is not in `ctx 10`),
so `compaction_sound` doesn't apply. -/

/-- Contexts: dot 0 is first; every other dot has context `{0}`. -/
def cexCtx3 (d : ℕ) : Finset ℕ := if d = 0 then ∅ else {0}

def cexWrite3 (d : ℕ) (k : Item Unit Unit Bool Unit) : Option (Option ℕ) :=
  if d = 0 then (match k with | .entry _ _ => some (some 0) | _ => none)
  else if d = 7 ∧ k = .entry () false then some none
  else if d = 10 ∧ k = .entry () true then some none
  else none

def cexHist3 : History (Item Unit Unit Bool Unit) (Option ℕ) ℕ :=
  ⟨cexCtx3, cexWrite3, fun _ => none⟩

/-- Two rows in one bucket, no filter, an allow-all policy. -/
def cexScan3 : Scan Unit Unit Bool Unit ℕ Unit Unit where
  ty := ()
  range := {()}
  filtFields := ∅
  projFields := ∅
  filt := fun _ _ => true
  shape := fun _ _ => ()
  pol := fun _ _ _ _ => true
  polReads := fun _ _ _ _ => ∅

/-- Entries map to one bucket marker (`none`); other items are left alone. -/
def cexC3 : Item Unit Unit Bool Unit → Option (Item Unit Unit Bool Unit)
  | .entry _ _ => none
  | k => some k

/-- The marker's summary is the dot with the largest counter among the bucket's ghosts. -/
def cexSumm3 (H : Finset ℕ) : Option (Item Unit Unit Bool Unit) → Finset ℕ
  | none => {max 7 10}
  | some k => (cexHist3.stamp H k).toFinset

theorem cexHist3_writes {d : ℕ} {k : Item Unit Unit Bool Unit} (hw : cexHist3.Writes d k) :
    d = 0 ∨ (d = 7 ∧ k = .entry () false) ∨ (d = 10 ∧ k = .entry () true) := by
  simp only [History.Writes, cexHist3, cexWrite3] at hw
  split_ifs at hw with h0 h7 h10
  · exact Or.inl h0
  · exact Or.inr (Or.inl h7)
  · exact Or.inr (Or.inr h10)
  · simp at hw

theorem cexHist3_wf : cexHist3.WF where
  irrefl d := by simp only [cexHist3, cexCtx3]; split_ifs with h <;> simp [h]
  closed d := by
    intro e he x hx
    simp only [cexHist3, cexCtx3] at he hx ⊢
    split_ifs at he hx ⊢ with h1 h2 <;> simp_all
  ordered d e k hne hd he := by
    simp only [cexHist3, cexCtx3]
    rcases cexHist3_writes hd with rfl | ⟨rfl, rfl⟩ | ⟨rfl, rfl⟩ <;>
    rcases cexHist3_writes he with rfl | ⟨rfl, hk⟩ | ⟨rfl, hk⟩ <;>
    simp_all

theorem counterMax_compaction_unsound :
    let H : Finset ℕ := {0, 7, 10}
    let V : Finset ℕ := {0, 10}
    cexHist3.WF ∧ V ⊆ H ∧ CausallyClosed cexHist3.ctx V ∧ cexScan3.PolicyLocal ∧
      CheckC (cexSumm3 H) cexC3 V (cexScan3.visit () (cexHist3.valAt H)) ∧
      cexScan3.eval () (cexHist3.valAt V) ≠ cexScan3.eval () (cexHist3.valAt H) ∧
      ¬ Covers cexHist3.ctx (cexHist3.stamp H) cexC3 (cexSumm3 H) := by
  intro H V
  have hw := cexHist3_wf
  have hVH : V ⊆ H := by intro x; simp [V, H]; omega
  have hVc : CausallyClosed cexHist3.ctx V := by
    intro d hd x hx
    simp only [cexHist3, cexCtx3] at hx
    split_ifs at hx <;> simp_all [V]
  -- row `false` at `V`: only dot 0 wrote its entry
  have eV : cexHist3.valAt V (.entry () false) = some 0 := by
    rw [History.valAt_of_isLast hw (d := 0)]
    · rfl
    · refine ⟨by simp [V], by simp [History.Writes, cexHist3, cexWrite3], ?_⟩
      intro e he hwe
      simp only [V, Finset.mem_insert, Finset.mem_singleton] at he
      rcases he with rfl | rfl
      · left; rfl
      · rcases cexHist3_writes hwe with h | ⟨h, _⟩ | ⟨_, h⟩ <;> simp_all
  have eHf : cexHist3.valAt H (.entry () false) = none := by
    rw [History.valAt_of_isLast hw (d := 7)]
    · simp [cexHist3, cexWrite3]
    · refine ⟨by simp [H], by simp [History.Writes, cexHist3, cexWrite3], ?_⟩
      intro e he hwe
      rcases cexHist3_writes hwe with rfl | ⟨rfl, _⟩ | ⟨_, h⟩
      · right; simp [cexHist3, cexCtx3]
      · left; rfl
      · simp at h
  have eHt : cexHist3.valAt H (.entry () true) = none := by
    rw [History.valAt_of_isLast hw (d := 10)]
    · simp [cexHist3, cexWrite3]
    · refine ⟨by simp [H], by simp [History.Writes, cexHist3, cexWrite3], ?_⟩
      intro e he hwe
      rcases cexHist3_writes hwe with rfl | ⟨_, h⟩ | ⟨rfl, _⟩
      · right; simp [cexHist3, cexCtx3]
      · simp at h
      · left; rfl
  have nP : ∀ k : Item Unit Unit Bool Unit, (k = .policy () ∨ k = .schema ()) →
      cexHist3.stamp H k = none := by
    intro k hk
    apply History.stamp_eq_none
    intro d _ hwd
    rcases hk with rfl | rfl <;>
    rcases cexHist3_writes hwd with rfl | ⟨_, h⟩ | ⟨_, h⟩ <;>
    simp_all [History.Writes, cexHist3, cexWrite3]
  have hcH : cexScan3.cand (cexHist3.valAt H) = ∅ := by
    ext r
    simp only [Scan.cand, cexScan3, Finset.mem_filter, Finset.mem_univ, true_and,
      Finset.mem_singleton, Finset.notMem_empty, iff_false, not_exists, not_not]
    cases r
    · exact fun _ => eHf
    · exact fun _ => eHt
  have eVal : cexScan3.eval () (cexHist3.valAt V) false = some () := by
    have hm : false ∈ cexScan3.cand (cexHist3.valAt V) := by simp [Scan.cand, cexScan3, eV]
    simp only [Scan.eval, Scan.Returned, hm, true_and]
    simp [cexScan3]
  have eHal : cexScan3.eval () (cexHist3.valAt H) false = none := by
    simp [Scan.eval, Scan.Returned, hcH]
  have hneq : cexScan3.eval () (cexHist3.valAt V) ≠ cexScan3.eval () (cexHist3.valAt H) := by
    intro heq
    have := congrFun heq false
    rw [eVal, eHal] at this
    cases this
  have hcheck : CheckC (cexSumm3 H) cexC3 V (cexScan3.visit () (cexHist3.valAt H)) := by
    intro k hk
    simp only [Scan.visit, hcH, Finset.biUnion_empty, Finset.union_empty, Finset.mem_union,
      Finset.mem_insert, Finset.mem_singleton] at hk
    rcases hk with (rfl | rfl) | hk
    · simp [cexC3, cexSumm3, nP (.policy ()) (Or.inl rfl)]
    · simp [cexC3, cexSumm3, nP (.schema ()) (Or.inr rfl)]
    · simp only [Scan.entries, Finset.mem_image] at hk
      obtain ⟨_, _, rfl⟩ := hk
      simp [cexC3, cexSumm3, V]
  refine ⟨hw, hVH, hVc, fun _ _ _ _ _ _ => rfl, hcheck, hneq, ?_⟩
  intro hcov
  exact hneq (compaction_sound hw hVH hVc (cexScan3.eval_readSetSound
    (fun _ _ _ _ _ _ => rfl) ()) hcov hcheck)

end Fresh
end Shep
