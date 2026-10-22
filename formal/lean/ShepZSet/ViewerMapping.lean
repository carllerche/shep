/-
# Actors, viewers, and `can_op` (design.md §6.3, §6.8)

A request is `(app, actor)`. Each schema declares a viewer type. An actor of that type is the
viewer; another actor type maps to a viewer with one declaration, `viewer from T = <expr>`.
Deleted actors are rejected before any policy runs, and a mapping to a deleted node yields no
viewer. Rules on `viewer` skip when there is no viewer; rules on `app` and `actor` still decide.

## The model

* `Graph`: each node's type and whether it is deleted.
* `Mapping`: per schema, the viewer type and the `viewer from T = expr` declarations, as a
  partial function from actor types to single-valued expressions (`Node → Option Node`).
* `Rule`: a first-match rule with an effect, either on the viewer (skipped without one) or on
  the app and actor only. `evalRules` is first match wins, default deny.
* `canOp`: the deleted-actor check, then the rules of the schema that owns the node, with the
  viewer from that schema's mapping.

## Results

* `viewerOf_unique`: `viewer_of(schema, actor)` relates an actor to at most one viewer, given
  that a schema declares no mapping for its own viewer type (`Mapping.WF`).
  `viewerOf_not_unique` shows that condition is needed.
* `viewerOf_deleted_actor`, `viewerOf_deleted_target`: a deleted actor, or a mapping that
  reaches a deleted node, yields no viewer.
* `canOp_deleted_actor`: **`can_op` for a deleted actor is false for every node.**
  `precheck_needed`: without the explicit check, an `app` or `actor` rule could still admit a
  deleted actor, since those rules don't go through the viewer.
* `canOp_deleted_owner`: an actor whose mapping reaches a deleted node is judged with no
  viewer, so it keeps nothing it had through the viewer; under a policy whose allow rules all
  read the viewer, it is denied everything.
-/
import ShepZSet.Basic

noncomputable section
open Classical

namespace Shep
namespace Viewer

variable {Node Ty Schema App : Type*}

/-- Node types and deletion. -/
structure Graph (Node Ty : Type*) where
  ty : Node → Ty
  deleted : Node → Prop

/-- Per schema: its viewer type, and its `viewer from T = expr` declarations (at most one per
actor type, each expression yielding at most one node). -/
structure Mapping (Node Ty Schema : Type*) where
  viewerType : Schema → Ty
  mapFor : Schema → Ty → Option (Node → Option Node)

/-- A schema declares no mapping for its own viewer type: an actor of that type is the
viewer. -/
def Mapping.WF (m : Mapping Node Ty Schema) : Prop :=
  ∀ s, m.mapFor s (m.viewerType s) = none

/-- The relation `viewer_of(schema, actor, viewer)`. -/
def ViewerOf (g : Graph Node Ty) (m : Mapping Node Ty Schema) (s : Schema) (a v : Node) :
    Prop :=
  ¬ g.deleted a ∧ ¬ g.deleted v ∧
    ((g.ty a = m.viewerType s ∧ v = a) ∨ ∃ e, m.mapFor s (g.ty a) = some e ∧ e a = some v)

variable {g : Graph Node Ty} {m : Mapping Node Ty Schema} {s : Schema}

