/-
# Apps, actors, viewers, and `can_op` (design.md §6.3, §6.8, §13.2, §16.4)

Supersedes the mapping part of `ViewerMapping.lean`. A request carries three principals: the
calling **app**, the **actor** (who is doing the work), and the **viewer** (whom the request is
for). Credentials, not schemas, fix the actor and the viewer (§6.3), so there is no mapping:
the viewer is part of the request.

## Two rule semantics

The file has two models of the policy language, with the same pre-checks:

* **`UMD` (current, §6.1): union minus deny**, as in Gel and Cedar. A policy is lists of allow and
  deny guards; a request is allowed iff some allow guard holds and no deny guard holds,
  `(∨ A) ∧ ¬ (∨ D)`. Guards are total and two-valued (no runtime errors), and an atom that reads
  an absent viewer or actor is false.
* **First match** (the top-level `Rule`/`evalRules`/`Model`, the earlier §6.1): ordered rules
  returning Allow | Deny | Skip, the first non-Skip deciding, errors as Deny. Kept because its
  theorems are proved and the argument is the same; `UMD` restates each one.

Guard and rule kinds, in both:
  - on the viewer (`allow if viewer in node.members`): reads the app, viewer, and node; false
    (or Skip) without a viewer;
  - on the app alone: an allow (`allow if app == slack`) matches only the app acting as itself,
    with no actor node and no viewer; a deny (`deny if app == legacy`) applies to every request,
    since §6.3 restricts only rules that grant by app alone;
  - on the actor: only in schemas that declare actors (`WF`);
  - `shep::visible(.e)` (formerly `delegate`): the target passes `can_op` for the same request
    (same app, actor, and viewer).

`canOp app actor viewer n` (`actor = none` is the app acting as itself, `viewer = none` is no
viewer) is, in order: the deleted-actor pre-check, the deleted-viewer pre-check, the actors
declaration (`none` = any actor; `some D` = the declared types, `none ∈ D` meaning `app`), the
effective viewer (a viewer not of the schema's viewer type is a Deny for that schema, never a
fall-through to rules on `app`), then the policy. The order matters only for the error a client
sees; the decision is their conjunction. A `visible` depth bound stands in for §6.8's rule that a
recursive `can_read` is materialized or depth-bounded.

## Results (each in both models unless noted; `UMD.Model.*` is the current one)

* `canOp_deleted_actor`, `canOp_deleted_viewer`: a deleted actor or viewer is denied on every
  node, under every policy and app. `precheck_viewer_needed` (first match): soft delete keeps
  edges, so without the viewer pre-check the rules would still allow a deleted viewer.
* `canOp_wrongtype_viewer`: a viewer of another type is denied, whatever the policy, including
  one with app-only allows. `canOp_appOnly_with_viewer`: an app-only allow never decides a request
  that has a viewer, so a policy whose only allows are app-only denies every such request.
  `fallthrough_reaches_app_grant`: under the superseded mapping semantics (no viewer → rules on
  `app` decide), a stray viewer reached an app grant.
* `actorsDecl_enforced` (and `canOp_asApp_undeclared_app`, first match): a schema that declares
  actors denies any other actor, including the app as itself unless `app` is declared.
* `visible_preserves_viewer`: `Message.read: allow if shep::visible(.channel)` admits exactly the
  requests, with the same app, actor, and viewer, that the channel admits.
  `visible_crosstype_denies`: a `visible` into a schema with another viewer type denies.
* **`canOp_actfor_no_amplification`**: if no allow rule reachable through `visible` reads the
  actor, then `canOp app g u n → canOp app u u n`: an agent acting for a user can never do more
  than the user acting directly. Side conditions, each shown necessary by a counterexample:
  - `u` acting as itself passes the reachable actor declarations and is caught by no reachable
    actor-reading deny (`ActorOK`; `actorsDecl_excluding_viewer_amplifies` (first match),
    `actorDeny_on_self_amplifies`);
  - under union minus deny, no reachable deny guard uses `visible` (`NoVisibleDeny`;
    `UMD.Ex.visibleDeny_amplifies`). In first match, `visible` is allow-only by construction.
  `actorAllow_amplifies`: an actor-reading allow (`allow if actor is agents.Agent`) breaks it.
  `canOp_actfor_eq_undeclared`: when no reachable schema declares actors, the agent can do
  exactly what the user can (in `UMD` this holds even with `visible` under a deny).
  `canOpN_actor_mono`: the general form, for any two actors acting for the same viewer.
* **Two-party issuance** (`Issuance`): a user-approved act-for key `(app, g, u)` exists only if
  the app registered `g`, the app requested the key with its own key, and `u` approved it while
  signed in (`actfor_key_two_party`, `actfor_needs_consent`, `actfor_needs_registration`); an
  act-for key can't register, request, approve, or revoke anything (`bound_key_inert`: no
  chaining).
-/
import ShepZSet.ViewerMapping

noncomputable section
open Classical

namespace Shep
namespace Principals

variable {Node Ty Schema App : Type*}

/-! ## First match (the earlier §6.1) -/

/-- A rule body's result. -/
inductive Res
  | yes
  | no
  | err

inductive Effect
  | allow
  | deny

/-- What a body's result does to a rule with effect `e`: match, skip, or (an error) deny. -/
def Res.eff (e : Effect) : Res → Option Effect
  | .yes => some e
  | .no => none
  | .err => some .deny

/-- First-match rules. -/
inductive Rule (App Node : Type*)
  /-- reads the app, the viewer, and the node; skips without a viewer -/
  | onViewer (eff : Effect) (p : App → Node → Node → Res)
  /-- on the app alone: as an allow, matches only the app acting as itself; as a deny, matches
  every request -/
  | onApp (eff : Effect) (p : App → Node → Res)
  /-- reads the actor (only in schemas that declare actors) -/
  | onActor (eff : Effect) (p : App → Option Node → Option Node → Node → Res)
  /-- `allow if shep::visible(.edge)` (formerly `delegate`) -/
  | visible (edge : Node → Option Node)

/-- A rule's result, given `dl`, the decision at a `visible` target. -/
def Rule.fire (dl : Node → Prop) :
    Rule App Node → App → Option Node → Option Node → Node → Option Effect
  | .onViewer e p, app, _, viewer, n =>
    match viewer with
    | some v => (p app v n).eff e
    | none => none
  | .onApp .allow p, app, actor, viewer, n =>
    if actor = none ∧ viewer = none then (p app n).eff .allow else none
  | .onApp .deny p, app, _, _, n => (p app n).eff .deny
  | .onActor e p, app, actor, viewer, n => (p app actor viewer n).eff e
  | .visible f, _, _, _, n =>
    match f n with
    | none => some .deny
    | some t => if dl t then some .allow else none

/-- First match wins; no match denies. -/
def evalRules (dl : Node → Prop) :
    List (Rule App Node) → App → Option Node → Option Node → Node → Prop
  | [], _, _, _, _ => False
  | r :: rs, app, actor, viewer, n =>
    match r.fire dl app actor viewer n with
    | some .allow => True
    | some .deny => False
    | none => evalRules dl rs app actor viewer n

theorem evalRules_cons {dl : Node → Prop} {r : Rule App Node} {rs : List (Rule App Node)}
    {app : App} {actor viewer : Option Node} {n : Node} :
    evalRules dl (r :: rs) app actor viewer n ↔
      (r.fire dl app actor viewer n = some .allow ∨
        (r.fire dl app actor viewer n = none ∧ evalRules dl rs app actor viewer n)) := by
  simp only [evalRules]
  split <;> rename_i h <;> simp [h]

/-- An `actors` declaration admits an actor type (`none` is `app`). -/
def admits : Option (Set (Option Ty)) → Option Ty → Prop
  | none, _ => True
  | some D, t => t ∈ D

/-- Node types and deletion, schemas, and their policies. -/
structure Model (Node Ty Schema App : Type*) where
  ty : Node → Ty
  deleted : Node → Prop
  schemaOf : Node → Schema
  viewerType : Schema → Ty
  /-- `none`: any actor is permitted; `some D`: `actors T₁ | T₂ | …` (`none ∈ D` is `app`) -/
  actors : Schema → Option (Set (Option Ty))
  pol : Schema → List (Rule App Node)
  depth : ℕ

