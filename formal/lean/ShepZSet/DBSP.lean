/-
# DBSP incrementalization (design.md §11.5; Budiu et al., arXiv 2203.16684)

* Linear operators (filter, map/projection with summing, plus) commute with deltas.
* Join is bilinear, giving `Δ(A ⋈ B) = ΔA ⋈ B₁ + A₁ ⋈ ΔB + ΔA ⋈ ΔB`.
* `distinct` is not linear; its incremental form uses the old integrated value.
* Streams: delay `z⁻¹`, integration `I`, differentiation `D`, `D ∘ I = id`, `I ∘ D = id`,
  and the chain rule for incremental queries.
-/
import ShepZSet.Changes

noncomputable section
open Classical

namespace Shep

variable {α β γ δ κ : Type*}

/-! ## Linear operators -/

/-- A query is *linear* if it is additive on Z-sets. -/
def IsLinear (Q : ZSet α → ZSet β) : Prop := ∀ a b, Q (a + b) = Q a + Q b

/-- The additive group homomorphism underlying a linear query. -/
def IsLinear.toHom {Q : ZSet α → ZSet β} (h : IsLinear Q) : ZSet α →+ ZSet β :=
  AddMonoidHom.mk' Q h

theorem IsLinear.map_zero {Q : ZSet α → ZSet β} (h : IsLinear Q) : Q 0 = 0 :=
  h.toHom.map_zero

theorem IsLinear.map_neg {Q : ZSet α → ZSet β} (h : IsLinear Q) (a : ZSet α) :
    Q (-a) = -Q a := h.toHom.map_neg a

theorem IsLinear.map_sub {Q : ZSet α → ZSet β} (h : IsLinear Q) (a b : ZSet α) :
    Q (a - b) = Q a - Q b := h.toHom.map_sub a b

/-- **Linear operators commute with deltas**: `Q (S + Δ) - Q S = Q Δ`. -/
theorem linear_delta {Q : ZSet α → ZSet β} (h : IsLinear Q) (S Δ : ZSet α) :
    changes Q S (S + Δ) = Q Δ := by
  simp [changes, h S Δ]

/-- The same, in the §11.5 form: the state at `V1` is `S₂ - ΔI`. -/
theorem linear_delta' {Q : ZSet α → ZSet β} (h : IsLinear Q) (S₂ ΔI : ZSet α) :
    changes Q (S₂ - ΔI) S₂ = Q ΔI := by
  have := linear_delta h (S₂ - ΔI) ΔI
  simpa using this

/-- Filter (`WHERE`). -/
def zfilter (p : α → Prop) (S : ZSet α) : ZSet α := Finsupp.filter p S

theorem zfilter_linear (p : α → Prop) : IsLinear (zfilter p) := fun _ _ =>
  Finsupp.filter_add

/-- Map / projection, *summing* the weights of rows that collide (`SELECT f(x)` with bag
semantics, i.e. without `DISTINCT`). -/
def zmap (f : α → β) (S : ZSet α) : ZSet β := Finsupp.mapDomain f S

theorem zmap_linear (f : α → β) : IsLinear (zmap f) := fun _ _ =>
  Finsupp.mapDomain_add

/-- Plus (`UNION ALL`) of two linear queries is linear. -/
theorem plus_linear {Q₁ Q₂ : ZSet α → ZSet β} (h₁ : IsLinear Q₁) (h₂ : IsLinear Q₂) :
    IsLinear (fun S => Q₁ S + Q₂ S) := by
  intro a b; simp only [h₁ a b, h₂ a b]; abel

theorem neg_linear {Q : ZSet α → ZSet β} (h : IsLinear Q) : IsLinear (fun S => -Q S) := by
  intro a b; simp only [h a b]; abel

theorem comp_linear {Q₁ : ZSet β → ZSet γ} {Q₂ : ZSet α → ZSet β}
    (h₁ : IsLinear Q₁) (h₂ : IsLinear Q₂) : IsLinear (Q₁ ∘ Q₂) := by
  intro a b; simp [h₂ a b, h₁ (Q₂ a) (Q₂ b)]

theorem id_linear : IsLinear (id : ZSet α → ZSet α) := fun _ _ => rfl

