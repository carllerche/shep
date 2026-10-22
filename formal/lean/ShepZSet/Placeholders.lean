/-
# Placeholders (design.md §3 "Placeholders", §6.5, §10.1)

A node is absent, a placeholder (linked to before it exists), live, or deleted. A link to a
missing key on a placeholder-enabled type creates a placeholder; an ordinary create fills it.

## The model

* `GState`: each node's lifecycle (`Life`) and each node's out-edges.
* **What readers can read** (`GState.raw`): a node's data and whether it is deleted. A
  placeholder has no data, and the `@placeholder` marker is not readable, so `raw` gives
  `none` for a placeholder exactly as for an absent node. Read policies are functions of
  `raw` and the edges (`Policy`): they read graph facts only.
* `visible` follows §6.5: `!placeholder && policy_allows && (deleted_at is null || include_deleted)`,
  and an absent node is never visible.
* A **query** is any function of what a viewer can see: `raw`, the edges, and the viewer's
  `visible` predicate. That includes every list, count, `One<T>` hop, and walk (`Walks.lean`).
* In the Z-set fact encoding (§10.1), a placeholder is the fact `(n, @placeholder)`, and
  readers see base facts through `readerView`, which drops `@placeholder` facts.

## Results

* `visible_placeholder`: a placeholder is never visible, to any viewer, even with
  `include_deleted`.
* `visible_placeholder_eq_absent`, `query_placeholder_eq_absent`: replacing a placeholder by an
  absent node changes nothing any viewer's query can compute. `walk_placeholder_eq_absent` and
  `count_placeholder_eq_absent` are instances.
* `create_placeholder`, `create_equal`, `create_conflict`: fill semantics. A create on a
  placeholder fills it; on a live node with equal data it is a no-op (empty delta, state
  unchanged); with different data it is a `conflict` (state unchanged).
* `create_placeholder_eq_absent`: the writer sees the same response, and the same state
  results, whether the key was a placeholder or absent.
* `create_delta_correct`: each create's logged delta takes the encoded state to the encoded
  new state.
* `readerView_fillDelta`, `fill_changes_eq_create`, `fill_changes_linear`: in change queries,
  a fill produces the same delta as creating the node fresh.
-/
import ShepZSet.Facts
import ShepZSet.Walks

noncomputable section
open Classical

namespace Shep
namespace Placeholder

/-- A node's lifecycle state. -/
inductive Life (D : Type*)
  | absent
  | placeholder
  | live (d : D)
  | deleted (d : D)

variable {N D Vw : Type*}

/-- What a reader or a policy can read about a node: its data, and whether it is deleted. -/
def Life.read : Life D → Option (D × Bool)
  | .live d => some (d, false)
  | .deleted d => some (d, true)
  | .absent => none
  | .placeholder => none

/-- A graph state: lifecycles and out-edges (of the walked or policy-relevant edge type). -/
structure GState (N D : Type*) where
  life : N → Life D
  edges : N → Finset N

/-- The readable facts of every node. -/
def GState.raw (s : GState N D) : N → Option (D × Bool) := fun n => (s.life n).read

/-- Change one node's lifecycle. -/
def GState.setLife (s : GState N D) (n : N) (l : Life D) : GState N D :=
  { s with life := Function.update s.life n l }

/-- A read policy: deterministic, reading graph facts only (node data and deletion, edges). -/
abbrev Policy (Vw N D : Type*) := Vw → N → (N → Option (D × Bool)) → (N → Finset N) → Prop

/-- §6.5: `visible(node) = !placeholder && policy_allows(read, node) &&
(deleted_at is null || include_deleted)`; an absent node is never visible. -/
def visible (pol : Policy Vw N D) (s : GState N D) (v : Vw) (incl : Bool) (n : N) : Prop :=
  match s.life n with
  | .absent => False
  | .placeholder => False
  | .live _ => pol v n s.raw s.edges
  | .deleted _ => pol v n s.raw s.edges ∧ incl = true

/-- **A placeholder is never visible**, to any viewer, even with `include_deleted`. -/
theorem visible_placeholder (pol : Policy Vw N D) {s : GState N D} {n : N}
    (h : s.life n = .placeholder) (v : Vw) (incl : Bool) : ¬ visible pol s v incl n := by
  simp [visible, h]

theorem raw_placeholder_eq_absent {s : GState N D} {n : N} (h : s.life n = .placeholder) :
    (s.setLife n .absent).raw = s.raw := by
  funext m
  by_cases hm : m = n
  · subst hm; simp [GState.raw, GState.setLife, h, Life.read]
  · simp [GState.raw, GState.setLife, Function.update_of_ne hm]