namespace Model

/-- The checks that run before any rule: deleted actor, deleted viewer, the actors declaration,
and the viewer's type (the effective viewer). -/
def Pre (M : Model Node Ty Schema App) (actor viewer : Option Node) (n : Node) : Prop :=
  (∀ a, actor = some a → ¬ M.deleted a) ∧
  (∀ v, viewer = some v → ¬ M.deleted v) ∧
  admits (M.actors (M.schemaOf n)) (actor.map M.ty) ∧
  (∀ v, viewer = some v → M.ty v = M.viewerType (M.schemaOf n))

/-- `can_op` with `k` levels of `visible` left. A `visible` target is judged for the same
request: the same app, actor, and viewer. -/
def canOpN (M : Model Node Ty Schema App) :
    ℕ → App → Option Node → Option Node → Node → Prop
  | 0, _, _, _, _ => False
  | k + 1, app, actor, viewer, n =>
    M.Pre actor viewer n ∧
      evalRules (canOpN M k app actor viewer) (M.pol (M.schemaOf n)) app actor viewer n

/-- `can_op(app, actor, viewer, node)` (§6.8). `actor = none` is the app acting as itself;
`viewer = none` is a request with no viewer (`db.as_app()`). -/
def canOp (M : Model Node Ty Schema App) (app : App) (actor viewer : Option Node) (n : Node) :
    Prop :=
  M.canOpN M.depth app actor viewer n

variable {M : Model Node Ty Schema App}

theorem canOpN_pre {k : ℕ} {app : App} {actor viewer : Option Node} {n : Node}
    (h : M.canOpN k app actor viewer n) : M.Pre actor viewer n := by
  cases k with
  | zero => exact h.elim
  | succ k => exact h.1

/-! ### Pre-checks -/

/-- **A deleted actor is denied on every node**, under every policy, for every app and viewer. -/
theorem canOp_deleted_actor (app : App) {a : Node} (hd : M.deleted a) (viewer : Option Node)
    (n : Node) : ¬ M.canOp app (some a) viewer n :=
  fun h => (canOpN_pre h).1 a rfl hd

/-- **A deleted viewer is denied on every node**, under every policy, for every app and actor:
an agent acting for an offboarded user keeps nothing the user had. -/
theorem canOp_deleted_viewer (app : App) (actor : Option Node) {v : Node} (hd : M.deleted v)
    (n : Node) : ¬ M.canOp app actor (some v) n :=
  fun h => (canOpN_pre h).2.1 v rfl hd

/-- **A viewer of another type is denied**, never judged as "no viewer", so it can't fall through
to rules on `app`: this holds for every policy, including one whose rules are app-only allows. -/
theorem canOp_wrongtype_viewer (app : App) (actor : Option Node) {v : Node} (n : Node)
    (ht : M.ty v ≠ M.viewerType (M.schemaOf n)) : ¬ M.canOp app actor (some v) n :=
  fun h => ht ((canOpN_pre h).2.2.2 v rfl)

/-- **Declared means enforced.** A schema that declares actors denies any actor not of a declared
type, before its rules run. -/
theorem actorsDecl_enforced (app : App) (actor viewer : Option Node) (n : Node)
    {D : Set (Option Ty)} (hD : M.actors (M.schemaOf n) = some D)
    (ha : actor.map M.ty ∉ D) : ¬ M.canOp app actor viewer n := by
  intro h
  have h3 := (canOpN_pre h).2.2.1
  rw [hD] at h3
  exact ha h3

/-- The app acting as itself is an actor too: a schema that declares actors without `app` denies
`db.as_app()`. -/
theorem canOp_asApp_undeclared_app (app : App) (viewer : Option Node) (n : Node)
    {D : Set (Option Ty)} (hD : M.actors (M.schemaOf n) = some D) (ha : (none : Option Ty) ∉ D) :
    ¬ M.canOp app none viewer n :=
  actorsDecl_enforced app none viewer n hD ha

/-! ### App-only rules -/

/-- **An app-only allow decides only the app acting as itself.** With an actor node or a viewer,
it skips. -/
theorem appRule_skips_unless_asApp (dl : Node → Prop) (p : App → Node → Res)
    (app : App) {actor viewer : Option Node} (n : Node) (h : actor ≠ none ∨ viewer ≠ none) :
    (Rule.onApp .allow p).fire dl app actor viewer n = none := by
  have hc : ¬ (actor = none ∧ viewer = none) := fun ⟨h1, h2⟩ => h.elim (· h1) (· h2)
  simp [Rule.fire, hc]

/-- A rule that can't produce Allow: a deny rule of any kind, or an app-only rule (which, for a
request with a viewer, skips). -/
def AllowViaAppOnly : Rule App Node → Prop
  | .onViewer e _ => e = .deny
  | .onApp _ _ => True
  | .onActor e _ => e = .deny
  | .visible _ => False

theorem evalRules_appOnly_with_viewer {dl : Node → Prop} {app : App} {actor : Option Node}
    {v n : Node} : ∀ {rules : List (Rule App Node)}, (∀ r ∈ rules, AllowViaAppOnly r) →
    ¬ evalRules dl rules app actor (some v) n
  | [], _, h => h
  | r :: rs, hall, h => by
    have ih := evalRules_appOnly_with_viewer (dl := dl) (app := app) (actor := actor) (v := v)
      (n := n) (fun r' hr' => hall r' (List.mem_cons_of_mem _ hr'))
    have hr := hall r List.mem_cons_self
    rw [evalRules_cons] at h
    rcases h with h | ⟨-, h⟩
    · cases r with
      | onViewer e p =>
        cases hr
        simp only [Rule.fire] at h
        cases hp : p app v n <;> simp [hp, Res.eff] at h
      | onApp e p =>
        cases e with
        | allow =>
          rw [appRule_skips_unless_asApp dl p app n (Or.inr (by simp))] at h
          cases h
        | deny =>
          simp only [Rule.fire] at h
          cases hp : p app n <;> simp [hp, Res.eff] at h
      | onActor e p =>
        cases hr
        simp only [Rule.fire] at h
        cases hp : p app actor (some v) n <;> simp [hp, Res.eff] at h
      | visible f => exact hr.elim
    · exact ih h

/-- **A policy whose only allows are app-only admits no request that has a viewer**, whatever the
actor. In particular, deleting the viewer of an agent's request (or giving it a stray viewer)
never leaves it to an app-only grant. -/
theorem canOp_appOnly_with_viewer (app : App) (actor : Option Node) (v n : Node)
    (hall : ∀ r ∈ M.pol (M.schemaOf n), AllowViaAppOnly r) : ¬ M.canOp app actor (some v) n := by
  unfold canOp
  cases M.depth with
  | zero => exact id
  | succ k => exact fun h => evalRules_appOnly_with_viewer hall h.2

/-! ### `visible` keeps the viewer -/

/-- **`visible` keeps the request's viewer.** For `Message.read = shep::visible(.channel)`, the
message admits exactly the requests (same app, actor, and viewer) that the channel admits, once
the message's own pre-checks pass. -/
theorem visible_preserves_viewer {k : ℕ} {app : App} {actor viewer : Option Node} {n t : Node}
    {f : Node → Option Node} (hpol : M.pol (M.schemaOf n) = [.visible f]) (hf : f n = some t)
    (hpre : M.Pre actor viewer n) :
    M.canOpN (k + 1) app actor viewer n ↔ M.canOpN k app actor viewer t := by
  simp only [canOpN, hpol, evalRules, Rule.fire, hf, hpre, true_and]
  by_cases h : M.canOpN k app actor viewer t <;> simp [h]

/-- **A `visible` into a schema with another viewer type denies**: the target sees the same
viewer and checks it against its own viewer type. (The checker warns on such a `visible`.) -/
theorem visible_crosstype_denies {k : ℕ} {app : App} {actor : Option Node} {v n t : Node}
    {f : Node → Option Node} (hpol : M.pol (M.schemaOf n) = [.visible f]) (hf : f n = some t)
    (ht : M.ty v ≠ M.viewerType (M.schemaOf t)) : ¬ M.canOpN (k + 1) app actor (some v) n := by
  intro h
  have hpre := h.1
  rw [visible_preserves_viewer hpol hf hpre] at h
  exact ht ((canOpN_pre h).2.2.2 v rfl)

