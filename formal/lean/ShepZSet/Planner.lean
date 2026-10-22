/-
# Skipping a Stable guard preserves outcome-serializability (design.md §7.6, §8, §10.3)

Planner obligation 1, extending `Validation.serializable`. The planner leaves the reads of a
`require p` out of the read set when `p` is Stable. We prove the committed history is still
**outcome-serializable** in log order: every committed mutation's writes, and the outcomes of
its `require`s (ShepQL `assert`s), equal those at its log position.

## The model

The log model of `Validation.lean`. Each key has a discipline (`DiscK`):

* `frozen`: a frozen field or edge of some node;
* `permanent`: the lifecycle of a `shep::Permanent` node;
* `mutable`: everything else.

A distinguished value `bot` means "absent": the node isn't created yet. A frozen field that
creation leaves unset holds a separate "null" value, not `bot`, so `bot` really means "before
creation".

* **The discipline order** `DLe σ τ` (`σ ≤_D τ`): every disciplined key is `bot` at `σ`, or
  equal at `σ` and `τ`. So frozen facts of created nodes are equal, and a permanent lifecycle
  only grows from absent to present. Mutable keys are unconstrained.
* **The leader's discipline check** (`Respects`): an entry writes a disciplined key only from
  `bot`. That is the key-value form of `Disc.FrozenOK` and `Disc.PermOK`: frozen facts are
  written only by the creating entry, and a permanent lifecycle is written only by its
  creation, never by a delete or restore.
* **A mutation** has a body (its writes, a function of its reads) and a guard (`require p`).
  The body's reads are validated: it is a sound read of the read set (the taint rule, §7.6). The
  guard's reads are **not** in the read set. Each committed entry's guard was true at `S`.

## Results

* `respects_mono`: each entry that passes the check is `≤_D`-monotone: `σ ≤_D apply e σ`.
* `readAt_le_serial`: in a log whose old values are true (`Validation.consistent_all`), the
  state at any `S` is `≤_D` the serial state at any later position.
* `stable_guard_holds`: a guard that is upward-closed under `≤_D` and true at `S` is true at
  the entry's log position.
* `outcome_serializable`: **the obligation.** For a validated log of entries that pass the
  discipline check, with Stable guards skipped, each committed mutation's writes equal its
  body's writes at its log position, and each guard holds there. Its validated reads agree too
  (`Validation.serializable`).
* `gstab_sound`, `stab_upClosed`, `stab_neg_closed`: the syntactic Stable⁺/Stable⁻ is sound,
  **for two-valued ShepQL guards** (§4.0 item 1; restatement-round obligation 2). Positive
  inputs are frozen or permanent, negative inputs frozen. A value read carries a `??` default
  (totality), and that default is the implicit lifecycle read of §7.6: it reads the node's
  absence, so Stable⁺ needs `?? false` and Stable⁻ needs `?? true`. `exists` applies to
  lifecycles, which are never frozen, so `not exists` is never Stable⁺. The proof is a plain
  induction over `Bool`; CEL's error absorption is gone.
* `mutableLifecycle_unsound`: **counterexample.** `assert(exists x)` over a non-permanent `x`
  is true at `S`, and a concurrent delete makes it false at the entry's log position. Every
  hypothesis of `Validated` holds, because the guard's read isn't in the read set.
* `trueDefault_unsound`: **counterexample (doc finding).** `assert((x.f = 1) ?? true)` over a
  frozen `f` is true at `S`, where `x` doesn't exist yet, and false at the log position after
  `x`'s creation sets `f = 2`. Judging a `??` default only by its field's discipline would
  call it Stable⁺. With `?? false` it is Stable⁺ and fails at `S` already.
* `Planner.CEL` (superseded): the earlier three-valued CEL guards, `geval`, `GStab`,
  `gstab_sound`, `stab_upClosed`, `mutableLifecycle_unsound`, kept as history.
-/
import ShepZSet.Validation

noncomputable section
open Classical

namespace Shep
namespace Planner

open Validation

variable {K Val Dot : Type*}

/-- A key's declared discipline. -/
inductive DiscK
  | frozen
  | permanent
  | mutable
  deriving DecidableEq

