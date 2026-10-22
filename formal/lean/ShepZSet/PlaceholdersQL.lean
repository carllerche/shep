/-
# Placeholders read as absent in ShepQL (design.md §3 "Placeholders", §4.0 item 4, §6.5)

Restatement-round obligation 9 (§18). `Placeholders.query_placeholder_eq_absent` assumes that a
query reads the graph only through what readers can see: the readable facts (`raw`), the
edges, and the viewer's `visible`. That is the premise "nothing reads `@placeholder`". We
re-establish it for ShepQL's constructs: key casts, paths, `.__type__`, `exists`, field reads,
and `shep::visible`, in both read contexts (§4.0): operations read through visibility, and
policies read raw data.

## The model

* `evalQ`: the semantics of a small ShepQL fragment, set-valued, in a read context:
  - `keyCast k` (`<T>$k`): `{k}` if `k` is readable, else `{}`; in operations "readable" is
    `visible` (§4.0 item 4: `{}` for a key that's missing, hidden, deleted, or a placeholder);
  - `hop e` (`e.l`): the targets of `e`'s nodes that are readable; in policies "readable" means
    the target has data (`raw ≠ none`), since "every path read treats a placeholder as `{}`"
    (§3);
  - `type e` (`e.__type__`): a node's type when it has its `@type` fact, else `{}`;
  - `data e` (a field read): the node's data, from `raw`;
  - `ex e` (`exists e`), `visible e` (`shep::visible`).
* `evalQ` takes only `raw`, the edges, and `visible`: each construct is defined from what
  readers see. The grounding lemmas show those definitions are what the fact encoding gives.

## Results

* `evalQ_placeholder_eq_absent`: **every ShepQL query in the fragment returns the same result
  for a placeholder as for an absent key**, in both read contexts, for every viewer and
  `include_deleted` (by `query_placeholder_eq_absent`).
* `typeFact_eq`, `readerView_typeFact`: **`.__type__` is a function of `raw`**: in the fact
  encoding, a node's `@type` fact is present exactly when it has readable data (live or
  deleted), and a placeholder has none (it has `@placeholder`, which `readerView` drops). So
  defining `.__type__` by the `@type` fact never reads `@placeholder`.
* `keyCast_placeholder`: a key cast of a placeholder is `{}`, in operations, as for an absent key;
  so `assert_exists` raises `not_found` for both.
* `hop_placeholder_policy`: in a policy, a hop to a placeholder is `{}`, even though the edge is
  stored (so a `required` link to a `shep::Placeholders` type is AtMostOne in policies;
  `Totality.placeholder_hop_not_total`).
* `lifecycleExists_distinguishes`: **the premise is needed**: an `exists` that tested for *any*
  lifecycle fact of a key (`@type` or `@placeholder`) would tell a placeholder from an absent key.
-/
import ShepZSet.Placeholders

noncomputable section
open Classical

namespace Shep
namespace Placeholder
namespace QL

variable {N D Vw T : Type*}

/-- Values a query can return. -/
inductive Val (N D T : Type*)
  | node (n : N)
  | ty (t : T)
  | data (d : D)
  | bool (b : Bool)

/-- A fragment of ShepQL over one edge type. -/
inductive PE (N : Type*)
  /-- `<T>$k` -/
  | keyCast (k : N)
  /-- `.`, the node a policy is evaluated on -/
  | self (n : N)
  /-- `e.l` -/
  | hop (e : PE N)
  /-- `e.__type__` -/
  | type (e : PE N)
  /-- a field read `e.f` -/
  | data (e : PE N)
  /-- `exists e` -/
  | ex (e : PE N)
  /-- `shep::visible(e)` -/
  | visible (e : PE N)

/-- The two read contexts (§4.0). -/
inductive Ctx
  | ops
  | policy

