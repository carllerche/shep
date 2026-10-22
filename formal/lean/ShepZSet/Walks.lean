/-
# Walks: bounded recursive traversal with privacy at every hop (design.md §7.1)

A walk repeats one hop (an edge type, in one direction) from a set of roots, breadth first,
up to a depth bound. It keeps a visited set, so each node is reported once, at the smallest
depth at which the walk reaches it. Each node is checked when the walk reaches it: a hidden
(or deleted, or placeholder) node is neither returned nor walked through.

## The model

* `E : N → Finset N` is the hop: the out-neighbours of a node along the walked edge.
  `stop_at` and edges with their own policies are both covered by choosing `E`: a stopping
  node has no out-neighbours, and a hidden edge is not in `E`.
* `vis : N → Prop` is visibility to the actor (`visible` of §6.5).
* `bfs` is the implementation: level `k + 1` is every visible, not yet seen out-neighbour of
  level `k`. Privacy is applied at the hop, before a node enters the visited set.

## Results

* `bfs_restrict`, `walk_restrict`: the walk equals a walk with **no** privacy checks over the
  subgraph restricted to visible nodes (roots and edges with both ends visible).
* `walk_congr_visible`: so the walk (results and depths) depends only on that subgraph. Any
  change to hidden nodes, or to edges into or out of them, is invisible: a hidden node never
  leaks the existence, count, or depth of anything beyond it.
* `bfs_spec`, `mem_walk`: the reported depth of a node is exactly the length of its shortest
  path from a root through visible nodes only (`VPath`). A shortcut through a hidden node
  can't make a visible node look closer.
* `budget_counting_hidden_leaks`: if the run-time budget that decides `truncated` counts the
  nodes a hop loads before privacy, two graphs with the same visible subgraph can differ in
  `truncated`. `visibleWork_congr`: a budget counting only returned (visible) nodes can't.
-/
import ShepZSet.Basic
import Mathlib.Data.Finset.Card

noncomputable section
open Classical

namespace Shep
namespace Walk

variable {N : Type*}

/-- One BFS step on `(level, seen)`: the next level is every visible out-neighbour of the
current level that hasn't been seen. -/
def step (E : N → Finset N) (vis : N → Prop) (p : Finset N × Finset N) :
    Finset N × Finset N :=
  ((p.1.biUnion E).filter (fun m => vis m ∧ m ∉ p.2),
    p.2 ∪ (p.1.biUnion E).filter (fun m => vis m ∧ m ∉ p.2))

/-- BFS state after `k` hops: `(nodes first reached at depth k, nodes reached at depth ≤ k)`.
The roots are checked too. -/
def bfs (E : N → Finset N) (vis : N → Prop) (roots : Finset N) : ℕ → Finset N × Finset N
  | 0 => (roots.filter vis, roots.filter vis)
  | k + 1 => step E vis (bfs E vis roots k)

/-- Nodes reported at depth `k`. -/
def level (E : N → Finset N) (vis : N → Prop) (roots : Finset N) (k : ℕ) : Finset N :=
  (bfs E vis roots k).1

/-- The walk's result: each reported node with its depth, up to `maxDepth`. -/
def walk (E : N → Finset N) (vis : N → Prop) (roots : Finset N) (maxDepth : ℕ) :
    Finset (N × ℕ) :=
  (Finset.range (maxDepth + 1)).biUnion
    (fun k => (level E vis roots k).image (fun m => (m, k)))

variable {E : N → Finset N} {vis : N → Prop} {roots : Finset N}

theorem bfs_fst_vis : ∀ k, ∀ m ∈ (bfs E vis roots k).1, vis m
  | 0, _, hm => (Finset.mem_filter.mp hm).2
  | _ + 1, _, hm => ((Finset.mem_filter.mp hm).2).1

/-! ## The walk equals a privacy-free walk over the visible subgraph -/

/-- The visible subgraph: edges whose both ends are visible. -/
def restrictE (E : N → Finset N) (vis : N → Prop) (n : N) : Finset N :=
  if vis n then (E n).filter vis else ∅

theorem step_restrict (p : Finset N × Finset N) (hp : ∀ n ∈ p.1, vis n) :
    step E vis p = step (restrictE E vis) (fun _ => True) p := by
  have : (p.1.biUnion E).filter (fun m => vis m ∧ m ∉ p.2) =
      (p.1.biUnion (restrictE E vis)).filter (fun m => True ∧ m ∉ p.2) := by
    ext m
    simp only [Finset.mem_filter, Finset.mem_biUnion, true_and]
    constructor
    · rintro ⟨⟨n, hn, hm⟩, hv, hs⟩
      refine ⟨⟨n, hn, ?_⟩, hs⟩
      simp only [restrictE, hp n hn, ↓reduceIte]
      exact Finset.mem_filter.mpr ⟨hm, hv⟩
    · rintro ⟨⟨n, hn, hm⟩, hs⟩
      simp only [restrictE, hp n hn, ↓reduceIte] at hm
      exact ⟨⟨n, hn, (Finset.mem_filter.mp hm).1⟩, (Finset.mem_filter.mp hm).2, hs⟩
  simp only [step, this]

