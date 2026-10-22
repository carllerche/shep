/-
# Shep's fact model and nested documents (design.md §10.1, §10.2)

Base data is a Z-set of facts `(node, path, value)`. A path is a list of segments: field IDs
and map keys (and reserved segments such as `@type`, `@deleted_at`). Per the current design:

* scalar leaves and **ordered lists** are single-valued leaves: at most one live value per
  `(node, path)`; a list is an atomic value, changed by `−(n, p, oldList) + (n, p, newList)`;
* **set members** are facts at the set's path, one per member, each with weight 0/1;
* **maps** are structural: a map key is a path segment.

The schema decides which paths are single-valued and which are set paths; we abstract it as
a function `kind : Path → Kind`. The model is parametric in the types of nodes `N`, path
segments `Seg`, and values `V` (values include lists, so list leaves need no special case).
-/
import ShepZSet.DBSP

noncomputable section
open Classical

namespace Shep

variable {N Seg V : Type*}

/-- A path: field IDs and map keys (and reserved segments such as `@type`). -/
abbrev Path (Seg : Type*) := List Seg

/-- A node fact `(node, path, value)`. -/
abbrev Fact (N Seg V : Type*) := N × Path Seg × V

/-- What the schema says lives at a path. -/
inductive Kind
  /-- a scalar or list leaf (also `@type`, `@deleted_at`): at most one live value -/
  | single
  /-- a set path: each member is its own fact -/
  | member
  deriving DecidableEq

/-- `p` is a strict prefix of `q`. -/
def StrictPrefix (p q : Path Seg) : Prop := p <+: q ∧ p ≠ q

/-- Functional dependency: at a single-valued path a node has at most one live value. -/
def FD (kind : Path Seg → Kind) (S : ZSet (Fact N Seg V)) : Prop :=
  ∀ n p v w, kind p = .single → S (n, p, v) ≠ 0 → S (n, p, w) ≠ 0 → v = w

/-- Tree shape: a path that holds a live leaf has no live leaf strictly below or above it
(a path is either a leaf or an interior node, never both). -/
def TreeShaped (S : ZSet (Fact N Seg V)) : Prop :=
  ∀ n p q v w, S (n, p, v) ≠ 0 → S (n, q, w) ≠ 0 → ¬ StrictPrefix p q

/-- **Well-formed state**: a set (weights 0/1), functional at single-valued paths, and
tree-shaped. -/
structure WF (kind : Path Seg → Kind) (S : ZSet (Fact N Seg V)) : Prop where
  isSet : IsSet S
  fd : FD kind S
  tree : TreeShaped S

variable {kind : Path Seg → Kind}

/-! ## General preservation lemmas -/

