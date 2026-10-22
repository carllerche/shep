/-
# Adapter chains, the running system, moving the head, and facts (design.md §4.2–§4.4, §9.5, §10)

* `chain steps j d`: version `j`'s lens when the head is `j + d`, i.e.
  `L_j = A_{j+1} ; … ; A_{j+d}`.
* `chain_laws`: if every step writes exactly, every chain satisfies every law.
  `chain_weakLaws`: laws 1, 2, 6, and no-hidden-write hold for every chain whatever the
  normalizations.
* `run`: any interleaving of reads and writes from in-service versions keeps storage valid
  for the head. Each read is valid for its version. Each accepted write is local, reads
  back, and can be undone by its writer.
* `head_forward_*`: registering a new head with a backfill preserves what every older
  version reads (exactly when the backfill is lossless on the row, otherwise as the
  declared lossy image), by induction over any number of head changes.
* `rowFacts_patch`, `wf_write_transition`: a write through any lens is a delta of
  single-valued leaf facts, and a valid transition (condition 2 of `wf_stateAt_closed`).
-/
import ShepZSet.AdapterLibrary

noncomputable section
open Classical

namespace Shep.Adapters

variable {F V : Type*}

/-! ## Chains -/

/-- `steps k` is the step adapter `A_{k+1}`: storage side version `k+1`, view side version
`k`. `chain steps j d` is version `j`'s lens when the head is `j + d`. -/
def chain (steps : ℕ → Lens F V) : ℕ → ℕ → Lens F V
  | _, 0 => Lens.id
  | j, d + 1 => steps j ⨾ chain steps (j + 1) d

/-- The footprint of a chain: the union of the step footprints along it. -/
def chainWr (wr : ℕ → F → Set F) : ℕ → ℕ → F → Set F
  | _, 0 => fun g => {g}
  | j, d + 1 => compWr (wr j) (chainWr wr (j + 1) d)

variable {steps : ℕ → Lens F V} {S : ℕ → Set (Row F V)} {wr : ℕ → F → Set F}

/-- **Every chain satisfies every law when every step writes exactly.** No per-chain
re-proof is needed. -/
theorem chain_laws (hstep : ∀ k, Laws (steps k) (S (k + 1)) (S k) (wr k) exactNorm) :
    ∀ d j, Laws (chain steps j d) (S (j + d)) (S j) (chainWr wr j d) exactNorm := by
  intro d
  induction d with
  | zero => intro j; simpa [chain, chainWr] using id_laws (S j)
  | succ d ih =>
    intro j
    have h := comp_laws (hstep j) (ih (j + 1))
    rw [show j + 1 + d = j + (d + 1) by omega] at h
    exact h