/-- Projection does not preserve set-ness: two distinct rows with the same projected value
give weight `2`. Hence `project` in §11.5 is linear only with bag semantics; SQL
`SELECT DISTINCT` / `UNION` are `distinct ∘ (…)`, which is not linear (see below). -/
theorem zmap_not_preserves_set :
    ∃ (S : ZSet Bool) (f : Bool → Unit), IsSet S ∧ ¬ IsSet (zmap f S) := by
  refine ⟨Finsupp.single false 1 + Finsupp.single true 1, fun _ => (), ?_, ?_⟩
  · intro x; cases x <;> simp
  · intro h
    rcases h () with h | h <;>
      simp [zmap, Finsupp.mapDomain_add, Finsupp.mapDomain_single] at h

/-! ## Products and joins (bilinear) -/

/-- Cartesian product of Z-sets: weights multiply. -/
def zprod (A : ZSet α) (B : ZSet β) : ZSet (α × β) :=
  Finsupp.onFinset (A.support ×ˢ B.support) (fun p => A p.1 * B p.2) (by
    intro p hp
    simp only [Finset.mem_product, Finsupp.mem_support_iff]
    exact ⟨fun h => hp (by simp [h]), fun h => hp (by simp [h])⟩)

@[simp] theorem zprod_apply (A : ZSet α) (B : ZSet β) (p : α × β) :
    zprod A B p = A p.1 * B p.2 := rfl

/-- Equi-join on keys `kA`, `kB`: product restricted to matching keys. -/
def zjoin (kA : α → κ) (kB : β → κ) (A : ZSet α) (B : ZSet β) : ZSet (α × β) :=
  Finsupp.filter (fun p => kA p.1 = kB p.2) (zprod A B)

theorem zjoin_apply (kA : α → κ) (kB : β → κ) (A : ZSet α) (B : ZSet β) (p : α × β) :
    zjoin kA kB A B p = if kA p.1 = kB p.2 then A p.1 * B p.2 else 0 := by
  simp [zjoin, Finsupp.filter_apply]

/-- A binary operator is *bilinear* if it is additive in each argument. -/
def IsBilinear (J : ZSet α → ZSet β → ZSet γ) : Prop :=
  (∀ a₁ a₂ b, J (a₁ + a₂) b = J a₁ b + J a₂ b) ∧ (∀ a b₁ b₂, J a (b₁ + b₂) = J a b₁ + J a b₂)

theorem zjoin_bilinear (kA : α → κ) (kB : β → κ) : IsBilinear (zjoin kA kB) := by
  constructor
  · intro a₁ a₂ b; ext p; simp only [zjoin_apply, Finsupp.add_apply]; split_ifs <;> ring
  · intro a b₁ b₂; ext p; simp only [zjoin_apply, Finsupp.add_apply]; split_ifs <;> ring

theorem zprod_bilinear : IsBilinear (zprod : ZSet α → ZSet β → ZSet (α × β)) := by
  constructor
  · intro a₁ a₂ b; ext p; simp only [zprod_apply, Finsupp.add_apply]; ring
  · intro a b₁ b₂; ext p; simp only [zprod_apply, Finsupp.add_apply]; ring

/-- **Incremental bilinear operator**, in the §11.5 form (old values `A₁`, `B₁`):
`J(A₁+ΔA, B₁+ΔB) − J(A₁,B₁) = J(ΔA,B₁) + J(A₁,ΔB) + J(ΔA,ΔB)`. -/
theorem bilinear_delta {J : ZSet α → ZSet β → ZSet γ} (hJ : IsBilinear J)
    (A₁ ΔA : ZSet α) (B₁ ΔB : ZSet β) :
    J (A₁ + ΔA) (B₁ + ΔB) - J A₁ B₁ = J ΔA B₁ + J A₁ ΔB + J ΔA ΔB := by
  rw [hJ.1, hJ.2, hJ.2]; abel