/-- **`visible` treats a placeholder exactly like an absent node**, for every viewer and
both settings of `include_deleted`: replacing the placeholder by an absent node leaves every
node's visibility unchanged (the placeholder's own, and every other node's, whose policy may
read the placeholder's facts and edges). -/
theorem visible_placeholder_eq_absent (pol : Policy Vw N D) {s : GState N D} {n : N}
    (h : s.life n = .placeholder) (v : Vw) (incl : Bool) :
    visible pol (s.setLife n .absent) v incl = visible pol s v incl := by
  funext m
  have hraw := raw_placeholder_eq_absent h
  by_cases hm : m = n
  · subst hm; simp [visible, GState.setLife, h]
  · have : (s.setLife n .absent).life m = s.life m := Function.update_of_ne hm _ _
    simp only [visible, this, hraw]
    rfl

/-- **Every query treats a placeholder exactly like an absent node.** A query sees the graph
through the readable facts, the edges, and the viewer's visibility predicate; any such query
gives the same answer, for every viewer, when a placeholder is replaced by an absent node. -/
theorem query_placeholder_eq_absent {β : Type*} (pol : Policy Vw N D) {s : GState N D} {n : N}
    (h : s.life n = .placeholder) (v : Vw) (incl : Bool)
    (Q : (N → Option (D × Bool)) → (N → Finset N) → (N → Prop) → β) :
    Q (s.setLife n .absent).raw (s.setLife n .absent).edges
        (visible pol (s.setLife n .absent) v incl) =
      Q s.raw s.edges (visible pol s v incl) := by
  rw [raw_placeholder_eq_absent h, visible_placeholder_eq_absent pol h]
  rfl

/-- Walks stop at a placeholder exactly as at an absent node. -/
theorem walk_placeholder_eq_absent (pol : Policy Vw N D) {s : GState N D} {n : N}
    (h : s.life n = .placeholder) (v : Vw) (incl : Bool) (roots : Finset N) (d : ℕ) :
    Walk.walk (s.setLife n .absent).edges (visible pol (s.setLife n .absent) v incl) roots d =
      Walk.walk s.edges (visible pol s v incl) roots d :=
  query_placeholder_eq_absent pol h v incl (fun _ E vis => Walk.walk E vis roots d)

/-- Counts don't see placeholders either. -/
theorem count_placeholder_eq_absent (pol : Policy Vw N D) {s : GState N D} {n : N}
    (h : s.life n = .placeholder) (v : Vw) (incl : Bool) (cands : Finset N) :
    (cands.filter (visible pol (s.setLife n .absent) v incl)).card =
      (cands.filter (visible pol s v incl)).card :=
  query_placeholder_eq_absent pol h v incl (fun _ _ vis => (cands.filter vis).card)

/-! ## Fill semantics -/

/-- The outcome of an ordinary create of key `n` on a placeholder-enabled type. -/
inductive Outcome (N D : Type*)
  | created (s : GState N D)
  | filled (s : GState N D)
  | noop
  | conflict
  | deletedErr

/-- What the writer is told: a content-addressed create returns only the key. -/
inductive Response
  | ok
  | conflict
  | deleted
  deriving DecidableEq

def create (s : GState N D) (n : N) (d : D) : Outcome N D :=
  match s.life n with
  | .absent => .created (s.setLife n (.live d))
  | .placeholder => .filled (s.setLife n (.live d))
  | .live d' => if d' = d then .noop else .conflict
  | .deleted _ => .deletedErr

def Outcome.response : Outcome N D → Response
  | .created _ => .ok
  | .filled _ => .ok
  | .noop => .ok
  | .conflict => .conflict
  | .deletedErr => .deleted

/-- The state after the outcome. -/
def Outcome.state (s : GState N D) : Outcome N D → GState N D
  | .created s' => s'
  | .filled s' => s'
  | _ => s

/-- **A create on a placeholder fills it**: the node becomes live with the given data. -/
theorem create_placeholder {s : GState N D} {n : N} (h : s.life n = .placeholder) (d : D) :
    create s n d = .filled (s.setLife n (.live d)) ∧
      (s.setLife n (.live d)).life n = .live d := by
  simp [create, h, GState.setLife]

/-- **A create with equal data is a no-op**: the state is unchanged. -/
theorem create_equal {s : GState N D} {n : N} {d : D} (h : s.life n = .live d) :
    create s n d = .noop ∧ (create s n d).state s = s ∧ (create s n d).response = .ok := by
  simp [create, h, Outcome.state, Outcome.response]