/-- With a normalizing (lossy-write) step at the top of the chain only, the chain
satisfies every law with that step's normalization. -/
theorem chain_laws_top {n : F → V → V} (j d : ℕ)
    (htop : Laws (steps j) (S (j + 1)) (S j) (wr j) n)
    (hrest : ∀ k, j < k → Laws (steps k) (S (k + 1)) (S k) (wr k) exactNorm) :
    Laws (chain steps j (d + 1)) (S (j + (d + 1))) (S j) (chainWr wr j (d + 1)) n := by
  have hlow : ∀ d' j', j < j' → Laws (chain steps j' d') (S (j' + d')) (S j')
      (chainWr wr j' d') exactNorm := by
    intro d'
    induction d' with
    | zero => intro j' _; simpa [chain, chainWr] using id_laws (S j')
    | succ d' ih =>
      intro j' hj'
      have h := comp_laws (hrest j' hj') (ih (j' + 1) (by omega))
      rw [show j' + 1 + d' = j' + (d' + 1) by omega] at h
      exact h
  have h := comp_laws htop (hlow d (j + 1) (by omega))
  rw [show j + 1 + d = j + (d + 1) by omega] at h
  exact h

/-- Laws 1, 2, 6, and no-hidden-write. These survive composition with any normalizations. -/
structure WeakLaws (L : Lens F V) (SX SY : Set (Row F V)) (wr : F → Set F) : Prop where
  totalRead : ∀ x ∈ SX, ∀ y, L.get x = some y → y ∈ SY
  totalWrite : ∀ x ∈ SX, ∀ y, L.get x = some y → ∀ p, applyP p y ∈ SY →
    ∀ q, L.put p x = some q → applyP q x ∈ SX
  noHiddenWrite : L.NoHiddenWrite
  locality : ∀ x p q, L.put p x = some q → ∀ f ∈ domP q, ∃ g ∈ domP p, f ∈ wr g

theorem Laws.weak {L : Lens F V} {SX SY : Set (Row F V)} {w : F → Set F} {n : F → V → V}
    (h : Laws L SX SY w n) : WeakLaws L SX SY w :=
  ⟨h.totalRead, h.totalWrite, h.noHiddenWrite, h.locality⟩

/-- **Laws 1, 2, 6, and no-hidden-write hold for every chain, whatever the steps'
normalizations.** Only laws 4 and 5 can fail for a chain whose steps are all lawful
(`chain_not_lawful`). -/
theorem chain_weakLaws {n : ℕ → F → V → V}
    (hstep : ∀ k, Laws (steps k) (S (k + 1)) (S k) (wr k) (n k)) :
    ∀ d j, WeakLaws (chain steps j d) (S (j + d)) (S j) (chainWr wr j d) := by
  intro d
  induction d with
  | zero => intro j; simpa [chain, chainWr] using (id_laws (S j)).weak
  | succ d ih =>
    intro j
    have hA := hstep j
    have hB := ih (j + 1)
    rw [show j + 1 + d = j + (d + 1) by omega] at hB
    -- reuse the general composition lemmas through a `Laws`-free restatement
    refine ⟨?_, ?_, ?_, ?_⟩
    · intro x hx z hz
      simp only [chain, Lens.comp] at hz
      cases hy : (chain steps (j + 1) d).get x with
      | none => simp [hy] at hz
      | some y =>
        simp only [hy, Option.bind_some] at hz
        exact hA.totalRead y (hB.totalRead x hx y hy) z hz
    · intro x hx z hz p hp r hr
      simp only [chain, Lens.comp] at hz hr
      cases hy : (chain steps (j + 1) d).get x with
      | none => simp [hy] at hz
      | some y =>
        simp only [hy, Option.bind_some] at hz hr
        cases hq : (steps j).put p y with
        | none => simp [hq] at hr
        | some q =>
          simp only [hq, Option.bind_some] at hr
          have hyM := hB.totalRead x hx y hy
          exact hB.totalWrite x hx y hy q (hA.totalWrite y hyM z hz p hp q hq) r hr
    · intro x p hx
      simp only [chain, Lens.comp] at hx ⊢
      cases hy : (chain steps (j + 1) d).get x with
      | none => simp
      | some y =>
        simp only [hy, Option.bind_some] at hx ⊢
        simp [hA.noHiddenWrite y p hx]
    · intro x p r hr f hf
      simp only [chain, Lens.comp] at hr
      cases hy : (chain steps (j + 1) d).get x with
      | none => simp [hy] at hr
      | some y =>
        simp only [hy, Option.bind_some] at hr
        cases hq : (steps j).put p y with
        | none => simp [hq] at hr
        | some q =>
          simp only [hq, Option.bind_some] at hr
          obtain ⟨m, hm, hfm⟩ := hB.locality x q r hr f hf
          obtain ⟨g, hg, hmg⟩ := hA.locality y p q hq m hm
          exact ⟨g, hg, m, hmg, hfm⟩

/-- Appending a new head step at the storage end of a chain (associativity plus the
identity laws). -/
theorem chain_snoc (hnh : ∀ k, (steps k).NoHiddenWrite) :
    ∀ d j, chain steps j (d + 1) = chain steps j d ⨾ steps (j + d) := by
  intro d
  induction d with
  | zero =>
    intro j
    simp only [chain, Nat.add_zero]
    rw [Lens.comp_id_right, (Lens.comp_id_left_iff _).mpr (hnh j)]
  | succ d ih =>
    intro j
    rw [chain, ih (j + 1), ← Lens.comp_assoc, show j + 1 + d = j + (d + 1) by omega]
    rfl

/-! ## The running system -/

/-- A read or a write by some in-service version. A write can opt into `overwrite`
(§4.3), which skips the write guard. -/
inductive Op (ι F V : Type*)
  | read (v : ι)
  | write (v : ι) (p : Patch F V) (overwrite : Bool)

variable {ι : Type*} (lens : ι → Lens F V) (SV : ι → Set (Row F V))

/-- The server accepts a write if the row is visible to the version, the patch is valid
for the version, and the write guard passes (unless the write opts into `overwrite`). -/
def Accepts (v : ι) (p : Patch F V) (ow : Bool) (x : Row F V) : Prop :=
  ∃ y, (lens v).get x = some y ∧ applyP p y ∈ SV v ∧ (ow = true ∨ Rep (lens v) (domP p) x)

/-- One operation. Rejected writes leave storage unchanged. -/
def stepOp (x : Row F V) : Op ι F V → Row F V
  | .read _ => x
  | .write v p ow => if Accepts lens SV v p ow x then ((lens v).write p x).getD x else x

/-- Run a sequence of operations. -/
def run (x : Row F V) (ops : List (Op ι F V)) : Row F V := ops.foldl (stepOp lens SV) x

variable {lens SV} {SH : Set (Row F V)} {wrV : ι → F → Set F} {normV : ι → F → V → V}

/-- **Storage always stays valid for the head**, under any interleaving. -/
theorem run_valid (hL : ∀ v, WeakLaws (lens v) SH (SV v) (wrV v)) :
    ∀ (ops : List (Op ι F V)) (x : Row F V), x ∈ SH → run lens SV x ops ∈ SH := by
  intro ops
  induction ops with
  | nil => intro x hx; exact hx
  | cons op ops ih =>
    intro x hx
    apply ih
    cases op with
    | read v => exact hx
    | write v p ow =>
      simp only [stepOp]
      split_ifs with hacc
      · obtain ⟨y, hy, hp, _⟩ := hacc
        cases hq : (lens v).put p x with
        | none => simp [Lens.write, hq, hx]
        | some q =>
          simp only [Lens.write, hq, Option.map_some, Option.getD_some]
          exact (hL v).totalWrite x hx y hy p hp q hq
      · exact hx

/-- **Every read lands in the version's valid set, or is a declared `hide`.** -/
theorem read_valid (hL : ∀ v, WeakLaws (lens v) SH (SV v) (wrV v)) {x : Row F V}
    (hx : x ∈ SH) (v : ι) : (lens v).get x = none ∨ ∃ y, (lens v).get x = some y ∧ y ∈ SV v := by
  cases hy : (lens v).get x with
  | none => exact Or.inl rfl
  | some y => exact Or.inr ⟨y, rfl, (hL v).totalRead x hx y hy⟩

/-- An accepted write's outcome. -/
theorem stepOp_write_accepted {v : ι} {p : Patch F V} {ow : Bool} {x : Row F V}
    (hacc : Accepts lens SV v p ow x) :
    stepOp lens SV x (.write v p ow) = ((lens v).write p x).getD x := by
  simp [stepOp, hacc]

/-- **No silent loss.** For a guarded write (no `overwrite`) that the server accepts:
1. storage fields outside the write's footprint are unchanged;
2. the writer reads back what it wrote (normalized), and its other fields as before;
3. the writer can undo the write by writing back the old values, and the server accepts
   that undo. -/
theorem no_silent_loss (hL : ∀ v, Laws (lens v) SH (SV v) (wrV v) (normV v)) {v : ι}
    {p : Patch F V} {x : Row F V} (hx : x ∈ SH) {y : Row F V} (hy : (lens v).get x = some y)
    (hp : applyP p y ∈ SV v) (hrep : Rep (lens v) (domP p) x) {x₁ : Row F V}
    (hw : (lens v).write p x = some x₁) :
    stepOp lens SV x (.write v p false) = x₁ ∧
      (∀ f, (∀ g ∈ domP p, f ∉ wrV v g) → x₁ f = x f) ∧
      (lens v).get x₁ = some (applyP (normP (normV v) p) y) ∧
      (lens v).write (restrictP y (domP p)) x₁ = some x := by
  have hacc : Accepts lens SV v p false x := ⟨y, hy, hp, Or.inr hrep⟩
  refine ⟨by rw [stepOp_write_accepted hacc, hw]; rfl, fun f hf => write_local (hL v) hw f hf,
    (write_readback (hL v) hx hy hp hw).1, guard_undo (hL v) hx hy hp hrep hw⟩

/-- **With exact writes, a version can always edit what it wrote**: after an accepted write,
the written fields are representable for the writer. -/
theorem rep_after_write {L : Lens F V} {SX SY : Set (Row F V)} {w : F → Set F}
    (hL : Laws L SX SY w exactNorm) {x : Row F V} (hx : x ∈ SX) {y : Row F V}
    (hy : L.get x = some y) {p : Patch F V} (hp : applyP p y ∈ SY) {x₁ : Row F V}
    (hw : L.write p x = some x₁) : Rep L (domP p) x₁ := by
  simp only [Lens.write] at hw
  cases hq₁ : L.put p x with
  | none => simp [hq₁] at hw
  | some q₁ =>
    simp only [hq₁, Option.map_some, Option.some.injEq] at hw
    subst hw
    have hg := hL.putGet x hx y hy p hp q₁ hq₁
    rw [normP_exact] at hg
    -- writing `p` again over the new view is the same as writing `p`
    have hrp : restrictP (applyP p y) (domP p) = p := by
      funext f
      simp only [restrictP, domP, Set.mem_ofPred_eq, applyP]
      rcases Option.eq_none_or_eq_some (p f) with h | ⟨u, h⟩ <;> simp [h]
    have hmerge : mergeP p p = p := by
      funext f; simp only [mergeP]; cases p f <;> rfl
    have hpp := hL.putPut x hx y hy p p hp q₁ hq₁
    rw [hmerge, hq₁] at hpp
    cases hq₂ : L.put p (applyP q₁ x) with
    | none => simp [hq₂] at hpp
    | some q₂ =>
      simp only [hq₂, Option.map_some, Option.some.injEq] at hpp
      refine ⟨applyP p y, q₂, hg, by rw [hrp]; exact hq₂, ?_⟩
      have := congrArg (fun q => applyP q x) hpp
      simp only [applyP_merge] at this
      exact this.symm

/-! ## Moving the head forward -/

/-- **Reads after a head change.** Register `steps n` (new head `n+1`) with backfill `b`.
If the new step reads the backfilled row as `N x` (its declared image of the old row; `x`
itself when lossless on `x`), then every older version reads the backfilled row exactly as it
read `N x` before. -/
theorem head_forward_read (hnh : ∀ k, (steps k).NoHiddenWrite) {n : ℕ}
    {b N : Row F V → Row F V} (hb : ∀ x ∈ S n, (steps n).get (b x) = some (N x))
    {j d : ℕ} (hjd : j + d = n) {x : Row F V} (hx : x ∈ S n) :
    (chain steps j (d + 1)).get (b x) = (chain steps j d).get (N x) := by
  rw [chain_snoc hnh d j, hjd]
  simp [Lens.comp, hb x hx]

/-- Lossless rows read exactly as before. -/
theorem head_forward_read_lossless (hnh : ∀ k, (steps k).NoHiddenWrite) {n : ℕ}
    {b N : Row F V → Row F V} (hb : ∀ x ∈ S n, (steps n).get (b x) = some (N x))
    {j d : ℕ} (hjd : j + d = n) {x : Row F V} (hx : x ∈ S n) (hlossless : N x = x) :
    (chain steps j (d + 1)).get (b x) = (chain steps j d).get x := by
  rw [head_forward_read hnh hb hjd hx, hlossless]

/-- Iterated backfills over `m` head changes, starting from head `n`. -/
def iterBackfill (b : ℕ → Row F V → Row F V) (n : ℕ) : ℕ → Row F V → Row F V
  | 0, x => x
  | m + 1, x => b (n + m) (iterBackfill b n m x)

/-- **Any number of head changes.** If each backfill is valid and lossless on the data it
receives, then after `m` head changes every older version reads exactly what it read
before. -/
theorem head_forward_many (hnh : ∀ k, (steps k).NoHiddenWrite) {n : ℕ}
    {b : ℕ → Row F V → Row F V}
    (hvalid : ∀ k, n ≤ k → ∀ x ∈ S k, b k x ∈ S (k + 1))
    (hread : ∀ k, n ≤ k → ∀ x ∈ S k, (steps k).get (b k x) = some x)
    {j d : ℕ} (hjd : j + d = n) {x : Row F V} (hx : x ∈ S n) :
    ∀ m, iterBackfill b n m x ∈ S (n + m) ∧
      (chain steps j (d + m)).get (iterBackfill b n m x) = (chain steps j d).get x := by
  intro m
  induction m with
  | zero => exact ⟨hx, rfl⟩
  | succ m ih =>
    obtain ⟨hm, hget⟩ := ih
    refine ⟨?_, ?_⟩
    · simp only [iterBackfill]
      rw [show n + (m + 1) = n + m + 1 by omega]
      exact hvalid (n + m) (by omega) _ hm
    · simp only [iterBackfill]
      rw [show d + (m + 1) = (d + m) + 1 by omega, chain_snoc hnh (d + m) j,
        show j + (d + m) = n + m by omega]
      simp only [Lens.comp, hread (n + m) (by omega) _ hm, Option.bind_some]
      exact hget

/-- **What registration must check.** Suppose the existing steps write exactly and satisfy
the laws, and the new head step `steps n` writes exactly, satisfies the laws, and has a
backfill that is valid and lossless on existing data. Then every chain, old and new,
satisfies every law, and every older version reads unchanged data. No per-chain proof is
needed. If the new step's write is lossy, chains above a cross-field step can fail laws 4
and 5 (`chain_not_lawful`), so those must be re-proven per chain. -/
theorem registration_sufficient {n : ℕ}
    (hstep : ∀ k, Laws (steps k) (S (k + 1)) (S k) (wr k) exactNorm)
    {b : Row F V → Row F V} (hbv : ∀ x ∈ S n, b x ∈ S (n + 1))
    (hbr : ∀ x ∈ S n, (steps n).get (b x) = some x) :
    (∀ d j, Laws (chain steps j d) (S (j + d)) (S j) (chainWr wr j d) exactNorm) ∧
      ∀ j d, j + d = n → ∀ x ∈ S n,
        b x ∈ S (n + 1) ∧ (chain steps j (d + 1)).get (b x) = (chain steps j d).get x :=
  ⟨chain_laws hstep, fun _ _ hjd x hx =>
    ⟨hbv x hx, head_forward_read_lossless (fun k => (hstep k).noHiddenWrite)
      (N := id) hbr hjd hx rfl⟩⟩

end Shep.Adapters

/-! ## The connection to facts (§10.1–§10.2, §9.5 condition 2) -/

namespace Shep.Adapters

open Shep

variable {F V N : Type*} [Fintype F]

/-- A node's row as facts: one single-valued leaf `(n, [f], x f)` per field. -/
def rowFacts (n : N) (x : Row F V) : ZSet (Fact N F V) :=
  ∑ f, Finsupp.single (n, [f], x f) 1

/-- The delta of a storage patch: `−old +new` on every field the patch names. -/
def patchDelta (n : N) (x : Row F V) (q : Patch F V) : ZSet (Fact N F V) :=
  ∑ f, match q f with
    | none => 0
    | some v => setLeafDelta n [f] (x f) v

/-- **A write is a delta of field facts**: the facts of the new row are the old row's facts
plus the patch delta. -/
theorem rowFacts_patch (n : N) (x : Row F V) (q : Patch F V) :
    rowFacts n x + patchDelta n x q = rowFacts n (applyP q x) := by
  unfold rowFacts patchDelta
  rw [← Finset.sum_add_distrib]
  apply Finset.sum_congr rfl
  intro f _
  rcases Option.eq_none_or_eq_some (q f) with h | ⟨v, h⟩
  · simp [h, applyP]
  · simp only [h, setLeafDelta, applyP, Option.getD_some]; abel

theorem rowFacts_apply (n m : N) (x : Row F V) (p : Path F) (v : V) :
    rowFacts n x (m, p, v) = if m = n ∧ ∃ f, p = [f] ∧ x f = v then 1 else 0 := by
  unfold rowFacts
  rw [Finsupp.finsetSum_apply]
  rcases p with _ | ⟨f₀, _ | ⟨g, rest⟩⟩
  · simp
  · rw [Finset.sum_eq_single f₀]
    · simp only [Finsupp.single_apply, Prod.mk.injEq]
      by_cases hm : m = n
      · subst hm; by_cases hv : x f₀ = v <;> simp [hv]
      · simp [hm, Ne.symm hm]
    · intro f _ hf; simp [hf]
    · intro h; exact absurd (Finset.mem_univ f₀) h
  · simp

/-- Every path is single-valued (scalar and list leaves). -/
def kindAll : Path F → Kind := fun _ => .single

theorem wf_rowFacts (n : N) (x : Row F V) : WF kindAll (rowFacts n x) := by
  refine ⟨?_, ?_, ?_⟩
  · intro ⟨m, p, v⟩; rw [rowFacts_apply]; split_ifs <;> simp
  · intro m p v w _ hv hw
    rw [rowFacts_apply] at hv hw
    split_ifs at hv hw with h1 h2
    · obtain ⟨-, f, rfl, rfl⟩ := h1
      obtain ⟨-, f', hf, rfl⟩ := h2
      simp only [List.cons.injEq, and_true] at hf
      rw [hf]
    all_goals simp_all
  · intro m p q v w hv hw hpq
    rw [rowFacts_apply] at hv hw
    split_ifs at hv hw with h1 h2
    · obtain ⟨-, f, rfl, -⟩ := h1
      obtain ⟨-, f', rfl, -⟩ := h2
      exact hpq.2 (hpq.1.eq_of_length rfl)
    all_goals simp_all

/-- The rest of the state: facts of other nodes. -/
def OtherNodes (n : N) (T : ZSet (Fact N F V)) : Prop := ∀ m p v, T (m, p, v) ≠ 0 → m ≠ n

theorem wf_rowFacts_add {n : N} {T : ZSet (Fact N F V)} (hT : WF kindAll T)
    (hO : OtherNodes n T) (x : Row F V) : WF kindAll (rowFacts n x + T) := by
  apply wf_add_fresh (wf_rowFacts n x) hT
  intro m p q v w h1 h2 _
  rw [rowFacts_apply] at h1
  split_ifs at h1 with h
  · exact hO m q w h2 h.1
  · exact h1 rfl

/-- **Each component of the delta has a live old value**: the precondition of
`wf_set_leaf` holds for every field the write names. -/
theorem patchDelta_old_live {n : N} {T : ZSet (Fact N F V)} (hO : OtherNodes n T)
    (x : Row F V) (f : F) : (rowFacts n x + T) (n, [f], x f) = 1 := by
  rw [Finsupp.add_apply, rowFacts_apply, ite_eq_left ⟨rfl, f, rfl, rfl⟩]
  have : T (n, [f], x f) = 0 := by
    by_contra h; exact hO n [f] (x f) h rfl
  rw [this, add_zero]

/-- **A write through any lens is a valid transition on facts.** If the state is node `n`'s
row plus other nodes' facts, then adding the write's patch delta yields a well-formed
state: the row of the written storage plus the same other facts. This discharges
condition 2 of `wf_stateAt_closed` for writes from any version. -/
theorem wf_write_transition {n : N} {T : ZSet (Fact N F V)} (hT : WF kindAll T)
    (hO : OtherNodes n T) (x : Row F V) (q : Patch F V) :
    rowFacts n x + T + patchDelta n x q = rowFacts n (applyP q x) + T ∧
      WF kindAll (rowFacts n x + T + patchDelta n x q) := by
  have e : rowFacts n x + T + patchDelta n x q = rowFacts n (applyP q x) + T := by
    rw [← rowFacts_patch]; abel
  exact ⟨e, e ▸ wf_rowFacts_add hT hO _⟩

/-- Through a lens: an accepted write's storage row and its fact delta. -/
theorem lens_write_facts {L : Lens F V} {p : Patch F V} {x x₁ : Row F V}
    (hw : L.write p x = some x₁) (n : N) :
    ∃ q, L.put p x = some q ∧ rowFacts n x + patchDelta n x q = rowFacts n x₁ := by
  simp only [Lens.write] at hw
  cases hq : L.put p x with
  | none => simp [hq] at hw
  | some q =>
    simp only [hq, Option.map_some, Option.some.injEq] at hw
    exact ⟨q, rfl, hw ▸ rowFacts_patch n x q⟩

end Shep.Adapters