/-! ### Act-for never amplifies -/

/-- The nodes a check at `n` can reach through `visible`. -/
inductive Reach (M : Model Node Ty Schema App) (n : Node) : Node → Prop
  | refl : Reach M n n
  | step {t t' : Node} {f : Node → Option Node} :
      Reach M n t → Rule.visible f ∈ M.pol (M.schemaOf t) → f t = some t' → Reach M n t'

theorem Reach.trans {n t s : Node} (h₁ : M.Reach n t) (h₂ : M.Reach t s) : M.Reach n s := by
  induction h₂ with
  | refl => exact h₁
  | step _ hf hft ih => exact .step ih hf hft

/-- No allow rule reachable from `n` (transitively through `visible`) reads the actor. -/
def NoActorAllow (M : Model Node Ty Schema App) (n : Node) : Prop :=
  ∀ t, M.Reach n t → ∀ r ∈ M.pol (M.schemaOf t), ∀ p, r ≠ .onActor .allow p

/-- Actor `a`, for viewer `v`, passes every reachable schema's actors declaration, and no
reachable actor-reading rule matches it (or errors). -/
def ActorOK (M : Model Node Ty Schema App) (app : App) (a v : Option Node) (n : Node) : Prop :=
  ∀ t, M.Reach n t → admits (M.actors (M.schemaOf t)) (a.map M.ty) ∧
    ∀ r ∈ M.pol (M.schemaOf t), ∀ e p, r = .onActor e p → p app a v t = .no

/-- The rule-list step: with the viewer fixed, swapping the actor can only turn Deny into Allow,
given that no actor rule allows and the new actor matches no actor rule. -/
theorem evalRules_actor_mono {dl₁ dl₂ : Node → Prop} {app : App} {a₁ a₂ : Option Node}
    {u n : Node} : ∀ (rules : List (Rule App Node)),
    (∀ f t, Rule.visible f ∈ rules → f n = some t → dl₁ t → dl₂ t) →
    (∀ r ∈ rules, ∀ p, r ≠ .onActor .allow p) →
    (∀ r ∈ rules, ∀ e p, r = .onActor e p → p app a₂ (some u) n = .no) →
    evalRules dl₁ rules app a₁ (some u) n → evalRules dl₂ rules app a₂ (some u) n
  | [], _, _, _, h => h
  | r :: rs, hdl, hno, hcl, h => by
    have ih := evalRules_actor_mono (a₁ := a₁) rs
      (fun f t hf => hdl f t (List.mem_cons_of_mem _ hf))
      (fun r' hr' => hno r' (List.mem_cons_of_mem _ hr'))
      (fun r' hr' => hcl r' (List.mem_cons_of_mem _ hr'))
    rw [evalRules_cons] at h ⊢
    cases r with
    | onViewer e p =>
      -- the same result on both sides: it reads only the app, the viewer, and the node
      simp only [Rule.fire] at h ⊢
      rcases h with h | ⟨h0, h⟩
      · exact Or.inl h
      · exact Or.inr ⟨h0, ih h⟩
    | onApp e p =>
      cases e with
      | allow =>
        rw [appRule_skips_unless_asApp dl₁ p app n (Or.inr (by simp))] at h
        rw [appRule_skips_unless_asApp dl₂ p app n (Or.inr (by simp))]
        rcases h with h | ⟨-, h⟩
        · cases h
        · exact Or.inr ⟨rfl, ih h⟩
      | deny =>
        -- reads only the app and the node: the same result on both sides
        simp only [Rule.fire] at h ⊢
        rcases h with h | ⟨h0, h⟩
        · exact Or.inl h
        · exact Or.inr ⟨h0, ih h⟩
    | onActor e p =>
      have he : e = .deny := by
        cases e with
        | allow => exact absurd rfl (hno _ List.mem_cons_self p)
        | deny => rfl
      subst he
      have h2 : p app a₂ (some u) n = .no := hcl _ List.mem_cons_self _ _ rfl
      simp only [Rule.fire, h2, Res.eff] at h ⊢
      rcases h with h | ⟨-, h⟩
      · cases hp : p app a₁ (some u) n <;> simp [hp] at h
      · simp [ih h]
    | visible f =>
      simp only [Rule.fire] at h ⊢
      cases hf : f n with
      | none => simp [hf] at h
      | some t =>
        simp only [hf] at h ⊢
        by_cases h₁ : dl₁ t
        · have h₂ := hdl f t List.mem_cons_self hf h₁
          simp [h₂]
        · simp only [h₁, ↓reduceIte] at h
          rcases h with h | ⟨-, h⟩
          · cases h
          · by_cases h₂ : dl₂ t
            · simp [h₂]
            · simp [h₂, ih h]

/-- **Swapping the actor, with the viewer fixed.** If no reachable allow rule reads the actor,
then any actor `a₂` that is live, passes the reachable declarations, and matches no reachable
actor rule can do everything any actor `a₁` can for the same viewer. -/
theorem canOpN_actor_mono (app : App) (a₁ a₂ : Option Node) (u : Node) :
    ∀ (k : ℕ) (n : Node), M.NoActorAllow n → M.ActorOK app a₂ (some u) n →
      (∀ x, a₂ = some x → ¬ M.deleted x) →
      M.canOpN k app a₁ (some u) n → M.canOpN k app a₂ (some u) n
  | 0, _, _, _, _, h => h
  | k + 1, n, hno, hok, hdel, h => by
    obtain ⟨⟨-, h2, -, h4⟩, hr⟩ := h
    refine ⟨⟨hdel, h2, (hok n .refl).1, h4⟩, ?_⟩
    refine evalRules_actor_mono _ ?_ (hno n .refl) (fun r hr e p he => (hok n .refl).2 r hr e p he)
      hr
    intro f t hf hft hdl
    have hnt : M.Reach n t := .step .refl hf hft
    exact canOpN_actor_mono app a₁ a₂ u k t (fun s hs => hno s (hnt.trans hs))
      (fun s hs => hok s (hnt.trans hs)) hdel hdl

/-- **An agent acting for a user can never do more than the user.** If no allow rule reads
`actor`, transitively through `visible`, then whatever `g` can do for viewer `u`, `u` can do
acting as itself, given that `u` acting as itself passes the reachable actor declarations and
actor-reading deny rules (`ActorOK`; see the counterexamples below for why it's needed). That
`u` is live and of the viewer type follows from the left side. -/
theorem canOp_actfor_no_amplification {app : App} {g u n : Node} (hno : M.NoActorAllow n)
    (hok : M.ActorOK app (some u) (some u) n) :
    M.canOp app (some g) (some u) n → M.canOp app (some u) (some u) n := fun h =>
  canOpN_actor_mono app (some g) (some u) u M.depth n hno hok
    (fun x hx => by cases hx; exact (canOpN_pre h).2.1 u rfl) h

/-- The checker's rule: a schema that declares no actors has no rules on `actor` (§6.3,
"undeclared means unreferenced"). -/
def WF (M : Model Node Ty Schema App) : Prop :=
  ∀ s, M.actors s = none → ∀ r ∈ M.pol s, ∀ e p, r ≠ .onActor e p

theorem actorOK_of_undeclared (hwf : M.WF) {app : App} {a v : Option Node} {n : Node}
    (hun : ∀ t, M.Reach n t → M.actors (M.schemaOf t) = none) : M.ActorOK app a v n := by
  intro t ht
  refine ⟨by rw [hun t ht]; trivial, ?_⟩
  intro r hr e p he
  exact absurd he (hwf _ (hun t ht) r hr e p)

theorem noActorAllow_of_undeclared (hwf : M.WF) {n : Node}
    (hun : ∀ t, M.Reach n t → M.actors (M.schemaOf t) = none) : M.NoActorAllow n :=
  fun t ht r hr p => hwf _ (hun t ht) r hr .allow p

