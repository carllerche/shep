/-
# Validation soundness: a partition's log is serializable in log order (design.md §10.3)

## The model

A partition's log is a sequence of entries `L 0, L 1, …, L (n-1)`. Each entry has:

* `stamp`: the dot its facts carry;
* `S`: its read version (a set of dots);
* `hits`: its **read set**, as a predicate on facts `(k, v)`. A point read of key `k` matches
  every fact of `k`. A range read is a predicate, such as "facts of `Issue.state` whose value is
  `Open`" (§10.3);
* `wr`: its **write set**, a delta with old values: `wr k = some (old, new)`.

An entry **writes into** a read set if one of its facts matches it: the removed `(k, old)` or
the added `(k, new)` (`HitsW`).

Two states:

* `serial m`: the state after the first `m` entries, in log order;
* `readAt S m`: the **materializer's state at `S`** (§10.4): of the first `m` entries, only those
  whose stamp is in `S` are applied, so each key holds its newest value with a stamp in `S`.

A query `Q` is a **sound read** for `hits` if it gives the same answer on two states that hold
the same facts matching `hits` (`SoundRead`, `AgreeOn`).

## The hypotheses (`Validated`)

* `valid`: **the leader's check.** No earlier entry in the log whose stamp isn't in `S` writes
  into the read set. Earlier means every entry before it in the log, committed or not.
* `reads_writes`: every key an entry writes is in its read set (its delta carries the old value).
* `old_from_S`: a delta's old values are those read at `S`.
* `no_future`: `S` names no entry at or after the entry's own position (it was read before the
  entry was appended).
* `S_ctx`, `S_closed`: the context contains `S`, and `S` is causally closed.

## Results

* `validated_writes_ordered`: writes to one key are causally ordered, in log order. This is
  condition 3 of `wf_stateAt_closed` and `Fresh.History.WF.ordered`; `toHistory_wf` builds the
  freshness model's `History` from a validated log.
* `consistent_all`: every delta's old values are the true values at its log position.
* `serializable`, `reads_eq_serial`: **each entry's reads at `S` equal its reads at its log
  position**, so the log is conflict-serializable in log order.
* `committedOnly_unsound`: checking only committed entries, with pipelining, is unsound: two
  increments both read `0` and the log loses one.
-/
import ShepZSet.Freshness

noncomputable section
open Classical

namespace Shep
namespace Validation

variable {K Val Dot : Type*}

/-- A log entry. -/
structure Entry (K Val Dot : Type*) where
  stamp : Dot
  S : Finset Dot
  hits : K → Val → Prop
  wr : K → Option (Val × Val)

/-- Entry `e` writes key `k`. -/
def Writes (e : Entry K Val Dot) (k : K) : Prop := ∃ ov, e.wr k = some ov

/-- Entry `e` writes into read set `hits`: a fact it removes or adds matches it. -/
def HitsW (e : Entry K Val Dot) (hits : K → Val → Prop) : Prop :=
  ∃ k o v, e.wr k = some (o, v) ∧ (hits k o ∨ hits k v)

/-- Applying an entry: each written key takes its new value. -/
def apply (e : Entry K Val Dot) (σ : K → Val) : K → Val :=
  fun k => match e.wr k with
    | some (_, v) => v
    | none => σ k

theorem apply_some {e : Entry K Val Dot} {σ : K → Val} {k : K} {o v : Val}
    (h : e.wr k = some (o, v)) : apply e σ k = v := by simp [apply, h]

theorem apply_none {e : Entry K Val Dot} {σ : K → Val} {k : K}
    (h : e.wr k = none) : apply e σ k = σ k := by simp [apply, h]

variable (L : ℕ → Entry K Val Dot) (init : K → Val)

/-- The state after the first `m` entries, in log order. -/
def serial : ℕ → K → Val
  | 0 => init
  | m + 1 => apply (L m) (serial m)

/-- The materializer's state at `S` over the first `m` entries (§10.4): only entries whose
stamp is in `S` are applied, so each key holds its newest value stamped in `S`. -/
def readAt (S : Finset Dot) : ℕ → K → Val
  | 0 => init
  | m + 1 => if (L m).stamp ∈ S then apply (L m) (readAt S m) else readAt S m