/-- Whether a node is readable in a context: through visibility in operations, raw (it has
data) in policies. -/
def readable (c : Ctx) (raw : N → Option (D × Bool)) (vis : N → Prop) (n : N) : Prop :=
  match c with
  | .ops => vis n
  | .policy => (raw n).isSome

/-- **The fragment's semantics**, from what readers see: readable facts, edges, `visible`. -/
def evalQ (ty : N → T) (c : Ctx) (raw : N → Option (D × Bool)) (edges : N → Finset N)
    (vis : N → Prop) : PE N → List (Val N D T)
  | .keyCast k => if readable c raw vis k then [.node k] else []
  | .self n => [.node n]
  | .hop e => (evalQ ty c raw edges vis e).flatMap (fun x => match x with
      | .node n => ((edges n).toList.filter (fun m => decide (readable c raw vis m))).map .node
      | _ => [])
  | .type e => (evalQ ty c raw edges vis e).flatMap (fun x => match x with
      | .node n => if (raw n).isSome then [.ty (ty n)] else []
      | _ => [])
  | .data e => (evalQ ty c raw edges vis e).flatMap (fun x => match x with
      | .node n => (raw n).elim [] (fun dd => [.data dd.1])
      | _ => [])
  | .ex e => [.bool !(evalQ ty c raw edges vis e).isEmpty]
  | .visible e => [.bool (decide (∃ x ∈ evalQ ty c raw edges vis e, ∃ n, x = .node n ∧ vis n))]

/-- **Every query in the fragment treats a placeholder exactly like an absent key**, in both
read contexts. -/
theorem evalQ_placeholder_eq_absent (ty : N → T) (c : Ctx) (pol : Policy Vw N D)
    {s : GState N D} {n : N} (h : s.life n = .placeholder) (v : Vw) (incl : Bool) (e : PE N) :
    evalQ ty c (s.setLife n .absent).raw (s.setLife n .absent).edges
        (visible pol (s.setLife n .absent) v incl) e =
      evalQ ty c s.raw s.edges (visible pol s v incl) e :=
  query_placeholder_eq_absent pol h v incl (fun raw edges vis => evalQ ty c raw edges vis e)

/-- **A key cast of a placeholder is `{}`** in operations, as for an absent key. -/
theorem keyCast_placeholder (ty : N → T) (pol : Policy Vw N D) {s : GState N D} {n : N}
    (h : s.life n = .placeholder) (v : Vw) (incl : Bool) :
    evalQ ty .ops s.raw s.edges (visible pol s v incl) (.keyCast n) = ([] : List (Val N D T)) ∧
      evalQ ty .ops (s.setLife n .absent).raw (s.setLife n .absent).edges
        (visible pol (s.setLife n .absent) v incl) (.keyCast n) = ([] : List (Val N D T)) := by
  have h1 : ¬ visible pol s v incl n := visible_placeholder pol h v incl
  have h2 : ¬ visible pol (s.setLife n .absent) v incl n := by
    simp [visible, GState.setLife]
  simp [evalQ, readable, h1, h2]

/-- **In a policy, a hop to a placeholder is `{}`**, though the edge is stored. -/
theorem hop_placeholder_policy (ty : N → T) (vis : N → Prop) {s : GState N D} {x n : N}
    (h : s.life n = .placeholder) (he : s.edges x = {n}) :
    evalQ ty .policy s.raw s.edges vis (.hop (.self x)) = ([] : List (Val N D T)) := by
  simp [evalQ, readable, he, GState.raw, h, Life.read]

/-! ## Grounding `.__type__` in the fact encoding -/

section Grounding

variable {Seg FV : Type*} (e : Enc N D Seg FV)

/-- What `.__type__` needs of the encoding: the reserved paths differ, and a node's data facts
sit under that node and never at `@type`. -/
def TyWF : Prop :=
  e.tySeg ≠ e.phSeg ∧ e.tySeg ≠ e.delSeg ∧
    ∀ m d x, e.data m d x ≠ 0 → x.1 = m ∧ x.2.1 ≠ [e.tySeg]

