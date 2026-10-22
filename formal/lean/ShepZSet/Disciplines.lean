/-
# Disciplines are preserved (design.md §4.1.2, §7.6)

Planner obligation 4. The leader rejects any entry whose delta breaks a declared
discipline. This file states those checks as predicates on deltas and proves what the planner
relies on: frozen facts never change after creation, and permanent lifecycles never retract.

## The model

Facts are `(node, path, value)`, as in `Facts.lean`. Two reserved segments matter here: `@type`
(`sch.ty`) and `@deleted_at` (`sch.del`). A node is **created** in a state when it has a live
`@type` fact. A placeholder has only `@placeholder`, so it isn't created, and a fill writes
`@type` (§3, `Placeholders.lean`). A node is **deleted** when it has a live `@deleted_at` fact.

`sch.frz` marks the frozen paths: frozen fields, every fact of a frozen edge (an edge is the
fact `(src, [edgeType, dst], sortKey)`), and `@type` itself, which a create or fill writes once
and nothing retracts. `sch.perm` marks the nodes of `#[permanent]` types.

## The leader's checks, per entry `d` (against the state at its context)

* `FrozenOK`: **a frozen fact is written only by the entry that creates or fills its node.**
  If `d`'s delta has a fact on a frozen path of `n`, then `n` isn't created at `d`'s context,
  and `d` writes `n`'s `@type`. For frozen edges this is the design's rule word for word. For
  frozen fields it is stronger than the design's "no `−old`": see `weakFrozen_unsound`.
* `PermOK`: **no `@deleted_at` fact, `+` or `−`, on a `#[permanent]` node.** So it can't be
  deleted or restored.

(`LeaderOps.ctx_agrees_logpos` shows that the state at an entry's context agrees with the
state at its log position on the keys it writes. So the leader can check `FrozenOK` against its
log position, and that is the same check.)

## Results

The hypotheses are those of `wf_stateAt_closed`: causality is a strict order with closed
contexts, and concurrent dots write prefix-incomparable paths (`Hist`).

* `no_ty_in_ctx`, `ty_unique`: at most one dot ever writes a given node's `@type`. That dot is
  its creating (or filling) entry.
* `stateAt_frozen`: a frozen fact's weight at any `V` is its weight in the creating entry's
  delta if that entry is in `V`, and `0` otherwise.
* `frozen_eq_creation`: **every frozen fact's value at any `V` that contains the creating
  entry equals its value at creation** (`insert c (ctx c)`).
* `created_mono`, `frozen_mono`: for `V ⊆ V'`, a node created at `V` is created at `V'`, and
  its frozen facts are the same at both.
* `perm_not_deleted`, `present_mono`: **a permanent lifecycle never retracts.** A permanent
  node is never deleted, and once present it stays present at every larger version.
* `weakFrozen_unsound`: the design's literal rule for frozen fields, "no `−old` on a frozen
  fact of an existing node", lets a later entry add a value to a frozen field that was unset at
  creation. The field's value then differs from its value at creation.
-/
import ShepZSet.Causal

noncomputable section
open Classical

namespace Shep
namespace Disc

variable {N Seg V Dot : Type*}

/-- The reserved segments and the declared disciplines. -/
structure Schema (N Seg : Type*) where
  /-- the `@type` segment -/
  ty : Seg
  /-- the `@deleted_at` segment -/
  del : Seg
  /-- frozen paths: frozen fields, frozen edges' facts, and `@type` -/
  frz : Path Seg → Prop
  /-- nodes of `#[permanent]` types -/
  perm : N → Prop
  /-- `@type` is written only by a create or a fill. -/
  frz_ty : frz [ty]

variable (sch : Schema N Seg)

/-- Node `n` has a live `@type` fact in `σ`. For a delta, this means it writes `n`'s `@type`. -/
def Created (σ : ZSet (Fact N Seg V)) (n : N) : Prop := ∃ v, σ (n, [sch.ty], v) ≠ 0

/-- Node `n` has a live `@deleted_at` fact. -/
def Deleted (σ : ZSet (Fact N Seg V)) (n : N) : Prop := ∃ v, σ (n, [sch.del], v) ≠ 0

/-- The lifecycle is present: created and not deleted. -/
def Present (σ : ZSet (Fact N Seg V)) (n : N) : Prop := Created sch σ n ∧ ¬ Deleted sch σ n