/-- **A create with different data is a conflict**: the state is unchanged. -/
theorem create_conflict {s : GState N D} {n : N} {d d' : D} (h : s.life n = .live d')
    (hne : d' ≠ d) :
    create s n d = .conflict ∧ (create s n d).state s = s ∧
      (create s n d).response = .conflict := by
  simp [create, h, hne, Outcome.state, Outcome.response]

/-- **The writer can't tell a fill from a create**: same response, same resulting state. -/
theorem create_placeholder_eq_absent {s : GState N D} {n : N} (h : s.life n = .placeholder)
    (d : D) :
    (create (s.setLife n .absent) n d).response = (create s n d).response ∧
      (create (s.setLife n .absent) n d).state (s.setLife n .absent) =
        (create s n d).state s := by
  simp [create, h, Outcome.response, Outcome.state, GState.setLife]

/-! ## Fill deltas in change queries -/

variable {Seg Val : Type*}

/-- The fact encoding of lifecycles (§10.1): `(n, @placeholder)` while a placeholder,
`(n, @type)` plus the node's data facts while live, plus `(n, @deleted_at, t)` while
deleted. -/
structure Enc (N D Seg Val : Type*) where
  phSeg : Seg
  tySeg : Seg
  delSeg : Seg
  phV : Val
  tyV : Val
  delV : Val
  data : N → D → ZSet (Fact N Seg Val)

namespace Enc

variable (e : Enc N D Seg Val)

/-- Data facts never sit at the reserved `@placeholder` path, and `@type` is not
`@placeholder`. -/
def WF : Prop := e.tySeg ≠ e.phSeg ∧ ∀ n d x, e.data n d x ≠ 0 → x.2.1 ≠ [e.phSeg]

def phFact (n : N) : ZSet (Fact N Seg Val) := Finsupp.single (n, [e.phSeg], e.phV) 1

def encodeLife (n : N) : Life D → ZSet (Fact N Seg Val)
  | .absent => 0
  | .placeholder => e.phFact n
  | .live d => Finsupp.single (n, [e.tySeg], e.tyV) 1 + e.data n d
  | .deleted d => Finsupp.single (n, [e.tySeg], e.tyV) 1 + e.data n d +
      Finsupp.single (n, [e.delSeg], e.delV) 1

/-- The encoded state. -/
def encode [Fintype N] (s : GState N D) : ZSet (Fact N Seg Val) :=
  ∑ n, e.encodeLife n (s.life n)

/-- §10.1: creating a node logs `+(n, @type)` plus its data facts. -/
def createDelta (n : N) (d : D) : ZSet (Fact N Seg Val) :=
  Finsupp.single (n, [e.tySeg], e.tyV) 1 + e.data n d

/-- §10.1: filling a placeholder logs `−(n, @placeholder)`, `+(n, @type)`, plus its data. -/
def fillDelta (n : N) (d : D) : ZSet (Fact N Seg Val) := e.createDelta n d - e.phFact n

/-- The delta a create outcome logs. A no-op, a conflict, and an error log nothing. -/
def outcomeDelta (n : N) (d : D) : Outcome N D → ZSet (Fact N Seg Val)
  | .created _ => e.createDelta n d
  | .filled _ => e.fillDelta n d
  | _ => 0

/-- What readers (queries and policies) see of the base facts: everything but `@placeholder`. -/
def readerView (S : ZSet (Fact N Seg Val)) : ZSet (Fact N Seg Val) :=
  zfilter (fun x => x.2.1 ≠ [e.phSeg]) S

theorem encode_setLife [Fintype N] (s : GState N D) (n : N) (l : Life D) :
    e.encode (s.setLife n l) = e.encode s + (e.encodeLife n l - e.encodeLife n (s.life n)) := by
  have : e.encode (s.setLife n l) - e.encode s = e.encodeLife n l - e.encodeLife n (s.life n) := by
    simp only [encode, ← Finset.sum_sub_distrib]
    rw [Finset.sum_eq_single n]
    · simp [GState.setLife]
    · intro m _ hm; simp [GState.setLife, Function.update_of_ne hm]
    · intro h; exact absurd (Finset.mem_univ n) h
  rw [← this]; abel

/-- **Each create logs a correct delta**: applying it to the encoded state gives the encoded
new state. -/
theorem create_delta_correct [Fintype N] (s : GState N D) (n : N) (d : D) :
    e.encode ((create s n d).state s) = e.encode s + e.outcomeDelta n d (create s n d) := by
  unfold create
  cases h : s.life n with
  | absent =>
    simp only [Outcome.state, outcomeDelta, encode_setLife, h, encodeLife, createDelta, sub_zero]
  | placeholder =>
    simp only [Outcome.state, outcomeDelta, encode_setLife, h, encodeLife, fillDelta, createDelta]
  | live d' =>
    by_cases hd : d' = d <;> simp [hd, Outcome.state, outcomeDelta]
  | deleted d' =>
    simp [Outcome.state, outcomeDelta]

theorem readerView_phFact (n : N) : e.readerView (e.phFact n) = 0 := by
  ext x
  rw [readerView, zfilter, Finsupp.filter_apply]
  split_ifs with hx
  · rw [phFact, Finsupp.single_eq_of_ne]
    · rfl
    · rintro rfl; exact hx rfl
  · rfl

theorem readerView_createDelta (he : e.WF) (n : N) (d : D) :
    e.readerView (e.createDelta n d) = e.createDelta n d := by
  ext x
  rw [readerView, zfilter, Finsupp.filter_apply]
  split_ifs with hp
  · rfl
  by_contra hx
  apply hp
  simp only [createDelta, Finsupp.add_apply] at hx
  by_cases hd : e.data n d x = 0
  · rw [hd, add_zero] at hx
    have : x = (n, [e.tySeg], e.tyV) := by
      by_contra hne
      exact hx (Finsupp.single_eq_of_ne hne).symm
    subst this
    simpa using he.1
  · exact he.2 n d x hd

/-- **Readers see a fill as a creation**: the reader view of the fill delta is exactly the
create delta. -/
theorem readerView_fillDelta (he : e.WF) (n : N) (d : D) :
    e.readerView (e.fillDelta n d) = e.createDelta n d := by
  rw [fillDelta, readerView, (zfilter_linear _).map_sub, ← readerView, ← readerView,
    readerView_createDelta e he, readerView_phFact, sub_zero]

/-- **A fill and a fresh create give the same change.** For any query `Q'` over what readers
see, the change across filling the placeholder `n` equals the change across creating `n` in
the state where it was absent. -/
theorem fill_changes_eq_create [Fintype N] {β : Type*} [AddCommGroup β]
    (Q' : ZSet (Fact N Seg Val) → β) {s : GState N D} {n : N} (h : s.life n = .placeholder)
    (d : D) :
    let Q := fun S => Q' (e.readerView S)
    let sAbs := s.setLife n .absent
    Q (e.encode ((create s n d).state s)) - Q (e.encode s) =
      Q (e.encode ((create sAbs n d).state sAbs)) - Q (e.encode sAbs) := by
  intro Q sAbs
  have hafter := (create_placeholder_eq_absent h d).2
  have hbefore : e.readerView (e.encode s) = e.readerView (e.encode sAbs) := by
    have : e.encode sAbs = e.encode s - e.phFact n := by
      simp only [sAbs, encode_setLife, h, encodeLife]; abel
    have hph := e.readerView_phFact n
    rw [this]
    unfold readerView at hph ⊢
    rw [(zfilter_linear _).map_sub, hph, sub_zero]
  simp only [Q, sAbs] at *
  rw [hafter, hbefore]

/-- **The same delta, for linear queries** (DBSP §11.5): the change a linear query `Q'` over
the reader view sees across a fill is `Q'` of the create delta. -/
theorem fill_changes_linear [Fintype N] {β : Type*} {Q' : ZSet (Fact N Seg Val) → ZSet β}
    (hQ : IsLinear Q') (he : e.WF) {s : GState N D} {n : N} (h : s.life n = .placeholder)
    (d : D) :
    changes (fun S => Q' (e.readerView S)) (e.encode s) (e.encode ((create s n d).state s)) =
      Q' (e.createDelta n d) := by
  rw [create_delta_correct e s n d]
  simp only [changes, readerView, (zfilter_linear _) (e.encode s), hQ _ _, add_sub_cancel_left]
  rw [← readerView]
  simp [create, h, outcomeDelta, readerView_fillDelta e he]

/-- A no-op create changes nothing in any change query. -/
theorem noop_changes {β : Type*} [Fintype N] (Q : ZSet (Fact N Seg Val) → ZSet β)
    {s : GState N D} {n : N} {d : D} (h : s.life n = .live d) :
    changes Q (e.encode s) (e.encode ((create s n d).state s)) = 0 ∧
      e.outcomeDelta n d (create s n d) = 0 := by
  rw [(create_equal h).2.1, (create_equal h).1]
  exact ⟨changes_self _ _, rfl⟩

end Enc

end Placeholder
end Shep