/-- **Viewer-only policies: an agent can do exactly what its user can.** When no schema reachable
from `n` declares actors (so, by the checker, none of their rules read `actor`), a live agent
acting for `u` and `u` acting as itself get the same answer on `n` (§6.8: such a policy is a
relation over viewers). -/
theorem canOp_actfor_eq_undeclared (hwf : M.WF) {app : App} {g u n : Node}
    (hun : ∀ t, M.Reach n t → M.actors (M.schemaOf t) = none) (hg : ¬ M.deleted g) :
    M.canOp app (some g) (some u) n ↔ M.canOp app (some u) (some u) n := by
  constructor
  · exact canOp_actfor_no_amplification (noActorAllow_of_undeclared hwf hun)
      (actorOK_of_undeclared hwf hun)
  · exact canOpN_actor_mono app (some u) (some g) u M.depth n
      (noActorAllow_of_undeclared hwf hun) (actorOK_of_undeclared hwf hun)
      (fun x hx => by cases hx; exact hg)

end Model

/-! ### Counterexamples (first match) -/

namespace Ex

/-- An agent `g`, a user `u`, and a document `d`. -/
inductive N
  | g
  | u
  | d
  deriving DecidableEq

inductive T
  | agent
  | user
  | doc
  deriving DecidableEq

def tyOf : N → T
  | .g => .agent
  | .u => .user
  | .d => .doc

/-- One schema whose viewer type is `users.User`, with the given declaration and policy. -/
def mk (decl : Option (Set (Option T))) (pol : List (Rule Unit N)) : Model N T Unit Unit :=
  ⟨tyOf, fun _ => False, fun _ => (), fun _ => .user, fun _ => decl, fun _ => pol, 1⟩

/-- `actors users.User | agents.Agent` -/
def both : Set (Option T) := {some .agent, some .user}

/-- `allow if actor is agents.Agent` -/
def actorIsAgent : Rule Unit N :=
  .onActor .allow (fun _ a _ _ => if a = some .g then .yes else .no)

/-- **An actor-reading allow rule amplifies:** the agent, acting for `u`, reads `d`; `u` itself
can't. -/
theorem actorAllow_amplifies :
    (mk (some both) [actorIsAgent]).canOp () (some .g) (some .u) .d ∧
      ¬ (mk (some both) [actorIsAgent]).canOp () (some .u) (some .u) .d := by
  simp [mk, both, actorIsAgent, Model.canOp, Model.canOpN, Model.Pre, evalRules, Rule.fire,
    Res.eff, admits, tyOf]

/-- `allow if true` on the viewer -/
def anyViewer : Rule Unit N := .onViewer .allow (fun _ _ _ => .yes)

/-- **A declaration that leaves out the viewer type** (`actors agents.Agent` in a schema whose
viewer type is `users.User`) lets the agent do what the user, acting directly, can't. -/
theorem actorsDecl_excluding_viewer_amplifies :
    (mk (some {some .agent}) [anyViewer]).canOp () (some .g) (some .u) .d ∧
      ¬ (mk (some {some .agent}) [anyViewer]).canOp () (some .u) (some .u) .d := by
  simp [mk, anyViewer, Model.canOp, Model.canOpN, Model.Pre, evalRules, Rule.fire, Res.eff,
    admits, tyOf]

/-- `deny if actor is users.User` -/
def denyUsers : Rule Unit N :=
  .onActor .deny (fun _ a _ _ => if a = some .u then .yes else .no)

/-- **A deny rule on the viewer's own type** does the same. -/
theorem actorDeny_on_self_amplifies :
    (mk (some both) [denyUsers, anyViewer]).canOp () (some .g) (some .u) .d ∧
      ¬ (mk (some both) [denyUsers, anyViewer]).canOp () (some .u) (some .u) .d := by
  simp [mk, both, denyUsers, anyViewer, Model.canOp, Model.canOpN, Model.Pre, evalRules,
    Rule.fire, Res.eff, admits, tyOf]