/-- Two states hold the same facts matching `hits`. -/
def AgreeOn (hits : K → Val → Prop) (σ τ : K → Val) : Prop :=
  ∀ k, σ k = τ k ∨ (¬ hits k (σ k) ∧ ¬ hits k (τ k))

/-- `Q` reads only what `hits` covers: it gives the same answer on states that hold the same
facts matching `hits`. -/
def SoundRead {β : Type*} (Q : (K → Val) → β) (hits : K → Val → Prop) : Prop :=
  ∀ σ τ, AgreeOn hits σ τ → Q σ = Q τ

/-- A delta's old values are the true values at its log position. -/
def Consistent (j : ℕ) : Prop :=
  ∀ k o v, (L j).wr k = some (o, v) → o = serial L init j k

/-- The hypotheses: a log of `n` entries, each appended by the leader's check. -/
structure Validated (ctx : Dot → Finset Dot) (n : ℕ) : Prop where
  /-- The leader's check, against every earlier entry in the log, committed or not. -/
  valid : ∀ i < n, ∀ j < i, (L j).stamp ∉ (L i).S → ¬ HitsW (L j) (L i).hits
  /-- Every key an entry writes is in its read set. -/
  reads_writes : ∀ i < n, ∀ k o v, (L i).wr k = some (o, v) → ∀ x, (L i).hits k x
  /-- A delta's old values were read at its read version. -/
  old_from_S : ∀ i < n, ∀ k o v, (L i).wr k = some (o, v) → o = readAt L init (L i).S n k
  /-- The read version names no entry at or after the entry's own position. -/
  no_future : ∀ i < n, ∀ j, i ≤ j → j < n → (L j).stamp ∉ (L i).S
  /-- The context contains the read version. -/
  S_ctx : ∀ i < n, (L i).S ⊆ ctx (L i).stamp
  /-- The read version is causally closed. -/
  S_closed : ∀ i < n, CausallyClosed ctx (L i).S

variable {L init}
variable {ctx : Dot → Finset Dot} {n : ℕ}

