------------------------- MODULE BootstrapFoundTrace -------------------------
(* Short, deterministic witnesses using the original model's guarded actions.
   This restricts Next; it introduces no new founding or Raft transitions.
   Each config checks the bug property plus TraceIncomplete: reaching the
   end must violate the bug property, while a blocked script is not evidence.
   The typo script respects seeds_empty's ascending peer-query order. *)
EXTENDS BootstrapFound

CONSTANT Script
VARIABLE step
tvars == <<vars, step>>

Deliver(ty, s, r) ==
  \E m \in msgs : m.ty = ty /\ m.from = s /\ m.to = r /\ Recv(m)

Typos ==
  CASE step = 0 -> Hello(1, 2)
    [] step = 1 -> Hello(1, 3)
    [] step = 2 -> Crash(3, {3, 4})
    [] step = 3 -> Hello(3, 4)
    [] step = 4 -> Initialize(1)
    [] step = 5 -> Initialize(3)
    [] step = 6 -> Campaign(1)
    [] step = 7 -> SendRV(1, 2)
    [] step = 8 -> Deliver("rv", 1, 2)
    [] step = 9 -> Deliver("rvr", 2, 1)
    [] step = 10 -> Campaign(3)
    [] step = 11 -> SendRV(3, 4)
    [] step = 12 -> Deliver("rv", 3, 4)
    [] step = 13 -> Deliver("rvr", 4, 3)
    [] step = 14 -> AppendFound(1)
    [] step = 15 -> AppendFound(3)
    [] step = 16 -> SendAE(1, 2)
    [] step = 17 -> Deliver("ae", 1, 2)
    [] step = 18 -> Deliver("aer", 2, 1)
    [] step = 19 -> SendAE(3, 4)
    [] step = 20 -> Deliver("ae", 3, 4)
    [] step = 21 -> Deliver("aer", 4, 3)

WipedSeed ==
  CASE step = 0 -> Hello(1, 2)
    [] step = 1 -> Hello(1, 3)
    [] step = 2 -> Initialize(1)
    [] step = 3 -> Campaign(1)
    [] step = 4 -> SendRV(1, 2)
    [] step = 5 -> Deliver("rv", 1, 2)
    [] step = 6 -> Deliver("rvr", 2, 1)
    [] step = 7 -> AppendFound(1)
    [] step = 8 -> SendAE(1, 2)
    [] step = 9 -> Deliver("ae", 1, 2)
    [] step = 10 -> Deliver("aer", 2, 1)
    [] step = 11 -> Wipe(2)
    [] step = 12 -> SendAE(1, 2)
    [] step = 13 -> Deliver("ae", 1, 2)

(* Seed 3 has the membership but not the committed Found. Wiping seed 2
   makes the majority {2,3} forget the decision made by majority {1,2}. *)
Amnesia ==
  CASE step <= 6 -> WipedSeed
    [] step = 7 -> SendAE(1, 3)
    [] step = 8 -> Deliver("ae", 1, 3)
    [] step = 9 -> Deliver("aer", 3, 1)
    [] step = 10 -> AppendFound(1)
    [] step = 11 -> SendAE(1, 2)
    [] step = 12 -> Deliver("ae", 1, 2)
    [] step = 13 -> Deliver("aer", 2, 1)
    [] step = 14 -> Wipe(2)
    [] step = 15 -> Campaign(3)
    [] step = 16 -> SendRV(3, 2)
    [] step = 17 -> Deliver("rv", 3, 2)
    [] step = 18 -> Deliver("rvr", 2, 3)
    [] step = 19 -> AppendFound(3)
    [] step = 20 -> SendAE(3, 2)
    [] step = 21 -> Deliver("ae", 3, 2)
    [] step = 22 -> Deliver("aer", 2, 3)

Steps == CASE Script = "typo" -> 22 [] Script = "wipe" -> 14 [] OTHER -> 23
TInit == Init /\ step = 0
TNext == /\ step < Steps
         /\ CASE Script = "typo" -> Typos [] Script = "wipe" -> WipedSeed
                 [] OTHER -> Amnesia
         /\ step' = step + 1
TSpec == TInit /\ [][TNext]_tvars
TraceIncomplete == step < Steps
=============================================================================