/-- **The walk's BFS state equals that of a walk with no privacy checks over the visible
subgraph** (visible roots, edges between visible nodes). -/
theorem bfs_restrict (k : ℕ) :
    bfs E vis roots k = bfs (restrictE E vis) (fun _ => True) (roots.filter vis) k := by
  induction k with
  | zero => simp [bfs]
  | succ k ih =>
    simp only [bfs]
    rw [step_restrict _ (bfs_fst_vis k), ih]

theorem walk_restrict (d : ℕ) :
    walk E vis roots d = walk (restrictE E vis) (fun _ => True) (roots.filter vis) d := by
  have h := fun k => bfs_restrict (E := E) (vis := vis) (roots := roots) k
  simp only [walk, level, h]

/-- **No leak beyond a hidden node.** Two graphs with the same visible subgraph give the same
walk: the same nodes at the same depths. Hidden nodes, their edges, and everything reachable
only through them can be anything at all. -/
theorem walk_congr_visible {E₁ E₂ : N → Finset N} {roots₁ roots₂ : Finset N}
    (hE : ∀ n, vis n → (E₁ n).filter vis = (E₂ n).filter vis)
    (hr : roots₁.filter vis = roots₂.filter vis) (d : ℕ) :
    walk E₁ vis roots₁ d = walk E₂ vis roots₂ d := by
  have : restrictE E₁ vis = restrictE E₂ vis := by
    funext n; unfold restrictE; split_ifs with h
    · exact hE n h
    · rfl
  rw [walk_restrict (E := E₁), walk_restrict (E := E₂), this, hr]

/-! ## Depths are shortest visible-path lengths -/

/-- A path of `k` hops from a root to `m`, through visible nodes only. -/
inductive VPath (E : N → Finset N) (vis : N → Prop) (roots : Finset N) : ℕ → N → Prop
  | root {r : N} : r ∈ roots → vis r → VPath E vis roots 0 r
  | step {k : ℕ} {n m : N} : VPath E vis roots k n → m ∈ E n → vis m →
      VPath E vis roots (k + 1) m

theorem vpath_zero {m : N} : VPath E vis roots 0 m ↔ m ∈ roots ∧ vis m := by
  constructor
  · intro h; cases h with | root hr hv => exact ⟨hr, hv⟩
  · rintro ⟨hr, hv⟩; exact .root hr hv

theorem vpath_succ {k : ℕ} {m : N} :
    VPath E vis roots (k + 1) m ↔ ∃ n, VPath E vis roots k n ∧ m ∈ E n ∧ vis m := by
  constructor
  · intro h; cases h with | step hp he hv => exact ⟨_, hp, he, hv⟩
  · rintro ⟨n, hp, he, hv⟩; exact .step hp he hv

/-- **BFS correctness.** After `k` hops, the visited set is the set of nodes with a visible
path of length at most `k`, and level `k` is the set of nodes whose shortest visible path has
length exactly `k`. -/
theorem bfs_spec (k : ℕ) :
    (∀ m, m ∈ (bfs E vis roots k).2 ↔ ∃ j ≤ k, VPath E vis roots j m) ∧
    (∀ m, m ∈ (bfs E vis roots k).1 ↔ VPath E vis roots k m ∧ ∀ j < k, ¬ VPath E vis roots j m) := by
  induction k with
  | zero =>
    refine ⟨fun m => ?_, fun m => ?_⟩
    · simp only [bfs, Finset.mem_filter, Nat.le_zero, exists_eq_left, vpath_zero]
    · simp only [bfs, Finset.mem_filter, vpath_zero, Nat.not_lt_zero, IsEmpty.forall_iff,
        implies_true, and_true]
  | succ k ih =>
    obtain ⟨ihS, ihL⟩ := ih
    have hL : ∀ m, m ∈ (bfs E vis roots (k + 1)).1 ↔
        VPath E vis roots (k + 1) m ∧ ∀ j < k + 1, ¬ VPath E vis roots j m := by
      intro m
      simp only [bfs, step, Finset.mem_filter, Finset.mem_biUnion]
      constructor
      · rintro ⟨⟨n, hn, hm⟩, hv, hs⟩
        refine ⟨.step ((ihL n).mp hn).1 hm hv, fun j hj hp => hs ((ihS m).mpr ⟨j, by omega, hp⟩)⟩
      · rintro ⟨hp, hmin⟩
        obtain ⟨n, hpn, hm, hv⟩ := vpath_succ.mp hp
        refine ⟨⟨n, (ihL n).mpr ⟨hpn, fun j hj hpj => hmin (j + 1) (by omega) (.step hpj hm hv)⟩,
          hm⟩, hv, fun hs => ?_⟩
        obtain ⟨j, hj, hpj⟩ := (ihS m).mp hs
        exact hmin j (by omega) hpj
    refine ⟨fun m => ?_, hL⟩
    have hS : m ∈ (bfs E vis roots (k + 1)).2 ↔
        m ∈ (bfs E vis roots k).2 ∨ m ∈ (bfs E vis roots (k + 1)).1 := by
      simp only [bfs, step, Finset.mem_union]
    rw [hS, ihS, hL]
    constructor
    · rintro (⟨j, hj, hp⟩ | ⟨hp, -⟩)
      · exact ⟨j, by omega, hp⟩
      · exact ⟨k + 1, le_refl _, hp⟩
    · rintro ⟨j, hj, hp⟩
      by_cases hjk : j ≤ k
      · exact Or.inl ⟨j, hjk, hp⟩
      · have hj' : j = k + 1 := by omega
        subst hj'
        by_cases hsh : ∃ i < k + 1, VPath E vis roots i m
        · obtain ⟨i, hi, hpi⟩ := hsh
          exact Or.inl ⟨i, by omega, hpi⟩
        · simp only [not_exists, not_and] at hsh
          exact Or.inr ⟨hp, hsh⟩

