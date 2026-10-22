/-
# Split lineage: which physical version of a key wins (design.md §3, §9.2, §9.6)

A materializer stores every version of a key, ordered so that the newest sorts first, and a
read at `V` takes the newest version whose stamp is in `V` (§3). A split's children keep their
parent's SSTs (§9.6, step 5) and start new logs. If versions were ordered by their *local*
log position alone, a parent's write at position 100 would sort ahead of a child's later write
at position 2, and the read would return the stale parent value.

The fix under test orders physical versions by **(lineage generation, local position)**,
lexicographically, where a partition's generation is strictly larger than its parent's.

## The model

* Partitions `P`, with `child p q` ("`q` is a child of `p`") and `gen : P → ℕ`. `Anc` is the
  transitive closure of `child`, so repeated splits are covered.
* A retired partition `p` has a split entry with dot `splitDot p`.
* The physical versions of one key: each has the partition whose log applied it (`part`), its
  position in that log (`pos`), and its stamp (`stamp`). Causality is `ctx : Dot → Finset Dot`.
* `Rules` collects the hypotheses. Each is a design rule (see the comments on the fields).

## Results

* `causal_total`: any two versions of the key are causally ordered.
* `physLt_of_causal`: the physical order agrees with causal order.
* `winner_iff_causalLast`, `winner_correct`: **for every version `V`** (causally closed or not),
  the physical winner (the maximum under (generation, position) among the versions whose stamp
  is in `V`) is exactly the causally latest write to the key in `V`.
* `winner_stamp`: tied to the freshness model, the winner's stamp is `History.stamp V k`.
* `localPos_unsound`: ordering by local position alone picks a stale version, in a history
  that satisfies every rule.
* Token membership across splits: `memL_consistent`, `memL_mono`, `fold_mono`,
  `fold_inflationary`, `fold_exact`, `fold_grows`.
-/
import ShepZSet.Freshness
import Mathlib.Order.Basic
import Mathlib.Order.Prod.Lex.Basic

noncomputable section
open Classical

namespace Shep
namespace Lineage

variable {P Dot Ver : Type*}

/-- A partition lineage: who split into whom, generations, and split-entry dots. -/
structure Tree (P Dot : Type*) where
  /-- `child p q`: `q` is one of the children `p` split into. -/
  child : P → P → Prop
  /-- The lineage generation, carried in every physical key. -/
  gen : P → ℕ
  /-- The dot of a retired partition's split entry. -/
  splitDot : P → Dot

namespace Tree

variable (T : Tree P Dot)

/-- `q` descends from `p` through one or more splits. -/
def Anc : P → P → Prop := Relation.TransGen T.child

/-- `p` has split. -/
def Retired (p : P) : Prop := ∃ q, T.child p q

end Tree

/-- The physical versions of one key, as the materializer's database holds them. -/
structure Versions (P Dot Ver : Type*) where
  /-- The partition whose log applied this version. -/
  part : Ver → P
  /-- Its position in that partition's log. -/
  pos : Ver → ℕ
  /-- Its stamp: the dot that makes it visible. -/
  stamp : Ver → Dot

/-- The physical sort key: (generation, local position), lexicographic. -/
def physKey (T : Tree P Dot) (X : Versions P Dot Ver) (w : Ver) : Lex (ℕ × ℕ) :=
  toLex (T.gen (X.part w), X.pos w)

/-- The sort key of the current layout: local position only. -/
def localKey (X : Versions P Dot Ver) (w : Ver) : ℕ := X.pos w