/-- The same formula expressed against the *new* state (what a transaction at `V2` has):
`A₁ = A₂ − ΔA`, `B₁ = B₂ − ΔB`. Equivalently `ΔA ⋈ B₂ + A₂ ⋈ ΔB − ΔA ⋈ ΔB`. -/
theorem bilinear_delta_new {J : ZSet α → ZSet β → ZSet γ} (hJ : IsBilinear J)
    (A₂ ΔA : ZSet α) (B₂ ΔB : ZSet β) :
    J A₂ B₂ - J (A₂ - ΔA) (B₂ - ΔB) = J ΔA B₂ + J A₂ ΔB - J ΔA ΔB := by
  have h := bilinear_delta hJ (A₂ - ΔA) ΔA (B₂ - ΔB) ΔB
  simp only [sub_add_cancel] at h
  rw [h]
  have e1 : J ΔA (B₂ - ΔB) = J ΔA B₂ - J ΔA ΔB := by
    have := hJ.2 ΔA (B₂ - ΔB) ΔB; simp only [sub_add_cancel] at this
    rw [this]; abel
  have e2 : J (A₂ - ΔA) ΔB = J A₂ ΔB - J ΔA ΔB := by
    have := hJ.1 (A₂ - ΔA) ΔA ΔB; simp only [sub_add_cancel] at this
    rw [this]; abel
  rw [e1, e2]; abel

/-- The design's join formula, literally. -/
theorem join_delta (kA : α → κ) (kB : β → κ) (A₁ ΔA : ZSet α) (B₁ ΔB : ZSet β) :
    zjoin kA kB (A₁ + ΔA) (B₁ + ΔB) - zjoin kA kB A₁ B₁ =
      zjoin kA kB ΔA B₁ + zjoin kA kB A₁ ΔB + zjoin kA kB ΔA ΔB :=
  bilinear_delta (zjoin_bilinear kA kB) A₁ ΔA B₁ ΔB