/-- **The walk's result, exactly.** `m` is reported at depth `k` iff `k` is within the bound
and `k` is the length of the shortest path to `m` through visible nodes. -/
theorem mem_walk {d k : ℕ} {m : N} :
    (m, k) ∈ walk E vis roots d ↔
      k ≤ d ∧ VPath E vis roots k m ∧ ∀ j < k, ¬ VPath E vis roots j m := by
  simp only [walk, level, Finset.mem_biUnion, Finset.mem_range, Finset.mem_image,
    Prod.mk.injEq]
  constructor
  · rintro ⟨i, hi, n, hn, rfl, rfl⟩
    exact ⟨by omega, ((bfs_spec _).2 _).mp hn⟩
  · rintro ⟨hk, hp⟩
    exact ⟨k, by omega, m, ((bfs_spec k).2 m).mpr hp, rfl, rfl⟩

/-- Each node is reported at most once. -/
theorem walk_depth_unique {d k k' : ℕ} {m : N} (h₁ : (m, k) ∈ walk E vis roots d)
    (h₂ : (m, k') ∈ walk E vis roots d) : k = k' := by
  obtain ⟨-, hp, hmin⟩ := mem_walk.mp h₁
  obtain ⟨-, hp', hmin'⟩ := mem_walk.mp h₂
  by_contra hne
  rcases Nat.lt_or_gt_of_ne hne with h | h
  · exact hmin' k h hp
  · exact hmin k' h hp'

/-! ## The run-time budget is a side channel if it counts hidden nodes -/

/-- The nodes hop `k + 1` must load to run privacy checks: every out-neighbour of level `k`,
hidden or not. -/
def loaded (E : N → Finset N) (vis : N → Prop) (roots : Finset N) (k : ℕ) : ℕ :=
  ((level E vis roots k).biUnion E).card

/-- The visible work: nodes actually reported up to depth `d`. -/
def visibleWork (E : N → Finset N) (vis : N → Prop) (roots : Finset N) (d : ℕ) : ℕ :=
  (walk E vis roots d).card

/-- A budget over visible work leaks nothing about hidden nodes. -/
theorem visibleWork_congr {E₁ E₂ : N → Finset N}
    (hE : ∀ n, vis n → (E₁ n).filter vis = (E₂ n).filter vis) (d : ℕ) :
    visibleWork E₁ vis roots d = visibleWork E₂ vis roots d := by
  simp only [visibleWork, walk_congr_visible (roots₁ := roots) (roots₂ := roots) hE rfl d]

/-- **A budget that counts loaded nodes leaks.** Root `0` has one out-neighbour, `1`, which is
hidden, in the second graph and none in the first. The visible subgraphs, and so the walks,
are equal, but the second walk loads one node at hop 1 and the first loads none, so a budget
of `0` loaded nodes reports `truncated` in one and not the other. -/
theorem budget_counting_hidden_leaks :
    let vis : ℕ → Prop := fun n => n ≠ 1
    let E₁ : ℕ → Finset ℕ := fun _ => ∅
    let E₂ : ℕ → Finset ℕ := fun n => if n = 0 then {1} else ∅
    (∀ d, walk E₁ vis {0} d = walk E₂ vis {0} d) ∧
      ¬ (0 < loaded E₁ vis {0} 0) ∧ 0 < loaded E₂ vis {0} 0 := by
  intro vis E₁ E₂
  refine ⟨fun d => walk_congr_visible (fun n _ => ?_) rfl d, ?_, ?_⟩
  · simp only [E₁, E₂]
    split_ifs <;> ext x <;> simp [vis]
  · simp [loaded, E₁]
  · have : level E₂ vis {0} 0 = {0} := by
      ext x; simp [level, bfs, vis]; intro h; subst h; simp
    simp [loaded, this, E₂]

end Walk
end Shep