/-- A model where `u` is deleted, but its membership edge (here: the rule's condition) remains. -/
def delU : Model N T Unit Unit :=
  ⟨tyOf, fun x => x = .u, fun _ => (), fun _ => .user, fun _ => none,
    fun _ => [.onViewer .allow (fun _ v _ => if v = .u then .yes else .no)], 1⟩

/-- **The deleted-viewer pre-check is load-bearing.** Soft delete keeps edges, so the rules alone
still allow an agent acting for the deleted user; `can_op` denies it only because of the check. -/
theorem precheck_viewer_needed :
    delU.deleted .u ∧ evalRules (fun _ => False) (delU.pol ()) () (some .g) (some .u) .d ∧
      ¬ delU.canOp () (some .g) (some .u) .d := by
  refine ⟨rfl, by simp [delU, evalRules, Rule.fire, Res.eff], ?_⟩
  exact Model.canOp_deleted_viewer () _ (M := delU) rfl _

/-- **The superseded semantics fell through.** In `ViewerMapping.lean`, a request with no viewer
(a mapping to a deleted node, or a viewer of another type) was judged by the rules on `app` and
`actor` alone, so `allow if app == slack` admitted an agent. `canOp_wrongtype_viewer`,
`canOp_deleted_viewer`, and `canOp_appOnly_with_viewer` close each path. -/
theorem fallthrough_reaches_app_grant :
    Viewer.evalRules [Viewer.Rule.onOther .allow (fun (_ : Unit) _ _ => True)] () (some N.g) none
      N.d := by
  simp [Viewer.evalRules, Viewer.Rule.fire]

end Ex

/-! ## Union minus deny (the current §6.1)

Policies combine as allow-union-minus-deny with default deny, as in Gel and Cedar: a request is
allowed iff some allow guard holds and no deny guard holds, `(∨ A) ∧ ¬ (∨ D)`. Guards are total
and two-valued (no runtime errors); a guard atom that reads an absent viewer or actor is false.
The pre-checks are unchanged. -/

namespace UMD

/-- A total, two-valued guard: atoms joined by `&&`. -/
inductive Guard (App Node : Type*)
  /-- reads the app, the viewer, and the node; false without a viewer -/
  | onViewer (p : App → Node → Node → Prop)
  /-- reads only the app and the node -/
  | onApp (p : App → Node → Prop)
  /-- reads the actor node (and maybe the app and viewer); false without an actor node -/
  | onActor (p : App → Node → Option Node → Node → Prop)
  /-- `shep::visible(.edge)`: the target passes `can_op` for the same request; false if the edge
  is absent -/
  | visible (edge : Node → Option Node)
  | and (g₁ g₂ : Guard App Node)

/-- A guard's value, given `dl`, the decision at a `visible` target. -/
def Guard.eval (dl : Node → Prop) :
    Guard App Node → App → Option Node → Option Node → Node → Prop
  | .onViewer p, app, _, viewer, n => ∃ v, viewer = some v ∧ p app v n
  | .onApp p, app, _, _, n => p app n
  | .onActor p, app, actor, viewer, n => ∃ a, actor = some a ∧ p app a viewer n
  | .visible f, _, _, _, n => ∃ t, f n = some t ∧ dl t
  | .and g₁ g₂, app, actor, viewer, n =>
    g₁.eval dl app actor viewer n ∧ g₂.eval dl app actor viewer n

/-- The guard reads only the app (`allow if app == slack`). -/
def Guard.appOnly : Guard App Node → Prop
  | .onApp _ => True
  | .and g₁ g₂ => g₁.appOnly ∧ g₂.appOnly
  | _ => False

def Guard.readsActor : Guard App Node → Prop
  | .onActor _ => True
  | .and g₁ g₂ => g₁.readsActor ∨ g₂.readsActor
  | _ => False

def Guard.hasVisible : Guard App Node → Prop
  | .visible _ => True
  | .and g₁ g₂ => g₁.hasVisible ∨ g₂.hasVisible
  | _ => False

/-- The `visible` targets of a guard at `n`. -/
def Guard.reaches : Guard App Node → Node → Node → Prop
  | .visible f, n, t => f n = some t
  | .and g₁ g₂, n, t => g₁.reaches n t ∨ g₂.reaches n t
  | _, _, _ => False

/-- Every actor atom of the guard is false for actor `a` and viewer `v`. -/
def Guard.actorAtomsFalse (app : App) (a v : Option Node) (n : Node) : Guard App Node → Prop
  | .onActor p => ¬ ∃ x, a = some x ∧ p app x v n
  | .and g₁ g₂ => g₁.actorAtomsFalse app a v n ∧ g₂.actorAtomsFalse app a v n
  | _ => True

/-- An allow guard; one that grants by app alone matches only the app acting as itself. -/
def allowHolds (dl : Node → Prop) (g : Guard App Node) (app : App) (actor viewer : Option Node)
    (n : Node) : Prop :=
  g.eval dl app actor viewer n ∧ (g.appOnly → actor = none ∧ viewer = none)

structure Policy (App Node : Type*) where
  allow : List (Guard App Node)
  deny : List (Guard App Node)

/-- **Union minus deny:** `(∨ A) ∧ ¬ (∨ D)`; no allow means deny. -/
def evalPolicy (dl : Node → Prop) (P : Policy App Node) (app : App) (actor viewer : Option Node)
    (n : Node) : Prop :=
  (∃ g ∈ P.allow, allowHolds dl g app actor viewer n) ∧
    ¬ ∃ g ∈ P.deny, g.eval dl app actor viewer n

structure Model (Node Ty Schema App : Type*) where
  ty : Node → Ty
  deleted : Node → Prop
  schemaOf : Node → Schema
  viewerType : Schema → Ty
  actors : Schema → Option (Set (Option Ty))
  pol : Schema → Policy App Node
  depth : ℕ

namespace Model

/-- The unchanged pre-checks: deleted actor, deleted viewer, the actors declaration, and the
viewer's type. -/
def Pre (M : Model Node Ty Schema App) (actor viewer : Option Node) (n : Node) : Prop :=
  (∀ a, actor = some a → ¬ M.deleted a) ∧
  (∀ v, viewer = some v → ¬ M.deleted v) ∧
  admits (M.actors (M.schemaOf n)) (actor.map M.ty) ∧
  (∀ v, viewer = some v → M.ty v = M.viewerType (M.schemaOf n))

def canOpN (M : Model Node Ty Schema App) :
    ℕ → App → Option Node → Option Node → Node → Prop
  | 0, _, _, _, _ => False
  | k + 1, app, actor, viewer, n =>
    M.Pre actor viewer n ∧
      evalPolicy (canOpN M k app actor viewer) (M.pol (M.schemaOf n)) app actor viewer n

def canOp (M : Model Node Ty Schema App) (app : App) (actor viewer : Option Node) (n : Node) :
    Prop :=
  M.canOpN M.depth app actor viewer n

variable {M : Model Node Ty Schema App}

theorem canOpN_pre {k : ℕ} {app : App} {actor viewer : Option Node} {n : Node}
    (h : M.canOpN k app actor viewer n) : M.Pre actor viewer n := by
  cases k with
  | zero => exact h.elim
  | succ k => exact h.1

theorem canOp_deleted_actor (app : App) {a : Node} (hd : M.deleted a) (viewer : Option Node)
    (n : Node) : ¬ M.canOp app (some a) viewer n :=
  fun h => (canOpN_pre h).1 a rfl hd

theorem canOp_deleted_viewer (app : App) (actor : Option Node) {v : Node} (hd : M.deleted v)
    (n : Node) : ¬ M.canOp app actor (some v) n :=
  fun h => (canOpN_pre h).2.1 v rfl hd

theorem canOp_wrongtype_viewer (app : App) (actor : Option Node) {v : Node} (n : Node)
    (ht : M.ty v ≠ M.viewerType (M.schemaOf n)) : ¬ M.canOp app actor (some v) n :=
  fun h => ht ((canOpN_pre h).2.2.2 v rfl)

theorem actorsDecl_enforced (app : App) (actor viewer : Option Node) (n : Node)
    {D : Set (Option Ty)} (hD : M.actors (M.schemaOf n) = some D)
    (ha : actor.map M.ty ∉ D) : ¬ M.canOp app actor viewer n := by
  intro h
  have h3 := (canOpN_pre h).2.2.1
  rw [hD] at h3
  exact ha h3

/-- **An app-only allow never admits a request with a viewer**, so a policy whose allows are all
app-only denies every such request. -/
theorem canOp_appOnly_with_viewer (app : App) (actor : Option Node) (v n : Node)
    (hall : ∀ g ∈ (M.pol (M.schemaOf n)).allow, g.appOnly) : ¬ M.canOp app actor (some v) n := by
  unfold canOp
  cases M.depth with
  | zero => exact id
  | succ k =>
    rintro ⟨-, ⟨g, hg, -, himp⟩, -⟩
    cases (himp (hall g hg)).2

/-- **`visible` keeps the request's viewer.** For `Message.read: allow if
shep::visible(.channel)`, the message admits exactly the requests (same app, actor, and viewer)
that the channel admits, once the message's own pre-checks pass. -/
theorem visible_preserves_viewer {k : ℕ} {app : App} {actor viewer : Option Node} {n t : Node}
    {f : Node → Option Node} (hpol : M.pol (M.schemaOf n) = ⟨[.visible f], []⟩)
    (hf : f n = some t) (hpre : M.Pre actor viewer n) :
    M.canOpN (k + 1) app actor viewer n ↔ M.canOpN k app actor viewer t := by
  simp [canOpN, hpol, evalPolicy, allowHolds, Guard.eval, Guard.appOnly, hf, hpre]

theorem visible_crosstype_denies {k : ℕ} {app : App} {actor : Option Node} {v n t : Node}
    {f : Node → Option Node} (hpol : M.pol (M.schemaOf n) = ⟨[.visible f], []⟩)
    (hf : f n = some t) (ht : M.ty v ≠ M.viewerType (M.schemaOf t)) :
    ¬ M.canOpN (k + 1) app actor (some v) n := by
  intro h
  rw [visible_preserves_viewer hpol hf h.1] at h
  exact ht ((canOpN_pre h).2.2.2 v rfl)

/-- The nodes a check at `n` can reach through `visible`, in allow or deny guards. -/
inductive Reach (M : Model Node Ty Schema App) (n : Node) : Node → Prop
  | refl : Reach M n n
  | step {t t' : Node} {g : Guard App Node} : Reach M n t →
      g ∈ (M.pol (M.schemaOf t)).allow ++ (M.pol (M.schemaOf t)).deny → g.reaches t t' →
      Reach M n t'

theorem Reach.trans {n t s : Node} (h₁ : M.Reach n t) (h₂ : M.Reach t s) : M.Reach n s := by
  induction h₂ with
  | refl => exact h₁
  | step _ hg hgt ih => exact .step ih hg hgt

/-- No allow guard reachable from `n` reads the actor. -/
def NoActorAllow (M : Model Node Ty Schema App) (n : Node) : Prop :=
  ∀ t, M.Reach n t → ∀ g ∈ (M.pol (M.schemaOf t)).allow, ¬ g.readsActor

/-- No deny guard reachable from `n` uses `visible`. -/
def NoVisibleDeny (M : Model Node Ty Schema App) (n : Node) : Prop :=
  ∀ t, M.Reach n t → ∀ g ∈ (M.pol (M.schemaOf t)).deny, ¬ g.hasVisible

/-- Actor `a`, for viewer `v`, passes every reachable actors declaration, and every actor atom of
every reachable deny guard is false for it. -/
def ActorOK (M : Model Node Ty Schema App) (app : App) (a v : Option Node) (n : Node) : Prop :=
  ∀ t, M.Reach n t → admits (M.actors (M.schemaOf t)) (a.map M.ty) ∧
    ∀ g ∈ (M.pol (M.schemaOf t)).deny, g.actorAtomsFalse app a v t

theorem allow_mono {dl₁ dl₂ : Node → Prop} {app : App} {a₁ a₂ : Option Node} {v : Option Node}
    {n : Node} : ∀ (g : Guard App Node), ¬ g.readsActor →
    (∀ t, g.reaches n t → dl₁ t → dl₂ t) →
    g.eval dl₁ app a₁ v n → g.eval dl₂ app a₂ v n
  | .onViewer _, _, _, h => h
  | .onApp _, _, _, h => h
  | .onActor _, hr, _, _ => absurd trivial hr
  | .visible _, _, hdl, ⟨t, hf, ht⟩ => ⟨t, hf, hdl t hf ht⟩
  | .and g₁ g₂, hr, hdl, ⟨h₁, h₂⟩ =>
    ⟨allow_mono g₁ (fun h => hr (Or.inl h)) (fun t ht => hdl t (Or.inl ht)) h₁,
      allow_mono g₂ (fun h => hr (Or.inr h)) (fun t ht => hdl t (Or.inr ht)) h₂⟩

theorem deny_antimono {dl₁ dl₂ : Node → Prop} {app : App} {a₁ a₂ : Option Node}
    {v : Option Node} {n : Node} : ∀ (g : Guard App Node), ¬ g.hasVisible →
    g.actorAtomsFalse app a₂ v n → g.eval dl₂ app a₂ v n → g.eval dl₁ app a₁ v n
  | .onViewer _, _, _, h => h
  | .onApp _, _, _, h => h
  | .onActor _, _, hf, h => absurd h hf
  | .visible _, hv, _, _ => absurd trivial hv
  | .and g₁ g₂, hv, ⟨hf₁, hf₂⟩, ⟨h₁, h₂⟩ =>
    ⟨deny_antimono g₁ (fun h => hv (Or.inl h)) hf₁ h₁,
      deny_antimono g₂ (fun h => hv (Or.inr h)) hf₂ h₂⟩

/-- **Swapping the actor, with the viewer fixed** (union minus deny). -/
theorem canOpN_actor_mono (app : App) (a₁ a₂ : Option Node) (u : Node) :
    ∀ (k : ℕ) (n : Node), M.NoActorAllow n → M.NoVisibleDeny n →
      M.ActorOK app a₂ (some u) n → (∀ x, a₂ = some x → ¬ M.deleted x) →
      M.canOpN k app a₁ (some u) n → M.canOpN k app a₂ (some u) n
  | 0, _, _, _, _, _, h => h
  | k + 1, n, hno, hnv, hok, hdel, h => by
    obtain ⟨⟨-, h2, -, h4⟩, ⟨g, hg, hgA, himp⟩, hD⟩ := h
    refine ⟨⟨hdel, h2, (hok n .refl).1, h4⟩, ⟨g, hg, ?_, ?_⟩, ?_⟩
    · refine allow_mono g (hno n .refl g hg) ?_ hgA
      intro t ht hdl
      have hnt : M.Reach n t := .step .refl (List.mem_append_left _ hg) ht
      exact canOpN_actor_mono app a₁ a₂ u k t (fun s hs => hno s (hnt.trans hs))
        (fun s hs => hnv s (hnt.trans hs)) (fun s hs => hok s (hnt.trans hs)) hdel hdl
    · intro hap; cases (himp hap).2
    · rintro ⟨d, hd, hdE⟩
      exact hD ⟨d, hd, deny_antimono d (hnv n .refl d hd) ((hok n .refl).2 d hd) hdE⟩

/-- **An agent acting for a user can never do more than the user** (union minus deny). If no
reachable allow guard reads `actor` and no reachable deny guard uses `visible`, then whatever `g`
can do for viewer `u`, `u` can do acting as itself, given that `u` acting as itself passes the
reachable actor declarations and makes every reachable deny guard's actor atoms false. -/
theorem canOp_actfor_no_amplification {app : App} {g u n : Node} (hno : M.NoActorAllow n)
    (hnv : M.NoVisibleDeny n) (hok : M.ActorOK app (some u) (some u) n) :
    M.canOp app (some g) (some u) n → M.canOp app (some u) (some u) n := fun h =>
  canOpN_actor_mono app (some g) (some u) u M.depth n hno hnv hok
    (fun x hx => by cases hx; exact (canOpN_pre h).2.1 u rfl) h

/-- The checker's rule: a schema that declares no actors has no guard on `actor`. -/
def WF (M : Model Node Ty Schema App) : Prop :=
  ∀ s, M.actors s = none → ∀ g ∈ (M.pol s).allow ++ (M.pol s).deny, ¬ g.readsActor

theorem eval_actor_indep {dl₁ dl₂ : Node → Prop} {app : App} {a₁ a₂ v : Option Node}
    {n : Node} : ∀ (g : Guard App Node), ¬ g.readsActor →
    (∀ t, g.reaches n t → (dl₁ t ↔ dl₂ t)) →
    (g.eval dl₁ app a₁ v n ↔ g.eval dl₂ app a₂ v n)
  | .onViewer _, _, _ => Iff.rfl
  | .onApp _, _, _ => Iff.rfl
  | .onActor _, hr, _ => absurd trivial hr
  | .visible _, _, hdl => by
    simp only [Guard.eval]
    exact exists_congr fun t => ⟨fun ⟨hf, h⟩ => ⟨hf, (hdl t hf).1 h⟩,
      fun ⟨hf, h⟩ => ⟨hf, (hdl t hf).2 h⟩⟩
  | .and g₁ g₂, hr, hdl => and_congr
    (eval_actor_indep g₁ (fun h => hr (Or.inl h)) (fun t ht => hdl t (Or.inl ht)))
    (eval_actor_indep g₂ (fun h => hr (Or.inr h)) (fun t ht => hdl t (Or.inr ht)))

/-- With no actors declared on any reachable schema, the decision doesn't depend on the actor
(given live actor nodes), even with `visible` under a deny. -/
theorem canOpN_actor_indep (hwf : M.WF) (app : App) (a₁ a₂ : Option Node) (u : Node)
    (hd₁ : ∀ x, a₁ = some x → ¬ M.deleted x) (hd₂ : ∀ x, a₂ = some x → ¬ M.deleted x) :
    ∀ (k : ℕ) (n : Node), (∀ t, M.Reach n t → M.actors (M.schemaOf t) = none) →
      (M.canOpN k app a₁ (some u) n ↔ M.canOpN k app a₂ (some u) n)
  | 0, _, _ => Iff.rfl
  | k + 1, n, hun => by
    have hn := hun n .refl
    have ih : ∀ g ∈ (M.pol (M.schemaOf n)).allow ++ (M.pol (M.schemaOf n)).deny,
        ∀ t, g.reaches n t →
          (M.canOpN k app a₁ (some u) t ↔ M.canOpN k app a₂ (some u) t) := fun g hg t ht =>
      canOpN_actor_indep hwf app a₁ a₂ u hd₁ hd₂ k t
        (fun s hs => hun s ((Reach.step .refl hg ht).trans hs))
    have hev : ∀ g ∈ (M.pol (M.schemaOf n)).allow ++ (M.pol (M.schemaOf n)).deny,
        (g.eval (M.canOpN k app a₁ (some u)) app a₁ (some u) n ↔
          g.eval (M.canOpN k app a₂ (some u)) app a₂ (some u) n) := fun g hg =>
      eval_actor_indep g (hwf _ hn g hg) (ih g hg)
    simp only [canOpN, Pre, evalPolicy, allowHolds, hn, admits]
    constructor
    · rintro ⟨⟨-, h2, -, h4⟩, ⟨g, hg, hgA, himp⟩, hD⟩
      refine ⟨⟨hd₂, h2, trivial, h4⟩, ⟨g, hg, (hev g (List.mem_append_left _ hg)).1 hgA,
        fun hap => by cases (himp hap).2⟩, ?_⟩
      rintro ⟨d, hd, hdE⟩
      exact hD ⟨d, hd, (hev d (List.mem_append_right _ hd)).2 hdE⟩
    · rintro ⟨⟨-, h2, -, h4⟩, ⟨g, hg, hgA, himp⟩, hD⟩
      refine ⟨⟨hd₁, h2, trivial, h4⟩, ⟨g, hg, (hev g (List.mem_append_left _ hg)).2 hgA,
        fun hap => by cases (himp hap).2⟩, ?_⟩
      rintro ⟨d, hd, hdE⟩
      exact hD ⟨d, hd, (hev d (List.mem_append_right _ hd)).1 hdE⟩

/-- **Viewer-only policies: an agent can do exactly what its user can** (union minus deny). -/
theorem canOp_actfor_eq_undeclared (hwf : M.WF) {app : App} {g u n : Node}
    (hun : ∀ t, M.Reach n t → M.actors (M.schemaOf t) = none) (hg : ¬ M.deleted g)
    (hu : ¬ M.deleted u) :
    M.canOp app (some g) (some u) n ↔ M.canOp app (some u) (some u) n :=
  canOpN_actor_indep hwf app (some g) (some u) u (fun x hx => by cases hx; exact hg)
    (fun x hx => by cases hx; exact hu) M.depth n hun

end Model

/-! ### Counterexamples under union minus deny -/

namespace Ex

open Principals.Ex (T)

/-- An agent `g`, a user `u`, a document `d`, and another node `t`. -/
inductive N
  | g
  | u
  | d
  | t
  deriving DecidableEq

def tyOf : N → T
  | .g => .agent
  | .u => .user
  | .d => .doc
  | .t => .doc

/-- Schemas: `false` holds `d`, `true` holds `t`; both have viewer type `users.User` and declare
`actors users.User | agents.Agent`. -/
def mk (pd pt : Policy Unit N) : Model N T Bool Unit :=
  ⟨tyOf, fun _ => False, fun x => decide (x = .t), fun _ => .user,
    fun _ => some {some .agent, some .user}, fun s => if s then pt else pd, 2⟩

def none' : Policy Unit N := ⟨[], []⟩

/-- **An actor-reading allow amplifies:** `allow if actor is agents.Agent`. -/
theorem actorAllow_amplifies :
    (mk ⟨[.onActor (fun _ a _ _ => a = .g)], []⟩ none').canOp () (some .g) (some .u) .d ∧
      ¬ (mk ⟨[.onActor (fun _ a _ _ => a = .g)], []⟩ none').canOp () (some .u) (some .u) .d := by
  simp [mk, Model.canOp, Model.canOpN, Model.Pre, evalPolicy, allowHolds, Guard.eval,
    Guard.appOnly, admits, tyOf]

/-- **A deny on the viewer's own type amplifies:** `deny if actor is users.User`. -/
theorem actorDeny_on_self_amplifies :
    (mk ⟨[.onViewer (fun _ _ _ => True)], [.onActor (fun _ a _ _ => a = .u)]⟩ none').canOp ()
        (some .g) (some .u) .d ∧
      ¬ (mk ⟨[.onViewer (fun _ _ _ => True)], [.onActor (fun _ a _ _ => a = .u)]⟩ none').canOp
        () (some .u) (some .u) .d := by
  simp [mk, Model.canOp, Model.canOpN, Model.Pre, evalPolicy, allowHolds, Guard.eval,
    Guard.appOnly, admits, tyOf]

/-- `t`'s policy: `allow if viewer …; deny if actor is agents.Agent`. -/
def ptAgentDeny : Policy Unit N :=
  ⟨[.onViewer (fun _ _ _ => True)], [.onActor (fun _ a _ _ => a = .g)]⟩

/-- `d`'s policy: `allow if viewer …; deny if shep::visible(.t)`. -/
def pdVisibleDeny : Policy Unit N :=
  ⟨[.onViewer (fun _ _ _ => True)], [.visible (fun _ => some .t)]⟩

/-- **`visible` under a deny amplifies** when its target reads the actor: `t` hides itself from
agents, so `deny if shep::visible(.t)` on `d` denies `u` but not the agent acting for `u`. No
allow reads the actor, and `u` makes every actor atom false; that is why the theorem needs
`NoVisibleDeny`. -/
theorem visibleDeny_amplifies :
    (mk pdVisibleDeny ptAgentDeny).canOp () (some .g) (some .u) .d ∧
      ¬ (mk pdVisibleDeny ptAgentDeny).canOp () (some .u) (some .u) .d := by
  simp [mk, pdVisibleDeny, ptAgentDeny, Model.canOp, Model.canOpN, Model.Pre, evalPolicy,
    allowHolds, Guard.eval, Guard.appOnly, admits, tyOf]

end Ex

end UMD

/-! ## Two-party issuance (§6.3, "Issuing takes two parties") -/

namespace Issuance

/-- Who makes a system-partition request. -/
inductive Cred (App Node : Type*)
  /-- a person signed in through SSO -/
  | token (u : Node)
  /-- one of the app's own API keys -/
  | appKey (a : App)
  /-- the app's owner, or an admin acting for it -/
  | owner (a : App)
  /-- an act-for key `(app, actor, viewer)` -/
  | bound (a : App) (g u : Node)

/-- System-partition actions. -/
inductive Act (App Node : Type*)
  /-- `shep agent register --app a g` -/
  | register (a : App) (g : Node)
  | unregister (a : App) (g : Node)
  /-- the app asks for an act-for key `(g, u)` -/
  | request (g u : Node)
  /-- `u` approves app `a`'s request for `g` -/
  | approve (a : App) (g : Node)
  /-- `u` revokes their key for `(a, g)` -/
  | revoke (a : App) (g : Node)

/-- Registrations, pending requests, and user-approved act-for keys. -/
structure St (App Node : Type*) where
  reg : Set (App × Node)
  req : Set (App × Node × Node)
  key : Set (App × Node × Node)

/-- One step. `isViewerType g`: `g`'s type is a viewer type of a schema the app uses (then no key
may make `g` an actor for someone else). A request that fails its conditions changes nothing. -/
def step (isViewerType : App → Node → Prop) (s : St App Node) :
    Cred App Node × Act App Node → St App Node
  | (.owner a, .register a' g) => if a = a' then { s with reg := s.reg ∪ {(a, g)} } else s
  | (.owner a, .unregister a' g) =>
    if a = a' then
      { reg := s.reg \ {(a, g)}, req := {x | x ∈ s.req ∧ (x.1, x.2.1) ≠ (a, g)},
        key := {x | x ∈ s.key ∧ (x.1, x.2.1) ≠ (a, g)} }
    else s
  | (.appKey a, .request g u) =>
    if (a, g) ∈ s.reg ∧ ¬ isViewerType a g then { s with req := s.req ∪ {(a, g, u)} } else s
  | (.token u, .approve a g) =>
    if (a, g, u) ∈ s.req ∧ (a, g) ∈ s.reg then
      { s with req := s.req \ {(a, g, u)}, key := s.key ∪ {(a, g, u)} }
    else s
  | (.token u, .revoke a g) => { s with key := s.key \ {(a, g, u)} }
  | _ => s

def init : St App Node := ⟨∅, ∅, ∅⟩

/-- The state after a trace, newest event first. -/
def run (isViewerType : App → Node → Prop) : List (Cred App Node × Act App Node) → St App Node
  | [] => init
  | e :: tr => step isViewerType (run isViewerType tr) e

/-- **No chaining.** An act-for key can't register, request, approve, or revoke anything. -/
theorem bound_key_inert (isViewerType : App → Node → Prop) (s : St App Node) (a : App)
    (g u : Node) (x : Act App Node) : step isViewerType s (.bound a g u, x) = s := by
  cases x <;> rfl

/-- The invariant: every request and key is for a registered, non-viewer-type actor, was asked
for by the app with its own key, and every key was approved by its viewer while signed in. -/
def Inv (isViewerType : App → Node → Prop) (tr : List (Cred App Node × Act App Node))
    (s : St App Node) : Prop :=
  (∀ a g u, (a, g, u) ∈ s.req →
    (a, g) ∈ s.reg ∧ ¬ isViewerType a g ∧ (Cred.appKey a, Act.request g u) ∈ tr) ∧
  (∀ a g u, (a, g, u) ∈ s.key →
    (a, g) ∈ s.reg ∧ ¬ isViewerType a g ∧ (Cred.appKey a, Act.request g u) ∈ tr ∧
      (Cred.token u, Act.approve a g) ∈ tr)

theorem inv_cons {P : App → Node → Prop} {tr : List (Cred App Node × Act App Node)}
    {s : St App Node} (e : Cred App Node × Act App Node) (h : Inv P tr s) : Inv P (e :: tr) s :=
  ⟨fun a g u hm => let ⟨h1, h2, h3⟩ := h.1 a g u hm; ⟨h1, h2, List.mem_cons_of_mem _ h3⟩,
    fun a g u hm => let ⟨h1, h2, h3, h4⟩ := h.2 a g u hm
      ⟨h1, h2, List.mem_cons_of_mem _ h3, List.mem_cons_of_mem _ h4⟩⟩

theorem inv_register {P : App → Node → Prop} {tr : List (Cred App Node × Act App Node)}
    {s : St App Node} (e : Cred App Node × Act App Node) (a : App) (g : Node)
    (h : Inv P tr s) : Inv P (e :: tr) { s with reg := s.reg ∪ {(a, g)} } := by
  obtain ⟨hr, hk⟩ := inv_cons e h
  exact ⟨fun a₁ g₁ u₁ hm => let ⟨h1, h2, h3⟩ := hr a₁ g₁ u₁ hm; ⟨Or.inl h1, h2, h3⟩,
    fun a₁ g₁ u₁ hm => let ⟨h1, h2, h3, h4⟩ := hk a₁ g₁ u₁ hm; ⟨Or.inl h1, h2, h3, h4⟩⟩

theorem inv_unregister {P : App → Node → Prop} {tr : List (Cred App Node × Act App Node)}
    {s : St App Node} (e : Cred App Node × Act App Node) (a : App) (g : Node)
    (h : Inv P tr s) :
    Inv P (e :: tr)
      { reg := s.reg \ {(a, g)}, req := {x | x ∈ s.req ∧ (x.1, x.2.1) ≠ (a, g)},
        key := {x | x ∈ s.key ∧ (x.1, x.2.1) ≠ (a, g)} } := by
  obtain ⟨hr, hk⟩ := inv_cons e h
  refine ⟨fun a₁ g₁ u₁ hm => ?_, fun a₁ g₁ u₁ hm => ?_⟩
  · simp only [Set.mem_ofPred_eq] at hm
    obtain ⟨h1, h2, h3⟩ := hr a₁ g₁ u₁ hm.1
    exact ⟨by simp only [Set.mem_sdiff, Set.mem_singleton_iff]; exact ⟨h1, hm.2⟩, h2, h3⟩
  · simp only [Set.mem_ofPred_eq] at hm
    obtain ⟨h1, h2, h3, h4⟩ := hk a₁ g₁ u₁ hm.1
    exact ⟨by simp only [Set.mem_sdiff, Set.mem_singleton_iff]; exact ⟨h1, hm.2⟩, h2, h3, h4⟩

theorem inv_request {P : App → Node → Prop} {tr : List (Cred App Node × Act App Node)}
    {s : St App Node} {a : App} {g u : Node} (hreg : (a, g) ∈ s.reg) (hvt : ¬ P a g)
    (h : Inv P tr s) :
    Inv P ((.appKey a, .request g u) :: tr) { s with req := s.req ∪ {(a, g, u)} } := by
  obtain ⟨hr, hk⟩ := inv_cons (.appKey a, .request g u) h
  refine ⟨fun a₁ g₁ u₁ hm => ?_, hk⟩
  rcases hm with hm | hm
  · exact hr a₁ g₁ u₁ hm
  · simp only [Set.mem_singleton_iff, Prod.mk.injEq] at hm
    obtain ⟨rfl, rfl, rfl⟩ := hm
    exact ⟨hreg, hvt, List.mem_cons_self⟩

theorem inv_approve {P : App → Node → Prop} {tr : List (Cred App Node × Act App Node)}
    {s : St App Node} {a : App} {g u : Node} (hreq : (a, g, u) ∈ s.req) (h : Inv P tr s) :
    Inv P ((.token u, .approve a g) :: tr)
      { s with req := s.req \ {(a, g, u)}, key := s.key ∪ {(a, g, u)} } := by
  obtain ⟨hr, hk⟩ := inv_cons (.token u, .approve a g) h
  refine ⟨fun a₁ g₁ u₁ hm => hr a₁ g₁ u₁ hm.1, fun a₁ g₁ u₁ hm => ?_⟩
  rcases hm with hm | hm
  · exact hk a₁ g₁ u₁ hm
  · simp only [Set.mem_singleton_iff, Prod.mk.injEq] at hm
    obtain ⟨rfl, rfl, rfl⟩ := hm
    obtain ⟨h1, h2, h3⟩ := hr a₁ g₁ u₁ hreq
    exact ⟨h1, h2, h3, List.mem_cons_self⟩

theorem inv_revoke {P : App → Node → Prop} {tr : List (Cred App Node × Act App Node)}
    {s : St App Node} (e : Cred App Node × Act App Node) (x : App × Node × Node)
    (h : Inv P tr s) : Inv P (e :: tr) { s with key := s.key \ {x} } := by
  obtain ⟨hr, hk⟩ := inv_cons e h
  exact ⟨hr, fun a₁ g₁ u₁ hm => hk a₁ g₁ u₁ hm.1⟩

theorem inv_run (P : App → Node → Prop) :
    ∀ tr : List (Cred App Node × Act App Node), Inv P tr (run P tr)
  | [] => by simp [Inv, run, init]
  | (c, x) :: tr => by
    have ih := inv_run P tr
    cases c with
    | bound a g u => exact inv_cons _ ih
    | owner a =>
      cases x with
      | register a' g =>
        simp only [run, step]
        split_ifs
        · exact inv_register _ a g ih
        · exact inv_cons _ ih
      | unregister a' g =>
        simp only [run, step]
        split_ifs
        · exact inv_unregister _ a g ih
        · exact inv_cons _ ih
      | request g u => exact inv_cons _ ih
      | approve a' g => exact inv_cons _ ih
      | revoke a' g => exact inv_cons _ ih
    | appKey a =>
      cases x with
      | register a' g => exact inv_cons _ ih
      | unregister a' g => exact inv_cons _ ih
      | request g u =>
        simp only [run, step]
        split_ifs with hc
        · exact inv_request hc.1 hc.2 ih
        · exact inv_cons _ ih
      | approve a' g => exact inv_cons _ ih
      | revoke a' g => exact inv_cons _ ih
    | token u =>
      cases x with
      | register a' g => exact inv_cons _ ih
      | unregister a' g => exact inv_cons _ ih
      | request g u' => exact inv_cons _ ih
      | approve a g =>
        simp only [run, step]
        split_ifs with hc
        · exact inv_approve hc.1 ih
        · exact inv_cons _ ih
      | revoke a g =>
        simp only [run, step]
        exact inv_revoke _ _ ih

/-- **Issuing takes two parties.** A user-approved act-for key `(a, g, u)` exists only if `g` is
registered to `a` (now), `g`'s type is not a viewer type of a schema `a` uses, `a` requested it
with its own key, and `u` approved it while signed in. -/
theorem actfor_key_two_party (isViewerType : App → Node → Prop)
    (tr : List (Cred App Node × Act App Node)) {a : App} {g u : Node}
    (h : (a, g, u) ∈ (run isViewerType tr).key) :
    (a, g) ∈ (run isViewerType tr).reg ∧ ¬ isViewerType a g ∧
      (Cred.appKey a, Act.request g u) ∈ tr ∧ (Cred.token u, Act.approve a g) ∈ tr :=
  (inv_run isViewerType tr).2 a g u h

/-- Neither party alone suffices: without `u`'s approval there is no key for `u`. -/
theorem actfor_needs_consent (isViewerType : App → Node → Prop)
    (tr : List (Cred App Node × Act App Node)) {a : App} {g u : Node}
    (hno : (Cred.token u, Act.approve a g) ∉ tr) : (a, g, u) ∉ (run isViewerType tr).key :=
  fun h => hno (actfor_key_two_party isViewerType tr h).2.2.2

/-- …and without the app's registration of `g` there is none either. -/
theorem actfor_needs_registration (isViewerType : App → Node → Prop)
    (tr : List (Cred App Node × Act App Node)) {a : App} {g u : Node}
    (hno : (a, g) ∉ (run isViewerType tr).reg) : (a, g, u) ∉ (run isViewerType tr).key :=
  fun h => hno (actfor_key_two_party isViewerType tr h).1

end Issuance

end Principals
end Shep