/-- The hypotheses. Each field is a design rule. -/
structure Rules (ctx : Dot → Finset Dot) (T : Tree P Dot) (X : Versions P Dot Ver) : Prop where
  /-- Causality is a strict order. -/
  irrefl : ∀ d, d ∉ ctx d
  closed : ∀ d, CausallyClosed ctx (ctx d)
  /-- **Generations increase from parent to child.** -/
  gen_lt : ∀ p q, T.child p q → T.gen p < T.gen q
  /-- Within one partition, a later write to the key is causally after an earlier one.
  Validation gives this (`Validation.validated_writes_ordered`). -/
  local_order : ∀ w w', X.part w = X.part w' → X.pos w < X.pos w' →
    X.stamp w ∈ ctx (X.stamp w')
  /-- One log position holds one version of the key. -/
  pos_inj : ∀ w w', X.part w = X.part w' → X.pos w = X.pos w' → w = w'
  /-- **One partition writes a key at a time.** Two partitions that both wrote the key are
  in one line of descent: splits are at group boundaries, and there are no merges. -/
  one_holder : ∀ w w', X.part w ≠ X.part w' →
    T.Anc (X.part w) (X.part w') ∨ T.Anc (X.part w') (X.part w)
  /-- **A split entry follows everything its partition applied.** Every version a retired
  partition applied is in the causal past of its split entry. For a local stamp this is the
  split entry's prefix. For a two-phase commit's part, stamped with a decision dot from
  another partition, it needs the intent release (or the split entry) to carry the decision
  dot in its context. -/
  before_split : ∀ w, T.Retired (X.part w) → X.stamp w ∈ ctx (T.splitDot (X.part w))
  /-- **A child's writes are causally after its parent's split entry.** The child's first
  entry has context `{p: i}`, and a proposal whose read version lacks `(p, i)` is rejected. -/
  child_after : ∀ w p, T.child p (X.part w) → T.splitDot p ∈ ctx (X.stamp w)
  /-- A child that splits again does so after it started. -/
  split_after : ∀ p q, T.child p q → T.Retired q → T.splitDot p ∈ ctx (T.splitDot q)

variable {ctx : Dot → Finset Dot} {T : Tree P Dot} {X : Versions P Dot Ver}

theorem ctx_trans (hc : ∀ d, CausallyClosed ctx (ctx d)) {a b c : Dot}
    (hab : a ∈ ctx b) (hbc : b ∈ ctx c) : a ∈ ctx c :=
  hc c b hbc hab

/-- Generations strictly increase along descent, through any number of splits. -/
theorem gen_lt_of_anc (R : Rules ctx T X) {p q : P} (h : T.Anc p q) : T.gen p < T.gen q := by
  induction h with
  | single h => exact R.gen_lt _ _ h
  | tail _ h ih => exact lt_trans ih (R.gen_lt _ _ h)

theorem anc_retired {p q : P} (h : T.Anc p q) : T.Retired p := by
  induction h with
  | single h => exact ⟨_, h⟩
  | tail _ _ ih => exact ih

/-- A descendant's split entry is causally after every ancestor's split entry. -/
theorem split_of_anc (R : Rules ctx T X) {p q : P} (h : T.Anc p q) (hq : T.Retired q) :
    T.splitDot p ∈ ctx (T.splitDot q) := by
  induction h with
  | single h => exact R.split_after _ _ h hq
  | @tail r q _ hrq ih =>
    exact ctx_trans R.closed (ih ⟨q, hrq⟩) (R.split_after _ _ hrq hq)

/-- A descendant's writes are causally after every ancestor's split entry. -/
theorem write_of_anc (R : Rules ctx T X) {p : P} {w : Ver} (h : T.Anc p (X.part w)) :
    T.splitDot p ∈ ctx (X.stamp w) := by
  cases h with
  | single h => exact R.child_after _ _ h
  | tail hpr hrq =>
    exact ctx_trans R.closed (split_of_anc R hpr ⟨_, hrq⟩) (R.child_after _ _ hrq)

/-- An ancestor's writes are causally before a descendant's. -/
theorem causal_of_anc (R : Rules ctx T X) {w w' : Ver} (h : T.Anc (X.part w) (X.part w')) :
    X.stamp w ∈ ctx (X.stamp w') :=
  ctx_trans R.closed (R.before_split w (anc_retired h)) (write_of_anc R h)

theorem not_both (R : Rules ctx T X) {a b : Dot} (hab : a ∈ ctx b) (hba : b ∈ ctx a) : False :=
  R.irrefl a (ctx_trans R.closed hab hba)

/-- **Writes to one key are causally ordered**, across any number of splits. -/
theorem causal_total (R : Rules ctx T X) {w w' : Ver} (hne : w ≠ w') :
    X.stamp w ∈ ctx (X.stamp w') ∨ X.stamp w' ∈ ctx (X.stamp w) := by
  by_cases hp : X.part w = X.part w'
  · rcases lt_trichotomy (X.pos w) (X.pos w') with h | h | h
    · exact Or.inl (R.local_order _ _ hp h)
    · exact absurd (R.pos_inj _ _ hp h) hne
    · exact Or.inr (R.local_order _ _ hp.symm h)
  · rcases R.one_holder _ _ hp with h | h
    · exact Or.inl (causal_of_anc R h)
    · exact Or.inr (causal_of_anc R h)

/-- **The physical order agrees with causal order.** -/
theorem physLt_of_causal (R : Rules ctx T X) {w w' : Ver}
    (h : X.stamp w ∈ ctx (X.stamp w')) : physKey T X w < physKey T X w' := by
  unfold physKey
  rw [Prod.Lex.toLex_lt_toLex]
  by_cases hp : X.part w = X.part w'
  · right
    refine ⟨by rw [hp], ?_⟩
    rcases lt_trichotomy (X.pos w) (X.pos w') with hl | hl | hl
    · exact hl
    · exact absurd h (by rw [R.pos_inj _ _ hp hl]; exact R.irrefl _)
    · exact (not_both R h (R.local_order _ _ hp.symm hl)).elim
  · left
    rcases R.one_holder _ _ hp with ha | ha
    · exact gen_lt_of_anc R ha
    · exact (not_both R h (causal_of_anc R ha)).elim

/-- `w` is the physical winner at `V`: among the versions whose stamp is in `V`, it has the
largest (generation, position). -/
def PhysWinner (T : Tree P Dot) (X : Versions P Dot Ver) (vers : Finset Ver) (V : Finset Dot)
    (w : Ver) : Prop :=
  w ∈ vers ∧ X.stamp w ∈ V ∧ ∀ w' ∈ vers, X.stamp w' ∈ V → physKey T X w' ≤ physKey T X w

/-- `w` is the causally latest write in `V` (`Fresh.History.IsLast`, for this key). -/
def CausalLast (ctx : Dot → Finset Dot) (X : Versions P Dot Ver) (vers : Finset Ver)
    (V : Finset Dot) (w : Ver) : Prop :=
  w ∈ vers ∧ X.stamp w ∈ V ∧ ∀ w' ∈ vers, X.stamp w' ∈ V → w' = w ∨ X.stamp w' ∈ ctx (X.stamp w)

/-- **The physical winner is the causally latest write**, for every `V`. -/
theorem winner_iff_causalLast (R : Rules ctx T X) (vers : Finset Ver) (V : Finset Dot)
    (w : Ver) : PhysWinner T X vers V w ↔ CausalLast ctx X vers V w := by
  constructor
  · rintro ⟨hw, hV, hmax⟩
    refine ⟨hw, hV, fun w' hw' hV' => ?_⟩
    by_cases he : w' = w
    · exact Or.inl he
    rcases causal_total R he with h | h
    · exact Or.inr h
    · exact absurd (hmax w' hw' hV') (not_le.mpr (physLt_of_causal R h))
  · rintro ⟨hw, hV, hlast⟩
    refine ⟨hw, hV, fun w' hw' hV' => ?_⟩
    rcases hlast w' hw' hV' with rfl | h
    · exact le_refl _
    · exact le_of_lt (physLt_of_causal R h)

/-- A physical winner exists whenever some version is visible. -/
theorem winner_exists (T : Tree P Dot) (X : Versions P Dot Ver) {vers : Finset Ver}
    {V : Finset Dot} (hne : ∃ w ∈ vers, X.stamp w ∈ V) : ∃ w, PhysWinner T X vers V w := by
  have hn : (vers.filter (fun w => X.stamp w ∈ V)).Nonempty := by
    obtain ⟨w, hw, hV⟩ := hne; exact ⟨w, Finset.mem_filter.mpr ⟨hw, hV⟩⟩
  obtain ⟨w, hw, hmax⟩ := Finset.exists_max_image _ (physKey T X) hn
  obtain ⟨hw, hV⟩ := Finset.mem_filter.mp hw
  exact ⟨w, hw, hV, fun w' hw' hV' => hmax w' (Finset.mem_filter.mpr ⟨hw', hV'⟩)⟩

/-- **Correct winner (headline).** Under the rules, through any number of splits, for every
version `V` that makes some version of the key visible, the physical winner exists, is
unique, and is the causally latest write to the key in `V`. -/
theorem winner_correct (R : Rules ctx T X) (vers : Finset Ver) (V : Finset Dot)
    (hne : ∃ w ∈ vers, X.stamp w ∈ V) :
    ∃ w, PhysWinner T X vers V w ∧ CausalLast ctx X vers V w ∧
      ∀ w', PhysWinner T X vers V w' → w' = w := by
  obtain ⟨w, hw⟩ := winner_exists T X hne
  refine ⟨w, hw, (winner_iff_causalLast R vers V w).mp hw, fun w' hw' => ?_⟩
  by_contra hne'
  rcases causal_total R hne' with h | h
  · exact absurd (hw'.2.2 w hw.1 hw.2.1) (not_le.mpr (physLt_of_causal R h))
  · exact absurd (hw.2.2 w' hw'.1 hw'.2.1) (not_le.mpr (physLt_of_causal R h))

/-- **Tied to the freshness model.** If the versions of key `k` are exactly the writes of `k`
in a well-formed history with the same causality, the physical winner's stamp is the stamp
`Fresh.History.stamp V k` that reads and freshness checks use, so the value read is
`valAt V k`. -/
theorem winner_stamp {K Val : Type*} (R : Rules ctx T X) {h : Fresh.History K Val Dot}
    (hw : h.WF) (hctx : h.ctx = ctx) {k : K} {vers : Finset Ver}
    (hwr : ∀ d, h.Writes d k ↔ ∃ w ∈ vers, X.stamp w = d) {V : Finset Dot} {w : Ver}
    (hwin : PhysWinner T X vers V w) : h.stamp V k = some (X.stamp w) := by
  obtain ⟨hw', hV, hlast⟩ := (winner_iff_causalLast R vers V w).mp hwin
  apply Fresh.History.stamp_eq_some hw
  refine ⟨hV, (hwr _).mpr ⟨w, hw', rfl⟩, fun e he hek => ?_⟩
  obtain ⟨w', hw'', rfl⟩ := (hwr e).mp hek
  rcases hlast w' hw'' he with rfl | h'
  · exact Or.inl rfl
  · exact Or.inr (hctx ▸ h')

/-! ## Counterexample: local position alone

The parent (`false`, generation 0) applies a write at position 100 (dot `0`), then splits at
dot `1`. The child (`true`, generation 1) applies a later write to the same key at its
position 2 (dot `2`). Every rule holds, and `V = {0, 1, 2}` is causally closed. By local
position the parent's version wins, though the child's write is causally later. -/

def cexCtx : ℕ → Finset ℕ
  | 0 => ∅
  | 1 => {0}
  | 2 => {0, 1}
  | _ => ∅

def cexTree : Tree Bool ℕ where
  child p q := p = false ∧ q = true
  gen p := if p then 1 else 0
  splitDot _ := 1

/-- Version `false`: the parent's write. Version `true`: the child's. -/
def cexVers : Versions Bool ℕ Bool where
  part w := w
  pos w := if w then 2 else 100
  stamp w := if w then 2 else 0

theorem cexCtx_closed (d : ℕ) : CausallyClosed cexCtx (cexCtx d) := by
  intro e he x hx
  rcases d with _ | _ | _ | d <;> simp [cexCtx] at he ⊢ <;>
    rcases he with rfl | rfl <;> simp_all [cexCtx]

theorem cex_rules : Rules cexCtx cexTree cexVers where
  irrefl d := by rcases d with _ | _ | _ | d <;> simp [cexCtx]
  closed := cexCtx_closed
  gen_lt p q h := by obtain ⟨rfl, rfl⟩ := h; simp [cexTree]
  local_order w w' hp hl := by
    simp only [cexVers] at hp; subst hp; simp at hl
  pos_inj w w' hp _ := hp
  one_holder w w' hp := by
    cases w <;> cases w' <;> simp_all [Tree.Anc, cexVers]
    · exact Or.inl (Relation.TransGen.single ⟨rfl, rfl⟩)
    · exact Or.inr (Relation.TransGen.single ⟨rfl, rfl⟩)
  before_split w hr := by
    obtain ⟨q, hq⟩ := hr
    cases w <;> simp_all [cexTree, cexVers, cexCtx]
  child_after w p h := by
    obtain ⟨rfl, hw⟩ := h
    cases w <;> simp_all [cexTree, cexVers, cexCtx]
  split_after p q h hq := by
    obtain ⟨_, rfl⟩ := h
    obtain ⟨r, hr⟩ := hq
    simp [cexTree] at hr

/-- **Ordering by local position alone is unsound.** Every rule holds and `V` is causally
closed, but the version with the largest local position is the parent's, while the causally
latest write is the child's. (Generation-first ordering picks the child's, by
`winner_correct`.) -/
theorem localPos_unsound :
    let V : Finset ℕ := {0, 1, 2}
    Rules cexCtx cexTree cexVers ∧ CausallyClosed cexCtx V ∧
      (∀ w ∈ (Finset.univ : Finset Bool), cexVers.stamp w ∈ V →
        localKey cexVers w ≤ localKey cexVers false) ∧
      CausalLast cexCtx cexVers Finset.univ V true ∧
      ¬ CausalLast cexCtx cexVers Finset.univ V false ∧
      PhysWinner cexTree cexVers Finset.univ V true := by
  intro V
  have hlast : CausalLast cexCtx cexVers Finset.univ V true := by
    refine ⟨Finset.mem_univ _, by simp [V, cexVers], fun w' _ _ => ?_⟩
    cases w' <;> simp [cexVers, cexCtx]
  refine ⟨cex_rules, ?_, ?_, hlast, ?_, (winner_iff_causalLast cex_rules _ _ _).mpr hlast⟩
  · intro d hd x hx
    simp only [V, Finset.mem_insert, Finset.mem_singleton] at hd
    rcases hd with rfl | rfl | rfl <;> simp [cexCtx, V] at hx ⊢ <;> omega
  · intro w _ _; cases w <;> simp [localKey, cexVers]
  · rintro ⟨-, -, h⟩
    rcases h true (Finset.mem_univ _) (by simp [V, cexVers]) with h | h
    · cases h
    · simp [cexVers, cexCtx] at h

/-! ## Token membership across splits (§9.6)

A token names, per partition, a prefix (`pfx p = some n`: entries `0 … n-1`; `none`: not
named), plus extras. A retired partition `p` split at its entry `idx p`. The design's rule:
`(p, j) ∈ V` iff `V`'s prefix for `p` covers `j`, `(p, j)` is an extra, or **`V` names any
descendant of `p` and `j ≤ idx p`** (`memL`). `memP` is the plain rule, without the lineage
clause.

* `memL_consistent`: on a token whose plain dots are causally closed, and whose named
  partitions each hold their first entry, the lineage clause adds nothing: `memL = memP`. It
  only restates causality, given that every entry of a child has its parent's log through the
  split entry in its causal past.
* `memL_mono`: membership is monotone in the token.
* `fold`: folding a retired parent's entry into its children (§9.4). `fold_mono`: folding is
  monotone. `fold_inflationary`: folding never loses a dot. `fold_exact`: folding is exact when
  the token's prefix for the parent reaches the split entry. `fold_grows`: otherwise it adds
  dots the token didn't hold.
-/

section Tokens

variable [DecidableEq P]

/-- A version token: a prefix per named partition (`none`: not named), plus extras. Dots are
`(partition, index)`, indices from `0`. -/
structure Token (P : Type*) where
  pfx : P → Option ℕ
  ext : Finset (P × ℕ)

/-- Plain membership: the prefix covers the dot, or it's an extra. -/
def memP (V : Token P) (d : P × ℕ) : Prop :=
  (∃ n, V.pfx d.1 = some n ∧ d.2 < n) ∨ d ∈ V.ext

/-- Membership with the lineage rule: plainly, or `V` names a descendant of the dot's
partition and the dot is at or before that partition's split entry. -/
def memL (T : Tree P Dot) (idx : P → ℕ) (V : Token P) (d : P × ℕ) : Prop :=
  memP V d ∨ ∃ q, T.Anc d.1 q ∧ V.pfx q ≠ none ∧ d.2 ≤ idx d.1

omit [DecidableEq P] in
/-- Every entry of a child has its parent's log through the split entry in its causal past,
through any number of splits. -/
theorem anc_ctx {T : Tree P Dot} {idx : P → ℕ} {cx : P × ℕ → Finset (P × ℕ)}
    (hcl : ∀ d, CausallyClosed cx (cx d))
    (hR : ∀ p q, T.child p q → ∀ j ≤ idx p, ∀ k, (p, j) ∈ cx (q, k))
    {p q : P} (h : T.Anc p q) : ∀ j ≤ idx p, ∀ k, (p, j) ∈ cx (q, k) := by
  induction h with
  | single h => exact hR _ _ h
  | @tail r q _ hrq ih =>
    intro j hj k
    exact hcl (q, k) (r, idx r) (hR _ _ hrq (idx r) le_rfl k) (ih j hj (idx r))

omit [DecidableEq P] in
/-- **The lineage rule is consistent with causality.** Suppose every entry of a child has the
parent's log through its split entry in its causal past (the child's first entry has context
`{p: i}`, and every later proposal's read version must contain `(p, i)`). Then for a token
whose plain dots are causally closed and whose named partitions each hold their first entry,
membership with the lineage rule is plain membership. -/
theorem memL_consistent {T : Tree P Dot} {idx : P → ℕ} {cx : P × ℕ → Finset (P × ℕ)}
    (hcl : ∀ d, CausallyClosed cx (cx d))
    (hR : ∀ p q, T.child p q → ∀ j ≤ idx p, ∀ k, (p, j) ∈ cx (q, k))
    {V : Token P} (hV : ∀ d, memP V d → ∀ e ∈ cx d, memP V e)
    (hnamed : ∀ q n, V.pfx q = some n → 0 < n) (d : P × ℕ) :
    memL T idx V d ↔ memP V d := by
  refine ⟨fun h => ?_, Or.inl⟩
  rcases h with h | ⟨q, hq, hn, hj⟩
  · exact h
  · obtain ⟨n, hn'⟩ := Option.ne_none_iff_exists'.mp hn
    have hq0 : memP V (q, 0) := Or.inl ⟨n, hn', hnamed q n hn'⟩
    exact hV (q, 0) hq0 d (anc_ctx hcl hR hq d.2 hj 0)

/-- Componentwise order on tokens. -/
def TokLe (V W : Token P) : Prop :=
  (∀ p n, V.pfx p = some n → ∃ m, W.pfx p = some m ∧ n ≤ m) ∧ V.ext ⊆ W.ext

omit [DecidableEq P] in
/-- **Membership is monotone in the token.** -/
theorem memL_mono {T : Tree P Dot} {idx : P → ℕ} {V W : Token P} (h : TokLe V W)
    {d : P × ℕ} (hd : memL T idx V d) : memL T idx W d := by
  rcases hd with (⟨n, hn, hj⟩ | he) | ⟨q, hq, hn, hj⟩
  · obtain ⟨m, hm, hnm⟩ := h.1 _ n hn
    exact Or.inl (Or.inl ⟨m, hm, by omega⟩)
  · exact Or.inl (Or.inr (h.2 he))
  · obtain ⟨n, hn'⟩ := Option.ne_none_iff_exists'.mp hn
    obtain ⟨m, hm, -⟩ := h.1 q n hn'
    exact Or.inr ⟨q, hq, by simp [hm], hj⟩

/-- Fold a retired parent `p`'s entry into its children: drop `p`, and name each child (at
its own prefix, or the empty prefix). A token that doesn't name `p` is unchanged. -/
def fold (T : Tree P Dot) (V : Token P) (p : P) : Token P :=
  if V.pfx p = none then V else
    { pfx := fun r => if r = p then none
        else if T.child p r then some ((V.pfx r).getD 0) else V.pfx r
      ext := V.ext }

/-- **Folding is monotone.** -/
theorem fold_mono {T : Tree P Dot} {V W : Token P} (h : TokLe V W) (p : P) :
    TokLe (fold T V p) (fold T W p) := by
  by_cases hV : V.pfx p = none
  · by_cases hW : W.pfx p = none
    · simpa [fold, hV, hW] using h
    · simp only [fold, hV, hW, ite_true, ite_false]
      refine ⟨fun r n hn => ?_, h.2⟩
      by_cases hr : r = p
      · subst hr; rw [hV] at hn; cases hn
      · obtain ⟨m, hm, hnm⟩ := h.1 r n hn
        by_cases hc : T.child p r <;> simp [hr, hc, hm, hnm]
  · have hW : W.pfx p ≠ none := by
      obtain ⟨n, hn⟩ := Option.ne_none_iff_exists'.mp hV
      obtain ⟨m, hm, -⟩ := h.1 p n hn
      simp [hm]
    simp only [fold, hV, hW, ite_false]
    refine ⟨fun r n hn => ?_, h.2⟩
    by_cases hr : r = p
    · simp [hr] at hn
    · by_cases hc : T.child p r
      · simp only [hr, hc, ite_true, ite_false, Option.some.injEq] at hn ⊢
        cases hVr : V.pfx r with
        | none => simp [hVr] at hn; subst hn; exact ⟨_, rfl, Nat.zero_le _⟩
        | some a =>
          obtain ⟨m, hm, ham⟩ := h.1 r a hVr
          simp [hVr] at hn; subst hn; exact ⟨m, by simp [hm], ham⟩
      · simp only [hr, hc, ite_false] at hn ⊢
        exact h.1 r n hn

omit [DecidableEq P] in
theorem child_ne {T : Tree P Dot} (hgen : ∀ p q, T.child p q → T.gen p < T.gen q)
    {p q : P} (h : T.child p q) : q ≠ p := by
  intro e; subst e; exact lt_irrefl _ (hgen _ _ h)

/-- **Folding never loses a dot**, for a retired parent whose prefix in the token doesn't run
past its split entry. -/
theorem fold_inflationary {T : Tree P Dot} {idx : P → ℕ}
    (hgen : ∀ p q, T.child p q → T.gen p < T.gen q) {V : Token P} {p : P}
    (hret : T.Retired p) (hn : ∀ n, V.pfx p = some n → n ≤ idx p + 1) {d : P × ℕ}
    (hd : memL T idx V d) : memL T idx (fold T V p) d := by
  by_cases hV : V.pfx p = none
  · simpa [fold, hV] using hd
  obtain ⟨c, hc⟩ := hret
  have hcp := child_ne hgen hc
  have hfc : (fold T V p).pfx c ≠ none := by simp [fold, hV, hcp, hc]
  have hfp : ∀ r, r ≠ p → V.pfx r ≠ none → (fold T V p).pfx r ≠ none := by
    intro r hr hne
    obtain ⟨a, ha⟩ := Option.ne_none_iff_exists'.mp hne
    by_cases hrc : T.child p r <;> simp [fold, hV, hr, hrc, ha]
  rcases hd with (⟨n, hn', hj⟩ | he) | ⟨q, hq, hnq, hj⟩
  · by_cases hdp : d.1 = p
    · refine Or.inr ⟨c, ?_, hfc, ?_⟩
      · rw [hdp]; exact Relation.TransGen.single hc
      · rw [hdp] at hn' ⊢; have := hn n hn'; omega
    · refine Or.inl (Or.inl ⟨n, ?_, hj⟩)
      by_cases hrc : T.child p d.1 <;> simp [fold, hV, hdp, hrc, hn']
  · exact Or.inl (Or.inr (by simpa [fold, hV] using he))
  · by_cases hqp : q = p
    · subst hqp
      exact Or.inr ⟨c, Relation.TransGen.tail hq hc, hfc, hj⟩
    · exact Or.inr ⟨q, hq, hfp q hqp hnq, hj⟩

/-- **Folding is exact** when the token's prefix for `p` reaches `p`'s split entry, given that
each partition has one parent. -/
theorem fold_exact {T : Tree P Dot} {idx : P → ℕ}
    (hpar : ∀ p p' q, T.child p q → T.child p' q → p = p') {V : Token P} {p : P}
    (hfull : V.pfx p = some (idx p + 1)) {d : P × ℕ}
    (hd : memL T idx (fold T V p) d) : memL T idx V d := by
  have hV : V.pfx p ≠ none := by simp [hfull]
  simp only [fold, hV, ite_false] at hd
  rcases hd with (⟨n, hn, hj⟩ | he) | ⟨q, hq, hnq, hj⟩
  · have hdp : d.1 ≠ p := by intro e; simp [e] at hn
    by_cases hrc : T.child p d.1
    · simp only [hdp, hrc, ite_true, ite_false, Option.some.injEq] at hn
      cases hVr : V.pfx d.1 with
      | none => simp [hVr] at hn; omega
      | some a => simp [hVr] at hn; subst hn; exact Or.inl (Or.inl ⟨_, hVr, hj⟩)
    · simp only [hdp, hrc, ite_false] at hn
      exact Or.inl (Or.inl ⟨n, hn, hj⟩)
  · exact Or.inl (Or.inr he)
  · have hqp : q ≠ p := by intro e; simp [e] at hnq
    by_cases hqc : T.child p q
    · -- `d.1` is `p` or an ancestor of `p`
      have : d.1 = p ∨ T.Anc d.1 p := by
        cases hq with
        | single h => exact Or.inl (hpar _ _ _ h hqc)
        | tail h h' => rw [hpar _ _ _ h' hqc] at h; exact Or.inr h
      rcases this with hdp | hdp
      · refine Or.inl (Or.inl ⟨idx p + 1, by rw [hdp, hfull], ?_⟩)
        rw [hdp] at hj; omega
      · exact Or.inr ⟨p, hdp, hV, hj⟩
    · simp only [hqp, hqc, ite_false] at hnq
      exact Or.inr ⟨q, hq, hnq, hj⟩

/-- **Folding a short prefix adds dots.** The parent (`false`) split at entry `5`; the token
names it only through entry `1` (prefix `2`). After folding, `(false, 3)` is a member, though
it wasn't before. So a token may be folded only when its prefix for the parent reaches the
split entry, or when over-approximating is safe (a causal `at_least` token, never a `since`
result version). -/
theorem fold_grows :
    let T : Tree Bool Unit := ⟨fun p q => p = false ∧ q = true, fun p => if p then 1 else 0,
      fun _ => ()⟩
    let V : Token Bool := ⟨fun p => if p then none else some 2, ∅⟩
    ¬ memL T (fun _ => 5) V (false, 3) ∧ memL T (fun _ => 5) (fold T V false) (false, 3) := by
  intro T V
  constructor
  · rintro ((⟨n, hn, hj⟩ | he) | ⟨q, hq, hnq, -⟩)
    · simp [V] at hn; omega
    · simp [V] at he
    · cases q
      · cases hq with
        | single h => simp [T] at h
        | tail _ h => simp [T] at h
      · simp [V] at hnq
  · exact Or.inr ⟨true, Relation.TransGen.single ⟨rfl, rfl⟩, by simp [fold, V, T], by simp⟩

end Tokens

end Lineage
end Shep