theorem FD.mono {S S' : ZSet (Fact N Seg V)} (h : FD kind S)
    (hsub : ∀ x, S' x ≠ 0 → S x ≠ 0) : FD kind S' :=
  fun n p v w hk hv hw => h n p v w hk (hsub _ hv) (hsub _ hw)

/-- Tree shape depends only on which `(node, path)` keys are live. -/
theorem TreeShaped.mono_keys {S S' : ZSet (Fact N Seg V)} (h : TreeShaped S)
    (hsub : ∀ n p v, S' (n, p, v) ≠ 0 → ∃ w, S (n, p, w) ≠ 0) : TreeShaped S' := by
  intro n p q v w hv hw
  obtain ⟨v', hv'⟩ := hsub n p v hv
  obtain ⟨w', hw'⟩ := hsub n q w hw
  exact h n p q v' w' hv' hw'

/-- **Removal preserves well-formedness**: subtracting any part `R` of the state (each fact's
weight in `R` is `0` or its full weight in `S`). -/
theorem wf_sub_part {S R : ZSet (Fact N Seg V)} (hS : WF kind S)
    (hR : ∀ x, R x = 0 ∨ R x = S x) : WF kind (S - R) := by
  have hpart : ∀ x, (S - R) x = S x ∨ (S - R) x = 0 := by
    intro x; rcases hR x with h | h <;> simp [h]
  have hsub : ∀ x, (S - R) x ≠ 0 → S x ≠ 0 := by
    intro x hx; rcases hpart x with h | h
    · rwa [h] at hx
    · exact absurd h hx
  refine ⟨?_, hS.fd.mono hsub, hS.tree.mono_keys (fun n p v h => ⟨v, hsub _ h⟩)⟩
  intro x; rcases hpart x with h | h
  · rw [h]; exact hS.isSet x
  · left; exact h

/-! ## Subtree removal (§10.2 "Removing a subtree is −1 for every leaf under it") -/

/-- The facts of node `n` at or under path prefix `p`. -/
def subtree (S : ZSet (Fact N Seg V)) (n : N) (p : Path Seg) : ZSet (Fact N Seg V) :=
  Finsupp.filter (fun f => f.1 = n ∧ p <+: f.2.1) S

/-- The delta that removes the subtree at `(n, p)`. -/
def removeSubtreeDelta (S : ZSet (Fact N Seg V)) (n : N) (p : Path Seg) :
    ZSet (Fact N Seg V) := -subtree S n p

/-- The removal delta is exactly `−1` on every live leaf under the prefix (for a set `S`),
and `0` elsewhere. -/
theorem removeSubtreeDelta_apply (S : ZSet (Fact N Seg V)) (n : N) (p : Path Seg)
    (x : Fact N Seg V) :
    removeSubtreeDelta S n p x = if x.1 = n ∧ p <+: x.2.1 then -S x else 0 := by
  unfold removeSubtreeDelta subtree
  rw [Finsupp.neg_apply, Finsupp.filter_apply]
  split_ifs <;> simp

theorem wf_remove_subtree {S : ZSet (Fact N Seg V)} (hS : WF kind S) (n : N) (p : Path Seg) :
    WF kind (S + removeSubtreeDelta S n p) := by
  have : S + removeSubtreeDelta S n p = S - subtree S n p := by
    simp [removeSubtreeDelta, sub_eq_add_neg]
  rw [this]
  apply wf_sub_part hS
  intro x
  unfold subtree; rw [Finsupp.filter_apply]
  split_ifs <;> simp

/-- Removing a map entry `k` of the map at `p` is removing the subtree at `p ++ [k]`. -/
theorem wf_remove_map_entry {S : ZSet (Fact N Seg V)} (hS : WF kind S) (n : N)
    (p : Path Seg) (k : Seg) : WF kind (S + removeSubtreeDelta S n (p ++ [k])) :=
  wf_remove_subtree hS n (p ++ [k])

/-- Removing one live fact: a set member, or unsetting an optional leaf. -/
theorem wf_remove_fact {S : ZSet (Fact N Seg V)} (hS : WF kind S) (x : Fact N Seg V)
    (hx : S x = 1) : WF kind (S + -Finsupp.single x 1) := by
  rw [← sub_eq_add_neg]
  apply wf_sub_part hS
  intro y
  by_cases h : x = y
  · subst h; right; simp [hx]
  · left; simp [h]

/-! ## Setting a single-valued leaf (scalar or list): `−(n,p,old) + (n,p,new)` -/

/-- The field-change delta of §10.1 (`−(n, done, false), +(n, done, true)`); for a list leaf,
`old` and `new` are whole lists. -/
def setLeafDelta (n : N) (p : Path Seg) (old new : V) : ZSet (Fact N Seg V) :=
  Finsupp.single (n, p, new) 1 - Finsupp.single (n, p, old) 1

theorem wf_set_leaf {S : ZSet (Fact N Seg V)} (hS : WF kind S) {n : N} {p : Path Seg}
    {old new : V} (hk : kind p = .single) (hold : S (n, p, old) = 1) :
    WF kind (S + setLeafDelta n p old new) := by
  by_cases hon : old = new
  · subst hon; simpa [setLeafDelta] using hS
  -- `new` is not live before (functional dependency).
  have hnew0 : S (n, p, new) = 0 := by
    by_contra h
    exact hon (hS.fd n p old new hk (by rw [hold]; simp) h)
  have happ : ∀ x, (S + setLeafDelta n p old new) x =
      if x = (n, p, new) then 1 else if x = (n, p, old) then 0 else S x := by
    intro x
    simp only [setLeafDelta, Finsupp.add_apply, Finsupp.sub_apply, Finsupp.single_apply]
    by_cases h1 : x = (n, p, new)
    · subst h1
      have : (n, p, old) ≠ (n, p, new) := by simp [hon]
      simp [hnew0, this]
    · by_cases h2 : x = (n, p, old)
      · subst h2; simp [hold, Ne.symm h1, hon]
      · simp [Ne.symm h1, Ne.symm h2, h1, h2]
  refine ⟨?_, ?_, ?_⟩
  · intro x; rw [happ]; split_ifs
    · right; rfl
    · left; rfl
    · exact hS.isSet x
  · intro n' p' v w hk' hv hw
    rw [happ] at hv hw
    by_cases hkey : n' = n ∧ p' = p
    · obtain ⟨rfl, rfl⟩ := hkey
      -- at the changed key only `new` is live
      have : ∀ u, (if (n', p', u) = (n', p', new) then (1 : ℤ)
          else if (n', p', u) = (n', p', old) then 0 else S (n', p', u)) ≠ 0 → u = new := by
        intro u hu
        by_contra hne
        have h1 : (n', p', u) ≠ (n', p', new) := by simp [hne]
        rw [ite_eq_right h1] at hu
        by_cases h2 : u = old
        · subst h2; simp at hu
        · have h2' : (n', p', u) ≠ (n', p', old) := by simp [h2]
          rw [ite_eq_right h2'] at hu
          exact h2 (hS.fd n' p' u old hk' hu (by rw [hold]; simp))
      rw [this v hv, this w hw]
    · have hne1 : ∀ u, (n', p', u) ≠ (n, p, new) := by
        intro u h; simp at h; exact hkey ⟨h.1, h.2.1⟩
      have hne2 : ∀ u, (n', p', u) ≠ (n, p, old) := by
        intro u h; simp at h; exact hkey ⟨h.1, h.2.1⟩
      rw [ite_eq_right (hne1 v), ite_eq_right (hne2 v)] at hv
      rw [ite_eq_right (hne1 w), ite_eq_right (hne2 w)] at hw
      exact hS.fd n' p' v w hk' hv hw
  · apply hS.tree.mono_keys
    intro n' p' v hv
    rw [happ] at hv
    split_ifs at hv with h1 h2
    · simp at h1; obtain ⟨rfl, rfl, rfl⟩ := h1; exact ⟨old, by rw [hold]; simp⟩
    · exact absurd rfl hv
    · exact ⟨v, hv⟩

/-! ## Adding fresh facts: creating a node, setting an absent leaf, adding a map entry -/

/-- `T` adds facts whose paths are unrelated (neither prefix of the other, nor equal) to
every live path of the same node in `S`. -/
def Compatible (S T : ZSet (Fact N Seg V)) : Prop :=
  ∀ n p q v w, S (n, p, v) ≠ 0 → T (n, q, w) ≠ 0 → ¬ (p <+: q ∨ q <+: p)

/-- **Adding a fresh well-formed piece** (a new node's facts, a new map entry's subtree, an
absent leaf being set) preserves well-formedness. -/
theorem wf_add_fresh {S T : ZSet (Fact N Seg V)} (hS : WF kind S) (hT : WF kind T)
    (hc : Compatible S T) : WF kind (S + T) := by
  -- at every fact at most one side is live
  have hone : ∀ x, S x = 0 ∨ T x = 0 := by
    intro ⟨n, p, v⟩
    by_contra h; push Not at h
    exact hc n p p v v h.1 h.2 (Or.inl (List.prefix_refl p))
  have hlive : ∀ x, (S + T) x ≠ 0 → S x ≠ 0 ∨ T x ≠ 0 := by
    intro x hx; by_contra h; push Not at h; simp [h.1, h.2] at hx
  refine ⟨?_, ?_, ?_⟩
  · intro x; rw [Finsupp.add_apply]
    rcases hone x with h | h <;> simp [h, hS.isSet x, hT.isSet x]
  · intro n p v w hk hv hw
    rcases hlive _ hv with hv | hv <;> rcases hlive _ hw with hw | hw
    · exact hS.fd n p v w hk hv hw
    · exact absurd (Or.inl (List.prefix_refl p)) (hc n p p v w hv hw)
    · exact absurd (Or.inl (List.prefix_refl p)) (hc n p p w v hw hv)
    · exact hT.fd n p v w hk hv hw
  · intro n p q v w hv hw hpq
    rcases hlive _ hv with hv | hv <;> rcases hlive _ hw with hw | hw
    · exact hS.tree n p q v w hv hw hpq
    · exact hc n p q v w hv hw (Or.inl hpq.1)
    · exact hc n q p w v hw hv (Or.inr hpq.1)
    · exact hT.tree n p q v w hv hw hpq

theorem wf_single (x : Fact N Seg V) : WF kind (Finsupp.single x (1 : ℤ)) := by
  refine ⟨isSet_single x, ?_, ?_⟩
  · intro n p v w _ hv hw
    rw [Finsupp.single_apply] at hv hw
    split_ifs at hv hw with h1 h2
    · rw [h1] at h2; simp at h2; exact h2
    all_goals simp_all
  · intro n p q v w hv hw hpq
    rw [Finsupp.single_apply] at hv hw
    split_ifs at hv hw with h1 h2
    · rw [h1] at h2; simp at h2; exact hpq.2 h2.1
    all_goals simp_all

/-- Setting a leaf (scalar, list, or map entry with a leaf value) at a path that has nothing
live at, above, or below it. -/
theorem wf_add_leaf {S : ZSet (Fact N Seg V)} (hS : WF kind S) {n : N} {p : Path Seg} (v : V)
    (hfree : ∀ q w, S (n, q, w) ≠ 0 → ¬ (q <+: p ∨ p <+: q)) :
    WF kind (S + Finsupp.single (n, p, v) 1) := by
  apply wf_add_fresh hS (wf_single _)
  intro n' q q' w w' hS' hT
  rw [Finsupp.single_apply] at hT
  split_ifs at hT with h
  · simp at h; obtain ⟨rfl, rfl, rfl⟩ := h; exact hfree q w hS'
  · exact absurd rfl hT

/-! ## Set members (§10.2: a set member is its own fact) -/

/-- Adding a member `m` to the set at path `p`. The set path must not be under or above a
live leaf (it may coincide with other members' path). -/
theorem wf_add_member {S : ZSet (Fact N Seg V)} (hS : WF kind S) {n : N} {p : Path Seg}
    {m : V} (hk : kind p = .member) (habsent : S (n, p, m) = 0)
    (hfree : ∀ q w, S (n, q, w) ≠ 0 → ¬ (StrictPrefix q p ∨ StrictPrefix p q)) :
    WF kind (S + Finsupp.single (n, p, m) 1) := by
  have happ : ∀ x : Fact N Seg V, (S + Finsupp.single (n, p, m) (1 : ℤ)) x =
      if x = (n, p, m) then 1 else S x := by
    intro x
    rw [Finsupp.add_apply, Finsupp.single_apply]
    by_cases h : (n, p, m) = x
    · subst h; simp [habsent]
    · simp [h, Ne.symm h]
  have hlive : ∀ (n' : N) (q : Path Seg) (w : V),
      (S + Finsupp.single (n, p, m) (1 : ℤ)) (n', q, w) ≠ 0 →
      ((n', q, w) = (n, p, m)) ∨ S (n', q, w) ≠ 0 := by
    intro n' q w h; rw [happ] at h; split_ifs at h with h1
    · left; exact h1
    · right; exact h
  refine ⟨?_, ?_, ?_⟩
  · intro x; rw [happ]; split_ifs
    · right; rfl
    · exact hS.isSet x
  · intro n' q v w hk' hv hw
    rcases hlive _ _ _ hv with hv | hv
    · simp at hv; obtain ⟨rfl, rfl, rfl⟩ := hv; rw [hk] at hk'; cases hk'
    rcases hlive _ _ _ hw with hw | hw
    · simp at hw; obtain ⟨rfl, rfl, rfl⟩ := hw; rw [hk] at hk'; cases hk'
    exact hS.fd n' q v w hk' hv hw
  · intro n' q q' v w hv hw hpq
    rcases hlive _ _ _ hv with hv' | hv' <;> rcases hlive _ _ _ hw with hw' | hw'
    · simp at hv' hw'; obtain ⟨rfl, rfl, rfl⟩ := hv'; obtain ⟨_, rfl, rfl⟩ := hw'
      exact hpq.2 rfl
    · simp at hv'; obtain ⟨rfl, rfl, rfl⟩ := hv'; exact hfree q' w hw' (Or.inr hpq)
    · simp at hw'; obtain ⟨rfl, rfl, rfl⟩ := hw'; exact hfree q v hv' (Or.inl hpq)
    · exact hS.tree n' q q' v w hv' hw' hpq

/-- Removing a member of a set. -/
theorem wf_remove_member {S : ZSet (Fact N Seg V)} (hS : WF kind S) {n : N} {p : Path Seg}
    {m : V} (hm : S (n, p, m) = 1) : WF kind (S + -Finsupp.single (n, p, m) 1) :=
  wf_remove_fact hS _ hm

/-- An untyped `Document` field changing shape (e.g. object → scalar) is a subtree removal
followed by adding a fresh leaf: both steps preserve well-formedness. -/
theorem wf_replace_subtree_with_leaf {S : ZSet (Fact N Seg V)} (hS : WF kind S) (n : N)
    (p : Path Seg) (v : V)
    (hfree : ∀ q w, (S + removeSubtreeDelta S n p) (n, q, w) ≠ 0 → ¬ (q <+: p ∨ p <+: q)) :
    WF kind (S + removeSubtreeDelta S n p + Finsupp.single (n, p, v) 1) :=
  wf_add_leaf (wf_remove_subtree hS n p) v hfree

/-! ## Concurrent deltas (§9.2, §10.4, §12)

A causally closed version can contain *concurrent* dots: neither is in the other's context.
Both deltas were computed against (supersets of) a common state, and the state at the
version contains both. The sum is well-formed when the two deltas touch disjoint nodes —
which Shep gets from Postgres row-level write conflicts (each node is one row, §10.2
"Concurrency is still per node"). Without that side condition it fails. -/

/-- The nodes a delta touches. -/
def NodeDisjoint (Δ₁ Δ₂ : ZSet (Fact N Seg V)) : Prop :=
  ∀ x y, Δ₁ x ≠ 0 → Δ₂ y ≠ 0 → x.1 ≠ y.1

theorem wf_concurrent {S Δ₁ Δ₂ : ZSet (Fact N Seg V)}
    (h₁ : WF kind (S + Δ₁)) (h₂ : WF kind (S + Δ₂)) (hd : NodeDisjoint Δ₁ Δ₂) :
    WF kind (S + Δ₁ + Δ₂) := by
  -- on each node, the merged state agrees with one of the two branches
  have hnode : ∀ n, (∀ p v, (S + Δ₁ + Δ₂) (n, p, v) = (S + Δ₁) (n, p, v)) ∨
      (∀ p v, (S + Δ₁ + Δ₂) (n, p, v) = (S + Δ₂) (n, p, v)) := by
    intro n
    by_cases h : ∃ p v, Δ₂ (n, p, v) ≠ 0
    · right
      obtain ⟨p₀, v₀, h₀⟩ := h
      intro p v
      have : Δ₁ (n, p, v) = 0 := by
        by_contra hne; exact hd _ _ hne h₀ rfl
      simp [this, add_comm]
    · left
      push Not at h
      intro p v
      simp [h p v]
  refine ⟨?_, ?_, ?_⟩
  · intro ⟨n, p, v⟩
    rcases hnode n with h | h
    · rw [h]; exact h₁.isSet _
    · rw [h]; exact h₂.isSet _
  · intro n p v w hk hv hw
    rcases hnode n with h | h
    · rw [h] at hv hw; exact h₁.fd n p v w hk hv hw
    · rw [h] at hv hw; exact h₂.fd n p v w hk hv hw
  · intro n p q v w hv hw
    rcases hnode n with h | h
    · rw [h] at hv hw; exact h₁.tree n p q v w hv hw
    · rw [h] at hv hw; exact h₂.tree n p q v w hv hw

/-- Counterexample: two concurrent changes of the same field from the same old value. Each
alone is a valid transition; their sum gives weight `−1` on the old value. -/
theorem concurrent_same_field_not_set :
    let S : ZSet (Fact Unit Unit ℕ) := Finsupp.single ((), [], 0) 1
    let Δ₁ : ZSet (Fact Unit Unit ℕ) := setLeafDelta () [] 0 1
    let Δ₂ : ZSet (Fact Unit Unit ℕ) := setLeafDelta () [] 0 2
    WF (fun _ => Kind.single) (S + Δ₁) ∧ WF (fun _ => Kind.single) (S + Δ₂) ∧
      ¬ IsSet (S + Δ₁ + Δ₂) := by
  intro S Δ₁ Δ₂
  have hS : WF (N := Unit) (Seg := Unit) (V := ℕ) (fun _ => Kind.single) S := wf_single _
  refine ⟨wf_set_leaf hS rfl (by simp [S]), wf_set_leaf hS rfl (by simp [S]), ?_⟩
  intro h
  rcases h ((), [], 0) with h | h <;>
    simp [S, Δ₁, Δ₂, setLeafDelta] at h

end Shep