/-- **The leader's frozen check.** A fact on a frozen path is written only by the entry that
creates or fills the node: the node isn't created at the entry's context, and the entry writes
its `@type`. -/
def FrozenOK (delta : Dot → ZSet (Fact N Seg V)) (ctx : Dot → Finset Dot) (d : Dot) : Prop :=
  ∀ n p v, sch.frz p → delta d (n, p, v) ≠ 0 →
    ¬ Created sch (stateAt delta (ctx d)) n ∧ Created sch (delta d) n

/-- **The leader's permanent check.** No `@deleted_at` fact on a permanent node. -/
def PermOK (delta : Dot → ZSet (Fact N Seg V)) (d : Dot) : Prop :=
  ∀ n v, sch.perm n → delta d (n, [sch.del], v) = 0

/-- The hypotheses of `wf_stateAt_closed` about causality: a strict order with closed contexts,
and concurrent dots write prefix-incomparable paths. -/
structure Hist (delta : Dot → ZSet (Fact N Seg V)) (ctx : Dot → Finset Dot) : Prop where
  irrefl : ∀ d, d ∉ ctx d
  closed : ∀ d, CausallyClosed ctx (ctx d)
  conc : ∀ d e, d ≠ e → d ∉ ctx e → e ∉ ctx d → PathDisjoint (delta d) (delta e)

variable {sch}
variable {delta : Dot → ZSet (Fact N Seg V)} {ctx : Dot → Finset Dot}

/-- **No dot that writes `n`'s `@type` has another such dot in its context.** If one did, take
the causally last one, `m`. Then `n` is created at the context, and the frozen check rejects the
later write. -/
theorem no_ty_in_ctx (hH : Hist delta ctx) (hF : ∀ d, FrozenOK sch delta ctx d) (n : N) :
    ∀ d, Created sch (delta d) n → ∀ e ∈ ctx d, ¬ Created sch (delta e) n := by
  suffices H : ∀ c d, (ctx d).card = c → Created sch (delta d) n →
      ∀ e ∈ ctx d, ¬ Created sch (delta e) n from fun d => H _ d rfl
  intro c
  induction c using Nat.strong_induction_on with
  | _ c ih =>
  intro d hcd hd e he hte
  set D := (ctx d).filter (fun e => Created sch (delta e) n) with hD
  have hne : D.Nonempty := ⟨e, Finset.mem_filter.mpr ⟨he, hte⟩⟩
  obtain ⟨m, hmD, hmax⟩ := Finset.exists_max_image D (fun d => (ctx d).card) hne
  obtain ⟨hm, htm⟩ := Finset.mem_filter.mp hmD
  have hsub : ctx m ⊆ ctx d := hH.closed d m hm
  have hlt : (ctx m).card < c := by
    rw [← hcd]
    exact Finset.card_lt_card
      (Finset.ssubset_iff_subset_ne.mpr ⟨hsub, fun heq => hH.irrefl m (heq ▸ hm)⟩)
  have ihm := ih _ hlt m rfl htm
  -- every dot of `ctx d` that writes `n`'s `@type` is `m`
  have honly : ∀ e ∈ ctx d, Created sch (delta e) n → e = m := by
    intro e he hte
    by_contra hem
    by_cases h1 : e ∈ ctx m
    · exact ihm e h1 hte
    by_cases h2 : m ∈ ctx e
    · have hss : ctx m ⊂ ctx e := Finset.ssubset_iff_subset_ne.mpr
        ⟨hH.closed e m h2, fun heq => hH.irrefl m (heq ▸ h2)⟩
      have := Finset.card_lt_card hss
      have := hmax e (Finset.mem_filter.mpr ⟨he, hte⟩)
      omega
    · obtain ⟨v, hv⟩ := hte
      obtain ⟨w, hw⟩ := htm
      exact hH.conc e m hem h1 h2 n _ _ v w hv hw (Or.inl (List.prefix_refl _))
  have hst : ∀ v, stateAt delta (ctx d) (n, [sch.ty], v) = delta m (n, [sch.ty], v) := by
    intro v
    simp only [stateAt, Finsupp.finsetSum_apply]
    rw [Finset.sum_eq_single m]
    · intro b hb hbm
      by_contra hne
      exact hbm (honly b hb ⟨v, hne⟩)
    · intro h; exact absurd hm h
  obtain ⟨v, hv⟩ := htm
  have hcr : Created sch (stateAt delta (ctx d)) n := ⟨v, by rw [hst]; exact hv⟩
  obtain ⟨w, hw⟩ := hd
  exact (hF d n [sch.ty] w sch.frz_ty hw).1 hcr