/-- **At most one viewer.** -/
theorem viewerOf_unique (hm : m.WF) {a v w : Node} (h₁ : ViewerOf g m s a v)
    (h₂ : ViewerOf g m s a w) : v = w := by
  rcases h₁ with ⟨-, -, ⟨ht, rfl⟩ | ⟨e, he, hv⟩⟩ <;>
    rcases h₂ with ⟨-, -, ⟨ht', rfl⟩ | ⟨e', he', hw⟩⟩
  · rfl
  · rw [ht, hm] at he'; cases he'
  · rw [ht', hm] at he; cases he
  · rw [he] at he'
    cases he'
    rw [hv] at hw
    exact Option.some_inj.mp hw

/-- Without `Mapping.WF`, an actor of the viewer type could also map to a second viewer. -/
theorem viewerOf_not_unique :
    let g : Graph Bool Unit := ⟨fun _ => (), fun _ => False⟩
    let m : Mapping Bool Unit Unit := ⟨fun _ => (), fun _ _ => some (fun _ => some true)⟩
    ViewerOf g m () false false ∧ ViewerOf g m () false true := by
  intro g m
  exact ⟨⟨id, id, Or.inl ⟨rfl, rfl⟩⟩, ⟨id, id, Or.inr ⟨_, rfl, rfl⟩⟩⟩

/-- `viewer_of` as a function. -/
def viewerOf (g : Graph Node Ty) (m : Mapping Node Ty Schema) (s : Schema) (a : Node) :
    Option Node :=
  if h : ∃ v, ViewerOf g m s a v then some h.choose else none

theorem viewerOf_eq_some_iff (hm : m.WF) {a v : Node} :
    viewerOf g m s a = some v ↔ ViewerOf g m s a v := by
  unfold viewerOf
  split_ifs with h
  · constructor
    · intro he; rw [← Option.some_inj.mp he]; exact h.choose_spec
    · intro hv; rw [viewerOf_unique hm h.choose_spec hv]
  · simp only [false_iff]
    exact fun hv => h ⟨v, hv⟩

theorem viewerOf_eq_none {a : Node} (h : ∀ v, ¬ ViewerOf g m s a v) :
    viewerOf g m s a = none := by
  unfold viewerOf
  rw [dite_eq_right_iff]
  intro hx; exact absurd hx.choose_spec (h _)

/-- **A deleted actor has no viewer.** -/
theorem viewerOf_deleted_actor {a : Node} (hd : g.deleted a) : viewerOf g m s a = none :=
  viewerOf_eq_none (fun _ hv => hv.1 hd)

/-- **A mapping that reaches a deleted node yields no viewer.** -/
theorem viewerOf_deleted_target (hm : m.WF) {a o : Node} {e : Node → Option Node}
    (he : m.mapFor s (g.ty a) = some e) (hea : e a = some o) (hd : g.deleted o) :
    viewerOf g m s a = none := by
  apply viewerOf_eq_none
  rintro v ⟨-, hv, ⟨ht, rfl⟩ | ⟨e', he', hv'⟩⟩
  · rw [ht, hm] at he; cases he
  · rw [he] at he'
    cases he'
    rw [hea] at hv'
    cases hv'
    exact hv hd

/-! ## Rules and `can_op` -/

inductive Effect
  | allow
  | deny

/-- A rule: on the viewer (`app, actor, viewer, node`), skipped when there is no viewer; or on
the app and actor only (`app, actor, node`). -/
inductive Rule (App Node : Type*)
  | onViewer (eff : Effect) (p : App → Option Node → Node → Node → Prop)
  | onOther (eff : Effect) (p : App → Option Node → Node → Prop)

def Rule.fire : Rule App Node → App → Option Node → Option Node → Node → Option Effect
  | .onViewer e p, app, actor, some v, n => if p app actor v n then some e else none
  | .onViewer _ _, _, _, none, _ => none
  | .onOther e p, app, actor, _, n => if p app actor n then some e else none

/-- First match wins; no match denies. -/
def evalRules : List (Rule App Node) → App → Option Node → Option Node → Node → Prop
  | [], _, _, _, _ => False
  | r :: rs, app, actor, viewer, n =>
    match r.fire app actor viewer n with
    | some .allow => True
    | some .deny => False
    | none => evalRules rs app actor viewer n

/-- `can_op(app, actor, node)`: reject a deleted actor before any policy runs; otherwise run
the rules of the schema that owns the node, with the viewer from that schema's mapping. The
app acting as itself has no actor and no viewer. -/
def canOp (g : Graph Node Ty) (m : Mapping Node Ty Schema) (schemaOf : Node → Schema)
    (pol : Schema → List (Rule App Node)) (app : App) (actor : Option Node) (n : Node) : Prop :=
  match actor with
  | none => evalRules (pol (schemaOf n)) app none none n
  | some a => ¬ g.deleted a ∧
      evalRules (pol (schemaOf n)) app (some a) (viewerOf g m (schemaOf n) a) n

/-- **`can_op` for a deleted actor is false for every node**, under every policy, for every
app. -/
theorem canOp_deleted_actor (schemaOf : Node → Schema) (pol : Schema → List (Rule App Node))
    (app : App) {a : Node} (hd : g.deleted a) (n : Node) :
    ¬ canOp g m schemaOf pol app (some a) n :=
  fun h => h.1 hd

/-- **The explicit check is needed.** A deleted actor has no viewer, but a rule on the app
alone (`allow if app == slack`) would still admit it if the rules ran. -/
theorem precheck_needed :
    let g : Graph Unit Unit := ⟨fun _ => (), fun _ => True⟩
    let m : Mapping Unit Unit Unit := ⟨fun _ => (), fun _ _ => none⟩
    let rules : List (Rule Unit Unit) := [.onOther .allow (fun _ _ _ => True)]
    g.deleted () ∧ viewerOf g m () () = none ∧ evalRules rules () (some ()) none () := by
  intro g m rules
  refine ⟨trivial, viewerOf_deleted_actor trivial, ?_⟩
  simp [rules, evalRules, Rule.fire]

/-- Every allow rule reads the viewer. -/
def AllowsOnlyViaViewer (rules : List (Rule App Node)) : Prop :=
  ∀ r ∈ rules, ∀ p, r ≠ .onOther .allow p

/-- With no viewer, a policy whose allow rules all read the viewer allows nothing. -/
theorem evalRules_no_viewer {rules : List (Rule App Node)} (h : AllowsOnlyViaViewer rules)
    (app : App) (actor : Option Node) (n : Node) : ¬ evalRules rules app actor none n := by
  induction rules with
  | nil => simp [evalRules]
  | cons r rs ih =>
    have ih := ih (fun r' hr' => h r' (List.mem_cons_of_mem _ hr'))
    cases r with
    | onViewer e p => simpa [evalRules, Rule.fire] using ih
    | onOther e p =>
      cases e with
      | allow => exact absurd rfl (h _ List.mem_cons_self p)
      | deny => simp only [evalRules, Rule.fire]; split_ifs <;> simp [ih]