/-- **The discipline order** `σ ≤_D τ`: each disciplined key is absent at `σ`, or equal. -/
def DLe (disc : K → DiscK) (bot : Val) (σ τ : K → Val) : Prop :=
  ∀ k, disc k ≠ .mutable → σ k = bot ∨ σ k = τ k

theorem dle_refl (disc : K → DiscK) (bot : Val) (σ : K → Val) : DLe disc bot σ σ :=
  fun _ _ => Or.inr rfl

theorem dle_trans {disc : K → DiscK} {bot : Val} {σ τ ρ : K → Val}
    (h₁ : DLe disc bot σ τ) (h₂ : DLe disc bot τ ρ) : DLe disc bot σ ρ := by
  intro k hk
  rcases h₁ k hk with h | h
  · exact Or.inl h
  · rcases h₂ k hk with h' | h'
    · exact Or.inl (h.trans h')
    · exact Or.inr (h.trans h')

/-- **The leader's discipline check**: a disciplined key is written only from absent. -/
def Respects (disc : K → DiscK) (bot : Val) (e : Entry K Val Dot) : Prop :=
  ∀ k o v, disc k ≠ .mutable → e.wr k = some (o, v) → o = bot

variable {disc : K → DiscK} {bot : Val}

/-- **Each entry that passes the check is `≤_D`-monotone**, applied where its old values are
true. -/
theorem respects_mono {e : Entry K Val Dot} {σ : K → Val} (hR : Respects disc bot e)
    (hold : ∀ k o v, e.wr k = some (o, v) → o = σ k) : DLe disc bot σ (apply e σ) := by
  intro k hk
  cases hw : e.wr k with
  | none => exact Or.inr (apply_none hw).symm
  | some ov =>
    obtain ⟨o, v⟩ := ov
    left
    rw [← hold k o v hw]
    exact hR k o v hk hw

variable {L : ℕ → Entry K Val Dot} {init : K → Val}

/-- **The state at any `S` is `≤_D` the serial state**, at every position `m`, in a log
whose old values are true and whose entries pass the check. -/
theorem readAt_le_serial {i : ℕ} (hcons : ∀ j < i, Consistent L init j)
    (hR : ∀ j < i, Respects disc bot (L j)) (S : Finset Dot) :
    ∀ m, m ≤ i → DLe disc bot (readAt L init S m) (serial L init m) := by
  intro m hm k hk
  induction m with
  | zero => exact Or.inr rfl
  | succ m ih =>
    have ih := ih (by omega)
    have hmi : m < i := by omega
    cases hw : (L m).wr k with
    | none =>
      have hr : readAt L init S (m + 1) k = readAt L init S m k := by
        simp only [readAt]; split_ifs <;> simp [apply_none hw]
      have hs : serial L init (m + 1) k = serial L init m k := by
        simp [serial, apply_none hw]
      rw [hr, hs]; exact ih
    | some ov =>
      obtain ⟨o, v⟩ := ov
      have hs : serial L init (m + 1) k = v := by simp [serial, apply_some hw]
      have ho : o = bot := hR m hmi k o v hk hw
      have hos : o = serial L init m k := hcons m hmi k o v hw
      by_cases hS : (L m).stamp ∈ S
      · right
        rw [hs]; simp [readAt, hS, apply_some hw]
      · have hr : readAt L init S (m + 1) k = readAt L init S m k := by simp [readAt, hS]
        rw [hr]
        rcases ih with h | h
        · exact Or.inl h
        · left; rw [h, ← hos, ho]

/-- A guard is **upward-closed** under `≤_D`: true at `σ` stays true at every `τ ≥_D σ`. -/
def UpClosed (disc : K → DiscK) (bot : Val) (g : (K → Val) → Prop) : Prop :=
  ∀ σ τ, DLe disc bot σ τ → g σ → g τ

variable {ctx : Dot → Finset Dot} {n : ℕ}

/-- **A Stable guard true at `S` is true at the entry's log position.** -/
theorem stable_guard_holds (hV : Validated L init ctx n) (hR : ∀ j < n, Respects disc bot (L j))
    {i : ℕ} (hi : i < n) {g : (K → Val) → Prop} (hg : UpClosed disc bot g)
    (hS : g (readAt L init (L i).S n)) : g (serial L init i) := by
  rw [readAt_stable (hV.no_future i hi) n (by omega) le_rfl] at hS
  exact hg _ _ (readAt_le_serial (fun j hj => consistent_all hV j (by omega))
    (fun j hj => hR j (by omega)) (L i).S i le_rfl) hS

/-- **Outcome-serializability with Stable guards skipped.** In a validated log whose entries
pass the discipline check, let entry `i` be the commit of a mutation:

* its writes are its body's writes at `S` (`hbody`), and the body reads only its read set
  (`hsound`);
* its guard held at `S` (`hguard`, or the mutation would have aborted) and is Stable
  (`hstable`), and its reads are left out of the read set.

Then, in log order, every committed mutation's writes equal its body's writes at its log
position, its guard holds there, and its validated reads agree. -/
theorem outcome_serializable (hV : Validated L init ctx n)
    (hR : ∀ j < n, Respects disc bot (L j))
    (body : ℕ → (K → Val) → (K → Option (Val × Val))) (guard : ℕ → (K → Val) → Prop)
    (hbody : ∀ i < n, (L i).wr = body i (readAt L init (L i).S n))
    (hsound : ∀ i < n, SoundRead (body i) (L i).hits)
    (hguard : ∀ i < n, guard i (readAt L init (L i).S n))
    (hstable : ∀ i < n, UpClosed disc bot (guard i)) :
    ∀ i < n, (L i).wr = body i (serial L init i) ∧ guard i (serial L init i) ∧
      AgreeOn (L i).hits (readAt L init (L i).S n) (serial L init i) := by
  intro i hi
  refine ⟨?_, stable_guard_holds hV hR hi (hstable i hi) (hguard i hi), serializable hV i hi⟩
  rw [hbody i hi]
  exact reads_eq_serial hV hi (hsound i hi)


/-! ## Syntactic Stable⁺, two-valued (ShepQL)

ShepQL guards are total (§4.0 item 1, `Totality.lean`): an `assert` condition has inferred
cardinality One, so a value read of a key that may be absent carries a `??` default. -/

/-- Guards: two-valued, over keys. -/
inductive Guard (K Val : Type*)
  | tt
  | ff
  /-- `(a test of k's value) ?? d`: the test `P` on `k`'s value, or `d` when `k` is absent
  (the node doesn't exist yet: the implicit lifecycle read). `P` sees a created node's unset
  field as its own "null" value. -/
  | val (k : K) (P : Val → Bool) (d : Bool)
  /-- `exists`: the lifecycle `k` is present -/
  | ex (k : K)
  | not (g : Guard K Val)
  | and (g h : Guard K Val)
  | or (g h : Guard K Val)

/-- Two-valued evaluation. -/
def geval (bot : Val) (σ : K → Val) : Guard K Val → Bool
  | .tt => true
  | .ff => false
  | .val k P d => if σ k = bot then d else P (σ k)
  | .ex k => decide (σ k ≠ bot)
  | .not g => !geval bot σ g
  | .and g h => geval bot σ g && geval bot σ h
  | .or g h => geval bot σ g || geval bot σ h

/-- `GStab b g`: Stable⁺ for `b = true`, Stable⁻ for `b = false`. A value read is both
positive and negative, so it needs a frozen key; and **its `??` default must be the opposite
of `b`** (`?? false` for Stable⁺, `?? true` for Stable⁻), since the default reads the node's
absence, a negative lifecycle input. `exists` needs a frozen or permanent key in positive
position and is never Stable⁻: lifecycles are never frozen. -/
def GStab (disc : K → DiscK) : Bool → Guard K Val → Prop
  | _, .tt => True
  | _, .ff => True
  | b, .val k _ d => disc k = .frozen ∧ d = !b
  | true, .ex k => disc k ≠ .mutable
  | false, .ex _ => False
  | b, .not g => GStab disc (!b) g
  | b, .and g h => GStab disc b g ∧ GStab disc b h
  | b, .or g h => GStab disc b g ∧ GStab disc b h

theorem gstab_sound {σ τ : K → Val} (hle : DLe disc bot σ τ) :
    ∀ (g : Guard K Val) (b : Bool), GStab disc b g → geval bot σ g = b → geval bot τ g = b := by
  intro g
  induction g with
  | tt => intro b _ h; exact h
  | ff => intro b _ h; exact h
  | val k P d =>
    intro b hs h
    obtain ⟨hfr, hd⟩ := hs
    have hk : disc k ≠ .mutable := by rw [hfr]; decide
    simp only [geval] at h ⊢
    by_cases hb : σ k = bot
    · simp only [hb, ↓reduceIte, hd] at h; cases b <;> simp at h
    · simp only [hb, ↓reduceIte] at h
      rcases hle k hk with h' | h'
      · exact absurd h' hb
      · rw [← h']; simp only [hb, ↓reduceIte]; exact h
  | ex k =>
    intro b hs h
    cases b with
    | false => exact absurd hs (by simp [GStab])
    | true =>
      simp only [geval, decide_eq_true_eq] at h ⊢
      rcases hle k hs with h' | h'
      · exact absurd h' h
      · rw [← h']; exact h
  | not g ih =>
    intro b hs h
    simp only [geval] at h ⊢
    rw [ih (!b) hs (by rw [← h]; simp)]
    simp
  | and g h ihg ihh =>
    intro b hs hev
    simp only [geval] at hev ⊢
    cases b with
    | true =>
      rw [Bool.and_eq_true] at hev
      rw [ihg true hs.1 hev.1, ihh true hs.2 hev.2]; rfl
    | false =>
      rcases hg : geval bot σ g
      · rw [ihg false hs.1 hg]; rfl
      · rw [hg, Bool.true_and] at hev
        rw [ihh false hs.2 hev, Bool.and_false]
  | or g h ihg ihh =>
    intro b hs hev
    simp only [geval] at hev ⊢
    cases b with
    | false =>
      rw [Bool.or_eq_false_iff] at hev
      rw [ihg false hs.1 hev.1, ihh false hs.2 hev.2]; rfl
    | true =>
      rcases hg : geval bot σ g
      · rw [hg, Bool.false_or] at hev
        rw [ihh true hs.2 hev, Bool.or_true]
      · rw [ihg true hs.1 hg]; rfl

/-- **Syntactic Stable⁺ is upward-closed**: an `assert` that passes at `σ` passes at every
`τ ≥_D σ`. -/
theorem stab_upClosed {g : Guard K Val} (hs : GStab disc true g) :
    UpClosed disc bot (fun σ => geval bot σ g = true) :=
  fun _ _ hle h => gstab_sound hle g true hs h

/-- And Stable⁻ is closed downward in the same sense: a deny-polarity guard false at `σ` stays
false. -/
theorem stab_neg_closed {g : Guard K Val} (hs : GStab disc false g) :
    UpClosed disc bot (fun σ => geval bot σ g = false) :=
  fun _ _ hle h => gstab_sound hle g false hs h

/-! ## Counterexamples

Both use one log. Keys: `false` is a key of node `x` (its lifecycle or a field; `0` is absent),
and `true` is an output key. Entry `0` writes key `false` from `o` to `v`. Entry `1` runs at
`S = ∅`, checks its guard, and writes the output key, which is its whole read set. Every
hypothesis of `Validated` holds, because the guard's read isn't in the read set. -/

def cxLog (o v : ℕ) : ℕ → Entry Bool ℕ ℕ
  | 0 => { stamp := 0, S := ∅, hits := fun k _ => k = false,
           wr := fun k => if k = false then some (o, v) else none }
  | _ => { stamp := 1, S := ∅, hits := fun k _ => k = true,
           wr := fun k => if k = true then some (0, 1) else none }

def cxInit (o : ℕ) : Bool → ℕ := fun k => if k = false then o else 0

theorem cxLog_validated (o v : ℕ) : Validated (cxLog o v) (cxInit o) (fun _ => ∅) 2 := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro i hi j hj _ ⟨k, o', v', hw, hk⟩
    have : i = 1 := by omega
    subst this
    have : j = 0 := by omega
    subst this
    simp only [cxLog] at hw hk
    by_cases h : k = false
    · subst h; simp at hk
    · simp [h] at hw
  · intro i hi k o' v' hw x
    match i, hi with
    | 0, _ => simp only [cxLog] at hw ⊢; by_contra h; simp [h] at hw
    | 1, _ => simp only [cxLog] at hw ⊢; by_contra h; simp [h] at hw
  · intro i hi k o' v' hw
    match i, hi with
    | 0, _ =>
      simp only [cxLog] at hw
      by_cases h : k = false
      · subst h; simp at hw; rw [← hw.1]; simp [readAt, cxLog, cxInit]
      · simp [h] at hw
    | 1, _ =>
      simp only [cxLog] at hw
      by_cases h : k = true
      · subst h; simp at hw; rw [← hw.1]; simp [readAt, cxLog, cxInit]
      · simp [h] at hw
  · intro i hi j hij hj
    match i, j, hi, hj with
    | 0, 0, _, _ => simp [cxLog]
    | 0, 1, _, _ => simp [cxLog]
    | 1, 1, _, _ => simp [cxLog]
  · intro i hi; match i, hi with
    | 0, _ => simp [cxLog]
    | 1, _ => simp [cxLog]
  · intro i hi; match i, hi with
    | 0, _ => simp [cxLog, CausallyClosed]
    | 1, _ => simp [cxLog, CausallyClosed]

/-! ### `assert(exists x)` over a mutable lifecycle

Key `false` is `x`'s lifecycle (mutable; `1` live; a deleted node reads as absent, so a delete
writes `0`). Entry `0` deletes `x`; entry `1` asserts `exists x`. -/

def mlDisc : Bool → DiscK := fun _ => .mutable

def mlGuard : Guard Bool ℕ := .ex false

theorem mutableLifecycle_unsound :
    Validated (cxLog 1 0) (cxInit 1) (fun _ => ∅) 2 ∧
    (∀ j < 2, Respects mlDisc 0 (cxLog 1 0 j)) ∧
    -- the guard reads a mutable lifecycle, so it isn't Stable⁺
    ¬ GStab mlDisc true mlGuard ∧
    -- it is true at `S` and false at the entry's log position
    geval 0 (readAt (cxLog 1 0) (cxInit 1) (cxLog 1 0 1).S 2) mlGuard = true ∧
    geval 0 (serial (cxLog 1 0) (cxInit 1) 1) mlGuard = false := by
  refine ⟨cxLog_validated 1 0, ?_, by simp [GStab, mlGuard, mlDisc], ?_, ?_⟩
  · intro j _ k o v hk _
    exact absurd rfl hk
  · simp [geval, readAt, cxLog, cxInit, mlGuard]
  · simp [geval, serial, apply, cxLog, mlGuard]

/-! ### A `?? true` default in an `assert` (**finding**)

Key `false` is the frozen field `f` of a node `x` that doesn't exist at `S` (`0`, absent).
Entry `0` creates `x` with `f = 2`. Entry `1` asserts `(x.f = 1) ?? true`: true at `S`, where
`x` is absent, and false at its log position. The field is frozen, so a classifier that judged
the `??` default only by the field's discipline ("`??` defaults: both polarities", §7.6) would
call the guard Stable⁺ and skip it. The default reads the node's *absence*, a negative
lifecycle input, so a Stable⁺ guard needs `?? false` (and a Stable⁻ one `?? true`), the same
default the checker suggests by polarity (§4.0 item 1). -/

def tdDisc : Bool → DiscK := fun k => if k = false then .frozen else .mutable

/-- `(x.f = 1) ?? d`. -/
def tdGuard (d : Bool) : Guard Bool ℕ := .val false (fun v => decide (v = 1)) d

theorem trueDefault_unsound :
    Validated (cxLog 0 2) (cxInit 0) (fun _ => ∅) 2 ∧
    (∀ j < 2, Respects tdDisc 0 (cxLog 0 2 j)) ∧
    -- `f` is frozen, but the guard's default is `true`, so it isn't Stable⁺
    tdDisc false = .frozen ∧ ¬ GStab tdDisc true (tdGuard true) ∧
    -- true at `S`, false at the entry's log position
    geval 0 (readAt (cxLog 0 2) (cxInit 0) (cxLog 0 2 1).S 2) (tdGuard true) = true ∧
    geval 0 (serial (cxLog 0 2) (cxInit 0) 1) (tdGuard true) = false ∧
    -- with `?? false` the guard is Stable⁺, and the `assert` fails at `S` already
    GStab tdDisc true (tdGuard false) ∧
    geval 0 (readAt (cxLog 0 2) (cxInit 0) (cxLog 0 2 1).S 2) (tdGuard false) = false := by
  refine ⟨cxLog_validated 0 2, ?_, rfl, by simp [GStab, tdGuard], ?_, ?_, ⟨rfl, rfl⟩, ?_⟩
  · intro j hj k o v hk hw
    match j, hj with
    | 0, _ =>
      simp only [cxLog] at hw
      by_cases h : k = false
      · subst h; simp at hw; exact hw.1.symm
      · simp [h] at hw
    | 1, _ =>
      simp only [cxLog] at hw
      by_cases h : k = true
      · subst h; simp [tdDisc] at hk
      · simp [h] at hw
  · simp [geval, readAt, cxLog, cxInit, tdGuard]
  · simp [geval, serial, apply, cxLog, tdGuard]
  · simp [geval, readAt, cxLog, cxInit, tdGuard]

/-! ## Superseded: CEL guards

The earlier model of guards as CEL, three-valued with error absorption. **Superseded** by the
two-valued `Guard` above (§4.0 item 1); kept as history, in namespace `Planner.CEL`. -/

namespace CEL

/-- Guards: CEL over keys, three-valued (`none` is an error). -/
inductive Guard (K Val : Type*)
  | tt
  | ff
  /-- a test on `k`'s value; an error if `k` is absent (the implicit lifecycle read) -/
  | val (k : K) (P : Val → Prop)
  /-- `exists`: the lifecycle `k` is present -/
  | ex (k : K)
  | not (g : Guard K Val)
  | and (g h : Guard K Val)
  | or (g h : Guard K Val)

/-- CEL semantics: `&&` and `||` are commutative, and `false`/`true` absorb errors. -/
def geval (bot : Val) (σ : K → Val) : Guard K Val → Option Bool
  | .tt => some true
  | .ff => some false
  | .val k P => if σ k = bot then none else some (decide (P (σ k)))
  | .ex k => some (decide (σ k ≠ bot))
  | .not g => (geval bot σ g).map (fun b => !b)
  | .and g h =>
    match geval bot σ g, geval bot σ h with
    | some false, _ => some false
    | _, some false => some false
    | some true, some true => some true
    | _, _ => none
  | .or g h =>
    match geval bot σ g, geval bot σ h with
    | some true, _ => some true
    | _, some true => some true
    | some false, some false => some false
    | _, _ => none

/-- `GStab b g`: Stable⁺ for `b = true`, Stable⁻ for `b = false`. A value read is both
positive and negative, so it needs a frozen key. `exists` needs a frozen or permanent key in
positive position and is never Stable⁻: lifecycles are never frozen. -/
def GStab (disc : K → DiscK) : Bool → Guard K Val → Prop
  | _, .tt => True
  | _, .ff => True
  | _, .val k _ => disc k = .frozen
  | true, .ex k => disc k ≠ .mutable
  | false, .ex _ => False
  | b, .not g => GStab disc (!b) g
  | b, .and g h => GStab disc b g ∧ GStab disc b h
  | b, .or g h => GStab disc b g ∧ GStab disc b h

theorem gstab_sound {σ τ : K → Val} (hle : DLe disc bot σ τ) :
    ∀ (g : Guard K Val) (b : Bool), GStab disc b g → geval bot σ g = some b →
      geval bot τ g = some b := by
  intro g
  induction g with
  | tt => intro b _ h; exact h
  | ff => intro b _ h; exact h
  | val k P =>
    intro b hs h
    have hk : disc k ≠ .mutable := by rw [hs]; decide
    simp only [geval] at h ⊢
    by_cases hb : σ k = bot
    · simp [hb] at h
    · rcases hle k hk with h' | h'
      · exact absurd h' hb
      · rw [← h']; exact h
  | ex k =>
    intro b hs h
    cases b with
    | false => exact absurd hs (by simp [GStab])
    | true =>
      simp only [geval, Option.some.injEq, decide_eq_true_eq] at h ⊢
      rcases hle k hs with h' | h'
      · exact absurd h' h
      · rw [← h']; exact h
  | not g ih =>
    intro b hs h
    simp only [geval] at h ⊢
    cases hg : geval bot σ g with
    | none => simp [hg] at h
    | some c =>
      rw [hg] at h
      simp only [Option.map_some, Option.some.injEq] at h
      have : c = !b := by rw [← h]; simp
      subst this
      rw [ih (!b) hs hg]
      simp
  | and g h ihg ihh =>
    intro b hs hev
    simp only [geval] at hev ⊢
    cases b with
    | true =>
      rcases hg : geval bot σ g with _ | _ | _ <;> rcases hh : geval bot σ h with _ | _ | _ <;>
        simp only [hg, hh] at hev <;> try cases hev
      rw [ihg true hs.1 hg, ihh true hs.2 hh]
    | false =>
      rcases hg : geval bot σ g with _ | _ | _
      · rcases hh : geval bot σ h with _ | _ | _ <;> simp only [hg, hh] at hev <;> try cases hev
        rw [ihh false hs.2 hh]; rcases geval bot τ g with _ | _ | _ <;> rfl
      · rw [ihg false hs.1 hg]
      · rcases hh : geval bot σ h with _ | _ | _ <;> simp only [hg, hh] at hev <;> try cases hev
        rw [ihh false hs.2 hh]; rcases geval bot τ g with _ | _ | _ <;> rfl
  | or g h ihg ihh =>
    intro b hs hev
    simp only [geval] at hev ⊢
    cases b with
    | false =>
      rcases hg : geval bot σ g with _ | _ | _ <;> rcases hh : geval bot σ h with _ | _ | _ <;>
        simp only [hg, hh] at hev <;> try cases hev
      rw [ihg false hs.1 hg, ihh false hs.2 hh]
    | true =>
      rcases hg : geval bot σ g with _ | _ | _
      · rcases hh : geval bot σ h with _ | _ | _ <;> simp only [hg, hh] at hev <;> try cases hev
        rw [ihh true hs.2 hh]; rcases geval bot τ g with _ | _ | _ <;> rfl
      · rcases hh : geval bot σ h with _ | _ | _ <;> simp only [hg, hh] at hev <;> try cases hev
        rw [ihh true hs.2 hh]; rcases geval bot τ g with _ | _ | _ <;> rfl
      · rw [ihg true hs.1 hg]

/-- **Syntactic Stable⁺ is upward-closed**: a `require` that passes at `σ` (evaluates to
`true`) passes at every `τ ≥_D σ`. -/
theorem stab_upClosed {g : Guard K Val} (hs : GStab disc true g) :
    UpClosed disc bot (fun σ => geval bot σ g = some true) :=
  fun _ _ hle h => gstab_sound hle g true hs h

theorem mutableLifecycle_unsound :
    Validated (cxLog 1 0) (cxInit 1) (fun _ => ∅) 2 ∧
    (∀ j < 2, Respects mlDisc 0 (cxLog 1 0 j)) ∧
    ¬ GStab mlDisc true (.ex false : Guard Bool ℕ) ∧
    geval 0 (readAt (cxLog 1 0) (cxInit 1) (cxLog 1 0 1).S 2) (.ex false : Guard Bool ℕ) =
      some true ∧
    geval 0 (serial (cxLog 1 0) (cxInit 1) 1) (.ex false : Guard Bool ℕ) = some false := by
  refine ⟨cxLog_validated 1 0, ?_, by simp [GStab, mlDisc], ?_, ?_⟩
  · intro j _ k o v hk _
    exact absurd rfl hk
  · simp [geval, readAt, cxLog, cxInit]
  · simp [geval, serial, apply, cxLog]

end CEL

end Planner
end Shep
