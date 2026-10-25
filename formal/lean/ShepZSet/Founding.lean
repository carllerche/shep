/-
Controller founding and disk amnesia, accompanying BootstrapFound.tla.

This is an abstract protocol argument, not a refinement proof of the Rust
Raft engine. The first theorem states exactly what overlapping durable
pledges buy. The examples isolate why a disk wipe invalidates that premise,
and why clearing the volatile proposal strands the current pledge candidate.
Uses only Lean's standard library; no axioms or admitted proofs.
-/
import Std

namespace Shep.Founding

/-- Two founding sets sharing a seed with one durable pledge must choose
the same identity. The hypothesis requires that the pledge survive both
observations, including any intervening restart. -/
theorem overlapping_pledges_unique {Node Id : Type}
    (pledge : Node → Option Id) (a b : Id) (left right : Node → Prop)
    (ha : ∀ n, left n → pledge n = some a)
    (hb : ∀ n, right n → pledge n = some b)
    (overlap : ∃ n, left n ∧ right n) : a = b := by
  obtain ⟨n, hn, hm⟩ := overlap
  exact Option.some.inj ((ha n hn).symm.trans (hb n hm))

/-- The persistent state relevant to a Raft vote; an empty disk resets all
four fields. Node and log identifiers are abstract natural numbers. -/
structure VoteState where
  term : Nat
  votedFor : Option Nat
  lastTerm : Nat
  lastIndex : Nat
  deriving DecidableEq

def emptyDisk : VoteState := ⟨0, none, 0, 0⟩

def canGrant (s : VoteState) (candidate term lastTerm lastIndex : Nat) : Prop :=
  term ≥ s.term ∧
  (term > s.term ∨ s.votedFor = none ∨ s.votedFor = some candidate) ∧
  (lastTerm > s.lastTerm ∨ (lastTerm = s.lastTerm ∧ lastIndex ≥ s.lastIndex))

/-- A seed that voted for node 1 and stored its entry correctly refuses
node 3's older log. Reusing that seed's identity after a wipe permits the
same-term vote: the two different majorities may share this one seed. -/
theorem wipe_allows_conflicting_vote :
    ¬ canGrant ⟨1, some 1, 1, 1⟩ 3 1 0 0 ∧
    canGrant emptyDisk 3 1 0 0 := by
  simp [canGrant, emptyDisk]

/-- The proposed promise protocol's HELLO guard, with 0 denoting no id.
`proposal` is volatile; `pledge` is durable (BootstrapFound.tla, Hello). -/
def helloAllowed (pledge proposal nextId : Nat) : Prop :=
  pledge = 0 ∨ pledge = (if proposal ≠ 0 then proposal else nextId)

def initializeAllowed (pledge proposal : Nat) : Prop :=
  proposal ≠ 0 ∧ pledge = proposal

/-- After the first HELLO reserves id 1 and increments nextId to 2, a
restart clears proposal. With no expiry, neither local founding action
is enabled. Recovery must reload the reserved proposal, or fail closed
and require intervention. This concerns the unlanded candidate fix. -/
theorem restart_strands_pledged_initializer :
    helloAllowed 1 1 2 ∧ initializeAllowed 1 1 ∧
    ¬ helloAllowed 1 0 2 ∧ ¬ initializeAllowed 1 0 := by
  simp [helloAllowed, initializeAllowed]

/-- Reloading a nonzero reserved proposal from the durable pledge restores
both local guards, independently of the next fresh ID. This checks only
the proposed recovery's local guards, not global safety or liveness. -/
theorem restore_proposal_enables_founding (reserved nextId : Nat)
    (h : reserved ≠ 0) :
    helloAllowed reserved reserved nextId ∧ initializeAllowed reserved reserved := by
  simp [helloAllowed, initializeAllowed, h]

end Shep.Founding