/-- **At most one dot writes a node's `@type`**: its creating (or filling) entry. -/
theorem ty_unique (hH : Hist delta ctx) (hF : ∀ d, FrozenOK sch delta ctx d) {n : N} {d e : Dot}
    (hd : Created sch (delta d) n) (he : Created sch (delta e) n) : d = e := by
  by_contra hne
  by_cases h1 : d ∈ ctx e
  · exact no_ty_in_ctx hH hF n e he d h1 hd
  by_cases h2 : e ∈ ctx d
  · exact no_ty_in_ctx hH hF n d hd e h2 he
  obtain ⟨v, hv⟩ := hd
  obtain ⟨w, hw⟩ := he
  exact hH.conc d e hne h1 h2 n _ _ v w hv hw (Or.inl (List.prefix_refl _))

/-- Every dot that writes a frozen fact of `n` is `n`'s creating entry `c`. -/
theorem frozen_writer (hH : Hist delta ctx) (hF : ∀ d, FrozenOK sch delta ctx d) {n : N}
    {c : Dot} (hc : Created sch (delta c) n) {p : Path Seg} (hp : sch.frz p) {d : Dot} {v : V}
    (hd : delta d (n, p, v) ≠ 0) : d = c :=
  ty_unique hH hF (hF d n p v hp hd).2 hc

/-- **A frozen fact's weight at any `V`** is its weight in the creating entry's delta when that
entry is in `V`, and `0` otherwise. -/
theorem stateAt_frozen (hH : Hist delta ctx) (hF : ∀ d, FrozenOK sch delta ctx d) {n : N}
    {c : Dot} (hc : Created sch (delta c) n) {p : Path Seg} (hp : sch.frz p) (X : Finset Dot)
    (v : V) : stateAt delta X (n, p, v) = if c ∈ X then delta c (n, p, v) else 0 := by
  simp only [stateAt, Finsupp.finsetSum_apply]
  split_ifs with hcV
  · rw [Finset.sum_eq_single c]
    · intro b _ hbc
      by_contra hne
      exact hbc (frozen_writer hH hF hc hp hne)
    · intro h; exact absurd hcV h
  · apply Finset.sum_eq_zero
    intro b hb
    by_contra hne
    exact hcV (frozen_writer hH hF hc hp hne ▸ hb)

/-- **Frozen facts keep their value at creation.** For every version `V` that contains the
creating entry `c`, every frozen fact of `n` has the same weight at `V` as at `c`'s creation
(`c` with its context). -/
theorem frozen_eq_creation (hH : Hist delta ctx) (hF : ∀ d, FrozenOK sch delta ctx d) {n : N}
    {c : Dot} (hc : Created sch (delta c) n) {p : Path Seg} (hp : sch.frz p) {X : Finset Dot}
    (hcV : c ∈ X) (v : V) :
    stateAt delta X (n, p, v) = stateAt delta (insert c (ctx c)) (n, p, v) := by
  rw [stateAt_frozen hH hF hc hp, stateAt_frozen hH hF hc hp, ]
  simp [hcV]

/-- A node created at `V` has its creating entry in `V`. -/
theorem creator_of_created {X : Finset Dot} {n : N}
    (h : Created sch (stateAt delta X) n) : ∃ c ∈ X, Created sch (delta c) n := by
  obtain ⟨v, hv⟩ := h
  simp only [stateAt, Finsupp.finsetSum_apply] at hv
  obtain ⟨c, hc, hne⟩ := Finset.exists_ne_zero_of_sum_ne_zero hv
  exact ⟨c, hc, v, hne⟩