/-- **Writes to one key are causally ordered, in log order.** If entries `j < j'` both write
`k`, then `j'` read `k`, so the check put `j`'s stamp in `j'`'s read version, and so in its
context. -/
theorem validated_writes_ordered (hV : Validated L init ctx n) {j j' : ℕ} (hjj : j < j')
    (hj' : j' < n) {k : K} (hwj : Writes (L j) k) (hwj' : Writes (L j') k) :
    (L j).stamp ∈ ctx (L j').stamp := by
  apply hV.S_ctx j' hj'
  by_contra hS
  obtain ⟨⟨o, v⟩, ho⟩ := hwj
  obtain ⟨⟨o', v'⟩, ho'⟩ := hwj'
  exact hV.valid j' hj' j hjj hS ⟨k, o, v, ho, Or.inl (hV.reads_writes j' hj' k o' v' ho' o)⟩

/-- Within a causally closed `S`, the writes to one key that `S` holds come first. -/
theorem prefix_of_valid (hV : Validated L init ctx n) {i : ℕ} (hi : i < n) {k : K} {j j' : ℕ}
    (hjj : j < j') (hj' : j' < i) (hwj : Writes (L j) k) (hwj' : Writes (L j') k)
    (hS : (L j').stamp ∈ (L i).S) : (L j).stamp ∈ (L i).S :=
  hV.S_closed i hi _ hS (validated_writes_ordered hV hjj (by omega) hwj hwj')

/-- The materializer's state at `S` stops changing once no later entry is stamped in `S`. -/
theorem readAt_stable {S : Finset Dot} {i : ℕ} (hfut : ∀ j, i ≤ j → j < n → (L j).stamp ∉ S) :
    ∀ m, i ≤ m → m ≤ n → readAt L init S m = readAt L init S i := by
  intro m him hmn
  induction m with
  | zero => have : i = 0 := by omega
            subst this; rfl
  | succ m ih =>
    rcases Nat.eq_or_lt_of_le him with h | h
    · rw [h]
    · have hm := hfut m (by omega) (by omega)
      simp only [readAt, hm, ↓reduceIte]
      exact ih (by omega) (by omega)

/-- The per-key invariant. Up to position `m ≤ i`, either every write to `k` so far is
stamped in `S` and both states agree on `k`, or some write isn't, and neither state's value of
`k` matches the read set. -/
theorem key_inv {S : Finset Dot} {hits : K → Val → Prop} {k : K} {i : ℕ}
    (hcons : ∀ j < i, Consistent L init j)
    (hval : ∀ j < i, (L j).stamp ∉ S → ¬ HitsW (L j) hits)
    (hpre : ∀ j j', j < j' → j' < i → Writes (L j) k → Writes (L j') k →
      (L j').stamp ∈ S → (L j).stamp ∈ S) :
    ∀ m, m ≤ i →
      (readAt L init S m k = serial L init m k ∧ ∀ j < m, Writes (L j) k → (L j).stamp ∈ S) ∨
      (¬ hits k (readAt L init S m k) ∧ ¬ hits k (serial L init m k) ∧
        ∃ j < m, Writes (L j) k ∧ (L j).stamp ∉ S) := by
  intro m hm
  induction m with
  | zero => exact Or.inl ⟨rfl, fun j hj => absurd hj (Nat.not_lt_zero _)⟩
  | succ m ih =>
    have ih := ih (by omega)
    have hmi : m < i := by omega
    cases hw : (L m).wr k with
    | none =>
      have hr : readAt L init S (m + 1) k = readAt L init S m k := by
        simp only [readAt]; split_ifs <;> simp [apply_none hw]
      have hs : serial L init (m + 1) k = serial L init m k := by
        simp [serial, apply_none hw]
      rw [hr, hs]
      rcases ih with ⟨he, hall⟩ | ⟨h1, h2, j, hj, hwj, hjS⟩
      · refine Or.inl ⟨he, fun j hj hwj => ?_⟩
        rcases Nat.lt_succ_iff_lt_or_eq.mp hj with hj | rfl
        · exact hall j hj hwj
        · obtain ⟨_, h⟩ := hwj; rw [hw] at h; cases h
      · exact Or.inr ⟨h1, h2, j, by omega, hwj, hjS⟩
    | some ov =>
      obtain ⟨o, v⟩ := ov
      have hwm : Writes (L m) k := ⟨_, hw⟩
      have hs : serial L init (m + 1) k = v := by simp [serial, apply_some hw]
      by_cases hS : (L m).stamp ∈ S
      · have hr : readAt L init S (m + 1) k = v := by simp [readAt, hS, apply_some hw]
        rw [hr, hs]
        rcases ih with ⟨_, hall⟩ | ⟨_, _, j, hj, hwj, hjS⟩
        · refine Or.inl ⟨rfl, fun j hj hwj => ?_⟩
          rcases Nat.lt_succ_iff_lt_or_eq.mp hj with hj | rfl
          · exact hall j hj hwj
          · exact hS
        · exact absurd (hpre j m hj hmi hwj hwm hS) hjS
      · have hr : readAt L init S (m + 1) k = readAt L init S m k := by simp [readAt, hS]
        rw [hr, hs]
        have hno : ¬ hits k o ∧ ¬ hits k v := by
          have := hval m hmi hS
          exact ⟨fun h => this ⟨k, o, v, hw, Or.inl h⟩, fun h => this ⟨k, o, v, hw, Or.inr h⟩⟩
        have ho : o = serial L init m k := hcons m hmi k o v hw
        refine Or.inr ⟨?_, hno.2, m, by omega, hwm, hS⟩
        rcases ih with ⟨he, _⟩ | ⟨h1, _, _⟩
        · rw [he, ← ho]; exact hno.1
        · exact h1

/-- The read state agrees with the serial state on the read set, for entry `i`, given that
every earlier delta's old values are true. -/
theorem agree_of_consistent (hV : Validated L init ctx n) {i : ℕ} (hi : i < n)
    (hcons : ∀ j < i, Consistent L init j) :
    AgreeOn (L i).hits (readAt L init (L i).S n) (serial L init i) := by
  intro k
  rw [readAt_stable (hV.no_future i hi) n (by omega) le_rfl]
  rcases key_inv hcons (fun j hj => hV.valid i hi j hj)
      (fun j j' hjj hj' hwj hwj' hS => prefix_of_valid hV hi hjj hj' hwj hwj' hS) i le_rfl with
    ⟨he, _⟩ | ⟨h1, h2, _⟩
  · exact Or.inl he
  · exact Or.inr ⟨h1, h2⟩

/-- **Every delta's old values are the true values at its log position.** -/
theorem consistent_all (hV : Validated L init ctx n) : ∀ i < n, Consistent L init i := by
  intro i
  induction i using Nat.strong_induction_on with
  | _ i ih =>
    intro hi k o v hw
    have hcons : ∀ j < i, Consistent L init j := fun j hj => ih j hj (by omega)
    rw [hV.old_from_S i hi k o v hw]
    rcases agree_of_consistent hV hi hcons k with h | ⟨h, _⟩
    · exact h
    · exact absurd (hV.reads_writes i hi k o v hw _) h

/-- **Validation soundness.** In a validated log, every entry's read state at its read version
holds the same facts on its read set as the serial state at its log position. -/
theorem serializable (hV : Validated L init ctx n) :
    ∀ i < n, AgreeOn (L i).hits (readAt L init (L i).S n) (serial L init i) :=
  fun i hi => agree_of_consistent hV hi (fun j hj => consistent_all hV j (by omega))

/-- **Each entry's reads at `S` equal its reads at its log position.** Any query that reads
only what the entry's read set covers gives the same answer at `S` as in the serial execution
in log order. So the log is conflict-serializable in log order. -/
theorem reads_eq_serial (hV : Validated L init ctx n) {i : ℕ} (hi : i < n) {β : Type*}
    {Q : (K → Val) → β} (hQ : SoundRead Q (L i).hits) :
    Q (readAt L init (L i).S n) = Q (serial L init i) :=
  hQ _ _ (serializable hV i hi)

/-! ## The freshness model's `History`, built from a validated log -/

/-- The history of a log: dot `d` writes what the entry stamped `d` writes. -/
def toHistory (L : ℕ → Entry K Val Dot) (init : K → Val) (ctx : Dot → Finset Dot) (n : ℕ) :
    Fresh.History K Val Dot where
  ctx := ctx
  write d k := if h : ∃ j, j < n ∧ (L j).stamp = d then ((L h.choose).wr k).map Prod.snd
    else none
  init := init

/-- **A validated log is a well-formed `History`** (`Fresh.History.WF`), given that causality is
a strict order. So the freshness theorems apply to it. (If two entries shared a stamp, the
history would keep one of them; Shep's local stamps are distinct.) -/
theorem toHistory_wf (hV : Validated L init ctx n) (hirr : ∀ d, d ∉ ctx d)
    (hclosed : ∀ d, CausallyClosed ctx (ctx d)) :
    (toHistory L init ctx n).WF where
  irrefl := hirr
  closed := hclosed
  ordered d e k hne hd he := by
    simp only [Fresh.History.Writes, toHistory] at hd he ⊢
    by_cases h1 : ∃ j, j < n ∧ (L j).stamp = d
    swap; · simp [h1] at hd
    by_cases h2 : ∃ j, j < n ∧ (L j).stamp = e
    swap; · simp [h2] at he
    rw [dite_eq_left_of_eq_true (eq_true h1)] at hd; rw [dite_eq_left_of_eq_true (eq_true h2)] at he
    obtain ⟨hj, hjd⟩ := h1.choose_spec
    obtain ⟨hj', hje⟩ := h2.choose_spec
    have hwd : Writes (L h1.choose) k := by
      cases hx : (L h1.choose).wr k with
      | none => simp [hx] at hd
      | some ov => exact ⟨ov, hx⟩
    have hwe : Writes (L h2.choose) k := by
      cases hx : (L h2.choose).wr k with
      | none => simp [hx] at he
      | some ov => exact ⟨ov, hx⟩
    rcases lt_trichotomy h1.choose h2.choose with hlt | heq | hgt
    · left; rw [← hjd, ← hje]; exact validated_writes_ordered hV hlt hj' hwd hwe
    · exact absurd (by rw [← hjd, ← hje, heq]) hne
    · right; rw [← hjd, ← hje]; exact validated_writes_ordered hV hgt hj hwe hwd

/-! ## Counterexample: checking only committed entries, with pipelining

Two increments of a counter `k` (one key, `Unit`), both read at `S = ∅`, where the counter is
`0`. Entry `0` writes `0 → 1`. With pipelining, entry `1` is validated while entry `0` is
appended but not yet committed (the commit index is `0`). A check of committed entries only
passes it, and entry `1` also writes `0 → 1`. Every other hypothesis holds. Entry `1` read `0`
at `S`, but at its log position the counter is `1`, so the log isn't serializable in log
order, and one increment is lost. The full check (every earlier entry) rejects it. -/

/-- The leader's check against the entries before position `c` only. -/
def ValidUpTo (L : ℕ → Entry K Val Dot) (c i : ℕ) : Prop :=
  ∀ j < c, (L j).stamp ∉ (L i).S → ¬ HitsW (L j) (L i).hits

/-- Both entries: stamp `i`, read version `∅`, read `k`, write `0 → 1`. -/
def incLog : ℕ → Entry Unit ℕ ℕ := fun i =>
  { stamp := i, S := ∅, hits := fun _ _ => True, wr := fun _ => some (0, 1) }

theorem committedOnly_unsound :
    let ctx : ℕ → Finset ℕ := fun _ => ∅
    -- the committed-only check passes entry 0 and entry 1 (commit index 0 at entry 1)
    ValidUpTo incLog 0 0 ∧ ValidUpTo incLog 0 1 ∧
    -- every other hypothesis of `Validated` holds
    (∀ i < 2, ∀ k o v, (incLog i).wr k = some (o, v) → ∀ x, (incLog i).hits k x) ∧
    (∀ i < 2, ∀ k o v, (incLog i).wr k = some (o, v) →
      o = readAt incLog (fun _ => 0) (incLog i).S 2 k) ∧
    (∀ i < 2, ∀ j, i ≤ j → j < 2 → (incLog j).stamp ∉ (incLog i).S) ∧
    (∀ i < 2, (incLog i).S ⊆ ctx (incLog i).stamp) ∧
    (∀ i < 2, CausallyClosed ctx (incLog i).S) ∧
    -- but entry 1's read at `S` differs from its read at its log position
    readAt incLog (fun _ => 0) (incLog 1).S 2 () = 0 ∧ serial incLog (fun _ => 0) 1 () = 1 ∧
    ¬ AgreeOn (incLog 1).hits (readAt incLog (fun _ => 0) (incLog 1).S 2)
      (serial incLog (fun _ => 0) 1) ∧
    -- two increments leave the counter at 1
    serial incLog (fun _ => 0) 2 () = 1 ∧
    -- the full check rejects entry 1
    ¬ ValidUpTo incLog 1 1 := by
  intro ctx
  have hr : readAt incLog (fun _ => 0) ∅ 2 () = 0 := by simp [readAt, incLog]
  have hs1 : serial incLog (fun _ => 0) 1 () = 1 := by simp [serial, apply, incLog]
  refine ⟨fun j hj => absurd hj (Nat.not_lt_zero _), fun j hj => absurd hj (Nat.not_lt_zero _),
    fun _ _ _ _ _ _ _ => trivial, ?_, fun _ _ _ _ _ => by simp [incLog],
    fun _ _ => by simp [incLog], fun _ _ => by simp [incLog, CausallyClosed], hr, hs1, ?_,
    by simp [serial, apply, incLog], ?_⟩
  · intro i _ k o v h
    simp only [incLog, Option.some.injEq, Prod.mk.injEq] at h
    rw [← h.1]; exact hr.symm
  · intro h
    rcases h () with h | ⟨h, _⟩
    · simp only [incLog] at h; rw [hr, hs1] at h; cases h
    · exact h trivial
  · intro h
    exact h 0 (by omega) (by simp [incLog]) ⟨(), 0, 1, rfl, Or.inl trivial⟩

end Validation
end Shep