/-- **An actor whose mapping reaches a deleted node keeps nothing it had through the viewer.**
It is judged with no viewer, exactly as a request with no viewer; under a policy whose allow
rules all read the viewer, it can do nothing. -/
theorem canOp_deleted_owner (hm : m.WF) (schemaOf : Node → Schema)
    (pol : Schema → List (Rule App Node)) (app : App) {a o n : Node}
    {e : Node → Option Node} (ha : ¬ g.deleted a)
    (he : m.mapFor (schemaOf n) (g.ty a) = some e) (hea : e a = some o) (hd : g.deleted o) :
    (canOp g m schemaOf pol app (some a) n ↔ evalRules (pol (schemaOf n)) app (some a) none n) ∧
      (AllowsOnlyViaViewer (pol (schemaOf n)) → ¬ canOp g m schemaOf pol app (some a) n) := by
  have hv : viewerOf g m (schemaOf n) a = none := viewerOf_deleted_target hm he hea hd
  have key : canOp g m schemaOf pol app (some a) n ↔
      evalRules (pol (schemaOf n)) app (some a) none n := by
    simp only [canOp, hv, ha, not_false_eq_true, true_and]
  exact ⟨key, fun hall h => evalRules_no_viewer hall app (some a) n (key.mp h)⟩

end Viewer
end Shep