/-- **Creation is never undone**: a node created at `V` is created at every `V' ⊇ V`. -/
theorem created_mono (hH : Hist delta ctx) (hF : ∀ d, FrozenOK sch delta ctx d) {X X' : Finset Dot}
    (hVV : X ⊆ X') {n : N} (h : Created sch (stateAt delta X) n) :
    Created sch (stateAt delta X') n := by
  obtain ⟨c, hcV, hc⟩ := creator_of_created h
  obtain ⟨v, hv⟩ := hc
  refine ⟨v, ?_⟩
  rw [stateAt_frozen hH hF ⟨v, hv⟩ sch.frz_ty]
  simpa [hVV hcV] using hv

/-- **Frozen facts of a created node agree** at `V` and at every `V' ⊇ V`. -/
theorem frozen_mono (hH : Hist delta ctx) (hF : ∀ d, FrozenOK sch delta ctx d) {X X' : Finset Dot}
    (hVV : X ⊆ X') {n : N} (h : Created sch (stateAt delta X) n) {p : Path Seg} (hp : sch.frz p)
    (v : V) : stateAt delta X (n, p, v) = stateAt delta X' (n, p, v) := by
  obtain ⟨c, hcV, hc⟩ := creator_of_created h
  rw [stateAt_frozen hH hF hc hp, stateAt_frozen hH hF hc hp, ]
  simp [hcV, hVV hcV]

/-- **A permanent node is never deleted**, at any version. -/
theorem perm_not_deleted (hP : ∀ d, PermOK sch delta d) {n : N} (hn : sch.perm n)
    (X : Finset Dot) : ¬ Deleted sch (stateAt delta X) n := by
  rintro ⟨v, hv⟩
  apply hv
  simp only [stateAt, Finsupp.finsetSum_apply]
  exact Finset.sum_eq_zero (fun d _ => hP d n v hn)

/-- **A permanent lifecycle never retracts**: present at `V`, present at every `V' ⊇ V`. -/
theorem present_mono (hH : Hist delta ctx) (hF : ∀ d, FrozenOK sch delta ctx d)
    (hP : ∀ d, PermOK sch delta d) {n : N} (hn : sch.perm n) {X X' : Finset Dot}
    (hVV : X ⊆ X') (h : Present sch (stateAt delta X) n) : Present sch (stateAt delta X') n :=
  ⟨created_mono hH hF hVV h.1, perm_not_deleted hP hn X'⟩

/-! ## Counterexample: "no `−old` on a frozen field" is too weak

Node `()`, segments `false = @type` and `true = f`, where `f` is a frozen field. Entry `0`
creates the node with `f` unset: `+(@type)`. Entry `1`, with entry `0` in its context, adds
`+(f, ())`. It removes nothing, so it passes the weak rule, which forbids only `−old` on a frozen
fact of an existing node. But `f`'s value at `{0, 1}` differs from its value at creation. The
frozen check (`FrozenOK`) rejects entry `1`, because the node already exists at its context. -/

/-- The schema: `@type = false`, `f = true`, both frozen. -/
def wkSch : Schema Unit Bool where
  ty := false
  del := false
  frz _ := True
  perm _ := False
  frz_ty := trivial

def wkDelta : ℕ → ZSet (Fact Unit Bool Unit)
  | 0 => Finsupp.single ((), [false], ()) 1
  | 1 => Finsupp.single ((), [true], ()) 1
  | _ => 0

def wkCtx : ℕ → Finset ℕ
  | 1 => {0}
  | _ => ∅

/-- The design's literal rule for frozen fields: no negative weight on a frozen fact of a node
that exists at the entry's context. -/
def WeakFrozenOK {N Seg V Dot : Type*} (sch : Schema N Seg) (delta : Dot → ZSet (Fact N Seg V))
    (ctx : Dot → Finset Dot) (d : Dot) : Prop :=
  ∀ n p v, sch.frz p → delta d (n, p, v) < 0 → ¬ Created sch (stateAt delta (ctx d)) n

theorem weakFrozen_unsound :
    -- both entries pass the weak rule
    (∀ d, WeakFrozenOK wkSch wkDelta wkCtx d) ∧
    -- entry 0 creates the node
    Created wkSch (wkDelta 0) () ∧
    -- `f` is unset at creation, and set at `{0, 1}`
    stateAt wkDelta (insert 0 (wkCtx 0)) ((), [true], ()) = 0 ∧
    stateAt wkDelta {0, 1} ((), [true], ()) = 1 ∧
    -- the frozen check rejects entry 1
    ¬ FrozenOK wkSch wkDelta wkCtx 1 := by
  refine ⟨?_, ⟨(), by simp [wkDelta, wkSch]⟩, ?_, ?_, ?_⟩
  · intro d n p v _ hneg
    exfalso
    match d with
    | 0 =>
      simp only [wkDelta, Finsupp.single_apply] at hneg
      split_ifs at hneg <;> omega
    | 1 =>
      simp only [wkDelta, Finsupp.single_apply] at hneg
      split_ifs at hneg <;> omega
    | _ + 2 => simp [wkDelta] at hneg
  · simp [stateAt, wkCtx, wkDelta]
  · simp [stateAt, wkDelta]
  · intro h
    have := (h () [true] () trivial (by simp [wkDelta])).1
    exact this ⟨(), by simp [stateAt, wkCtx, wkDelta, wkSch]⟩

end Disc
end Shep