/-- Join of sets is a set. -/
theorem zjoin_isSet (kA : α → κ) (kB : β → κ) {A : ZSet α} {B : ZSet β}
    (hA : IsSet A) (hB : IsSet B) : IsSet (zjoin kA kB A B) := by
  intro p
  rw [zjoin_apply]
  split_ifs
  · rcases hA p.1 with h | h <;> rcases hB p.2 with h' | h' <;> simp [h, h']
  · left; rfl

/-! ## `distinct` -/

/-- The weight function of `distinct`: positive weights become `1`, others `0`. -/
def pos1 (n : ℤ) : ℤ := if 0 < n then 1 else 0

/-- `distinct` (DBSP §4): keep the elements with positive weight, with weight `1`. -/
def distinct (S : ZSet α) : ZSet α := Finsupp.mapRange pos1 (by simp [pos1]) S

@[simp] theorem distinct_apply (S : ZSet α) (x : α) : distinct S x = pos1 (S x) := rfl

theorem distinct_isSet (S : ZSet α) : IsSet (distinct S) := by
  intro x; simp only [distinct_apply, pos1]; split_ifs <;> simp

theorem distinct_of_isSet {S : ZSet α} (h : IsSet S) : distinct S = S := by
  ext x; rcases h x with h | h <;> simp [pos1, h]

theorem distinct_idem (S : ZSet α) : distinct (distinct S) = distinct S :=
  distinct_of_isSet (distinct_isSet S)

/-- `distinct` is not linear. -/
theorem distinct_not_linear : ¬ IsLinear (distinct : ZSet Unit → ZSet Unit) := by
  intro h
  have := congrArg (fun S => S ()) (h (Finsupp.single () 1) (Finsupp.single () 1))
  simp [pos1] at this

/-- The incremental weight function of `distinct` (DBSP Prop. "H"): given the old integrated
weight `i` and the delta weight `d`, the output delta weight. -/
def distinctH (i d : ℤ) : ℤ :=
  if 0 < i ∧ i + d ≤ 0 then -1 else if i ≤ 0 ∧ 0 < i + d then 1 else 0

theorem distinctH_eq (i d : ℤ) : distinctH i d = pos1 (i + d) - pos1 i := by
  unfold distinctH pos1; split_ifs <;> omega

theorem distinctH_zero_right (i : ℤ) : distinctH i 0 = 0 := by
  rw [distinctH_eq]; simp

/-- The incremental `distinct` operator `H(I, Δ)`: it needs the *old integrated* input `I`
(not just the delta), but only at the elements in the support of `Δ`. -/
def distinctInc (I Δ : ZSet α) : ZSet α :=
  Finsupp.zipWith distinctH (distinctH_zero_right 0) I Δ

theorem distinctInc_apply (I Δ : ZSet α) (x : α) :
    distinctInc I Δ x = distinctH (I x) (Δ x) := by
  simp [distinctInc, Finsupp.zipWith_apply]

/-- **Incremental `distinct`**: `distinct (I + Δ) − distinct I = H(I, Δ)`. -/
theorem distinct_delta (I Δ : ZSet α) :
    changes distinct I (I + Δ) = distinctInc I Δ := by
  ext x
  simp [changes, distinctInc_apply, distinctH_eq]

/-- The cost of `H` is bounded by the delta: it is zero outside `Δ`'s support. -/
theorem distinctInc_support (I Δ : ZSet α) : (distinctInc I Δ).support ⊆ Δ.support := by
  intro x hx
  rw [Finsupp.mem_support_iff] at hx ⊢
  intro h0; apply hx
  rw [distinctInc_apply, h0, distinctH_zero_right]

/-! ## Streams, delay, integration, differentiation -/

variable {G H K : Type*} [AddCommGroup G] [AddCommGroup H] [AddCommGroup K]

/-- A stream of values (DBSP §2). -/
abbrev Stream' (G : Type*) := ℕ → G

/-- Delay `z⁻¹`: output `0` at time 0 and `s (t-1)` afterwards. -/
def delay (s : Stream' G) : Stream' G
  | 0 => 0
  | t + 1 => s t

/-- Integration `I`: running sum. -/
def integ (s : Stream' G) : Stream' G := fun t => ∑ i ∈ Finset.range (t + 1), s i

/-- Differentiation `D = 1 − z⁻¹`. -/
def diff (s : Stream' G) : Stream' G := fun t => s t - delay s t

theorem integ_succ (s : Stream' G) (t : ℕ) : integ s (t + 1) = integ s t + s (t + 1) := by
  simp [integ, Finset.sum_range_succ]

/-- `D ∘ I = id`. -/
theorem diff_integ (s : Stream' G) : diff (integ s) = s := by
  funext t
  cases t with
  | zero => simp [diff, delay, integ]
  | succ t => simp [diff, delay, integ_succ]

/-- `I ∘ D = id`. -/
theorem integ_diff (s : Stream' G) : integ (diff s) = s := by
  funext t
  induction t with
  | zero => simp [integ, diff, delay]
  | succ t ih => rw [integ_succ, ih]; simp [diff, delay]

/-- Lifting a function to streams, pointwise. -/
def lift (Q : G → H) (s : Stream' G) : Stream' H := fun t => Q (s t)

/-- The incremental version of a query: `Q^Δ = D ∘ ↑Q ∘ I`. -/
def incr (Q : G → H) (s : Stream' G) : Stream' H := diff (lift Q (integ s))

/-- For a linear (additive) query, `Q^Δ = ↑Q`: it applies directly to deltas. -/
theorem incr_linear (Q : G →+ H) (s : Stream' G) : incr Q s = lift Q s := by
  funext t
  cases t with
  | zero => simp [incr, diff, delay, lift, integ]
  | succ t => simp [incr, diff, delay, lift, integ_succ, map_add]

/-- **Chain rule**: `(Q₁ ∘ Q₂)^Δ = Q₁^Δ ∘ Q₂^Δ`. So a circuit's incremental form is built
operator by operator. -/
theorem incr_comp (Q₁ : H → K) (Q₂ : G → H) (s : Stream' G) :
    incr (Q₁ ∘ Q₂) s = incr Q₁ (incr Q₂ s) := by
  simp only [incr]
  rw [integ_diff]
  rfl

/-- Running the incremental query and integrating its output yields `Q` of the integrated
input at every time: a view maintained this way is exact. -/
theorem integ_incr (Q : G → H) (s : Stream' G) : integ (incr Q s) = lift Q (integ s) := by
  simp [incr, integ_diff]

end Shep