theorem encodeLife_typeFact (he : TyWF e) (m n : N) (l : Life D) :
    e.encodeLife m l (n, [e.tySeg], e.tyV) =
      if m = n ∧ (l.read).isSome then 1 else 0 := by
  have hdata : ∀ d, e.data m d (n, [e.tySeg], e.tyV) = 0 := by
    intro d
    by_contra hne
    exact (he.2.2 m d _ hne).2 rfl
  cases l with
  | absent => simp [Enc.encodeLife, Life.read]
  | placeholder =>
    have : e.phFact m (n, [e.tySeg], e.tyV) = 0 := by
      rw [Enc.phFact, Finsupp.single_eq_of_ne]
      intro hx
      simp only [Prod.mk.injEq, List.cons.injEq, and_true] at hx
      exact he.1 hx.2.1
    simp [Enc.encodeLife, Life.read, this]
  | live d =>
    simp only [Enc.encodeLife, Finsupp.add_apply, hdata, add_zero, Life.read,
      Option.isSome_some, and_true]
    by_cases hmn : m = n
    · subst hmn; simp
    · rw [Finsupp.single_eq_of_ne (by simp [Ne.symm hmn])]; simp [hmn]
  | deleted d =>
    simp only [Enc.encodeLife, Finsupp.add_apply, hdata, add_zero, Life.read,
      Option.isSome_some, and_true]
    rw [Finsupp.single_eq_of_ne (a := (m, [e.delSeg], e.delV))
      (by intro hx; simp only [Prod.mk.injEq, List.cons.injEq, and_true] at hx;
          exact he.2.1 hx.2.1), add_zero]
    by_cases hmn : m = n
    · subst hmn; simp
    · rw [Finsupp.single_eq_of_ne (by simp [Ne.symm hmn])]; simp [hmn]

/-- **A node's `@type` fact is present exactly when it has readable data**: live or deleted,
not a placeholder, not absent. -/
theorem typeFact_eq [Fintype N] (he : TyWF e) (s : GState N D) (n : N) :
    e.encode s (n, [e.tySeg], e.tyV) = if (s.raw n).isSome then 1 else 0 := by
  simp only [Enc.encode, Finsupp.finsetSum_apply, encodeLife_typeFact e he]
  rw [Finset.sum_eq_single n]
  · by_cases hr : ((s.life n).read).isSome = true <;> simp [GState.raw, hr]
  · intro m _ hm; simp [hm]
  · intro h; exact absurd (Finset.mem_univ n) h

/-- Readers see the `@type` fact unchanged: `readerView` drops only `@placeholder`. -/
theorem readerView_typeFact [Fintype N] (he : TyWF e) (s : GState N D) (n : N) :
    e.readerView (e.encode s) (n, [e.tySeg], e.tyV) = if (s.raw n).isSome then 1 else 0 := by
  have hc : ([e.tySeg] : List Seg) ≠ [e.phSeg] := by
    intro hx
    simp only [List.cons.injEq, and_true] at hx
    exact he.1 hx
  rw [Enc.readerView, zfilter, Finsupp.filter_apply]
  simp only [ne_eq, hc, not_false_eq_true, ↓reduceIte]
  exact typeFact_eq e he s n

end Grounding

/-! ## Counterexample: an `exists` that reads any lifecycle fact -/

/-- "The key has some lifecycle": what an `exists` reading `@placeholder` would compute. -/
def lifecycleExists (s : GState N D) (n : N) : Prop := s.life n ≠ .absent

theorem lifecycleExists_distinguishes :
    let s : GState Unit Unit := ⟨fun _ => .placeholder, fun _ => ∅⟩
    lifecycleExists s () ∧ ¬ lifecycleExists (s.setLife () .absent) () := by
  intro s
  simp [lifecycleExists, s, GState.setLife]

end QL
end Placeholder
end Shep
