---------------------------- MODULE ChangeQueries ----------------------------
(***************************************************************************)
(* Shep §11: changes(Q, V1, V2) = Q(V2) - Q(V1) over Z-sets, observe, the  *)
(* hot window and resync, and an app that holds (cached result, version).  *)
(*                                                                         *)
(* One PG node. Facts 1..NF; a mutation toggles one fact (+1 or -1, from   *)
(* its snapshot). Mutations by other writers run concurrently with the     *)
(* app's, so dots commit out of order; aborted dots are void.              *)
(*                                                                         *)
(* Q = count of present facts; its result is the one-row Z-set             *)
(* {count |-> 1}, so a change is "-old +new" (not linear in the input).    *)
(*                                                                         *)
(* The server computes a delta the way §11.5 says: state at V2 is the      *)
(* current snapshot; state at V1 is V2 minus the deltas of the log entries *)
(* in V2 \ V1 that are still in the hot log. Entries <= Horizon have been  *)
(* archived and dropped (§10.4).                                           *)
(*                                                                         *)
(* SinceSource = "cache" : the app sends its cache's version as `since`.   *)
(* SinceSource = "handle": the app sends its Db handle's merged token, as  *)
(*   §9.4 describes ("client libraries merge tokens ... and send them back *)
(*   as at_least or since").                                               *)
(* WindowCheck = "vector": a since token is answered with a delta iff      *)
(*   every archived committed dot is in it (its frontier >= the horizon).  *)
(* WindowCheck = "max": iff its highest dot is >= the horizon (a natural   *)
(*   but wrong reading of "older than the hot window").                    *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS NF, MaxDot, Writers, SinceSource, WindowCheck

Dots == 1..MaxDot
Facts == 1..NF
Counts == 0..NF

VARIABLES
  ctr, ent,        \* counter; ent[n] = [st, k (fact), w (weight)]
  wpc, wsnap, wdot,\* other writers: state, snapshot, dot
  horizon,         \* dots <= horizon are archived (not in the hot log)
  apc, asnap, adot, aobs, \* the app's in-flight mutation (with or without observe)
  cache, cver,     \* the app's cached Q result and the version it is at
  htok             \* the app's Db handle token (merge of every token received)

vars == <<ctr, ent, wpc, wsnap, wdot, horizon, apc, asnap, adot, aobs, cache, cver, htok>>

One(c) == [i \in Counts |-> IF i = c THEN 1 ELSE 0]
ZAdd(a, b) == [i \in Counts |-> a[i] + b[i]]
ZSub(a, b) == [i \in Counts |-> a[i] - b[i]]

Init ==
  /\ ctr = 0
  /\ ent = [n \in Dots |-> [st |-> "none", k |-> 1, w |-> 0]]
  /\ wpc = [x \in Writers |-> "idle"]
  /\ wsnap = [x \in Writers |-> {}]
  /\ wdot = [x \in Writers |-> 0]
  /\ horizon = 0
  /\ apc = "idle" /\ asnap = {} /\ adot = 0 /\ aobs = FALSE
  /\ cache = One(0)
  /\ cver = {}
  /\ htok = {}

Committed == {n \in Dots : ent[n].st = "committed"}
Resolved(n) == ent[n].st \in {"committed", "void"}

RECURSIVE SumW(_)
SumW(S) == IF S = {} THEN 0
           ELSE LET n == CHOOSE n \in S : TRUE IN ent[n].w + SumW(S \ {n})

FactW(k, S) == SumW({n \in S : ent[n].k = k})
\* Q at version S
QAt(S) == SumW(S)

\* a mutation toggling fact k, from snapshot S, gets weight:
Toggle(k, S) == IF FactW(k, S) > 0 THEN -1 ELSE 1

\* first committer wins on the fact's row
Conflicts(n, S) == \E m \in Committed : ent[m].k = ent[n].k /\ m \notin S

(*************************** the server's change query *********************)

MaxOf(S) == CHOOSE m \in S : \A x \in S : x <= m

InWindow(V1) ==
  IF WindowCheck = "vector"
    THEN \A n \in 1..horizon : n \in Committed => n \in V1
    ELSE horizon = 0 \/ (V1 /= {} /\ MaxOf(V1) >= horizon)

\* changes(Q, V1, V2) as computed from the hot log; V2 is a state version
\* (here: the snapshot, or the snapshot plus the mutation's own dot)
Answer(V1, V2) ==
  IF InWindow(V1)
    THEN LET hot == {n \in V2 \ V1 : n > horizon}
             q1 == QAt(V2) - SumW(hot)
         IN [resync |-> FALSE, z |-> ZSub(One(QAt(V2)), One(q1))]
    ELSE [resync |-> TRUE, z |-> One(QAt(V2))]

Since == IF SinceSource = "cache" THEN cver ELSE htok

\* the app's cache after a response
Applied(r) == IF r.resync THEN r.z ELSE ZAdd(cache, r.z)

(*************************** other writers *********************************)

WBegin(x, k) ==
  /\ wpc[x] = "idle"
  /\ ctr < MaxDot
  /\ ctr' = ctr + 1
  /\ ent' = [ent EXCEPT ![ctr + 1] = [st |-> "pending", k |-> k, w |-> Toggle(k, Committed)]]
  /\ wsnap' = [wsnap EXCEPT ![x] = Committed]
  /\ wdot' = [wdot EXCEPT ![x] = ctr + 1]
  /\ wpc' = [wpc EXCEPT ![x] = "mut"]
  /\ UNCHANGED <<horizon, apc, asnap, adot, aobs, cache, cver, htok>>

WEnd(x) ==
  LET n == wdot[x] IN
  /\ wpc[x] = "mut"
  /\ ent' = [ent EXCEPT ![n].st = IF Conflicts(n, wsnap[x]) THEN "void" ELSE "committed"]
  /\ wpc' = [wpc EXCEPT ![x] = "idle"]
  /\ UNCHANGED <<ctr, wsnap, wdot, horizon, apc, asnap, adot, aobs, cache, cver, htok>>

(*************************** the app ***************************************)

\* changes(Q, since)
Poll ==
  /\ apc = "idle"
  /\ LET r == Answer(Since, Committed) IN
       /\ cache' = Applied(r)
       /\ cver' = Committed
       /\ htok' = htok \cup Committed
  /\ UNCHANGED <<ctr, ent, wpc, wsnap, wdot, horizon, apc, asnap, adot, aobs>>

\* a mutation, with an observe clause (obs) or without
ABegin(k, obs) ==
  /\ apc = "idle"
  /\ ctr < MaxDot
  /\ ctr' = ctr + 1
  /\ ent' = [ent EXCEPT ![ctr + 1] = [st |-> "pending", k |-> k, w |-> Toggle(k, Committed)]]
  /\ asnap' = Committed
  /\ adot' = ctr + 1
  /\ aobs' = obs
  /\ apc' = "mut"
  /\ UNCHANGED <<wpc, wsnap, wdot, horizon, cache, cver, htok>>

AEnd ==
  /\ apc = "mut"
  /\ apc' = "idle"
  /\ IF Conflicts(adot, asnap)
       THEN /\ ent' = [ent EXCEPT ![adot].st = "void"]      \* serialization failure
            /\ UNCHANGED <<cache, cver, htok>>
       ELSE LET C == asnap \cup {adot}                     \* commit token
                ent2 == [ent EXCEPT ![adot].st = "committed"]
            IN /\ ent' = ent2
               /\ htok' = htok \cup C
               /\ IF aobs
                    THEN \* observe since: evaluated in the transaction, at C
                         LET r == Answer(Since, C) IN
                         /\ cache' = Applied(r)
                         /\ cver' = C
                    ELSE UNCHANGED <<cache, cver>>
  /\ UNCHANGED <<ctr, wpc, wsnap, wdot, horizon, asnap, adot, aobs>>

(* NB: Answer(Since, C) in AEnd is evaluated on the pre-commit `ent`; that *)
(* is fine because QAt/SumW only read k and w, which commit does not set.  *)

(*************************** archiver **************************************)

\* archive (and drop from the hot log) the next resolved dot
Archive ==
  /\ horizon < MaxDot
  /\ Resolved(horizon + 1)
  /\ horizon' = horizon + 1
  /\ UNCHANGED <<ctr, ent, wpc, wsnap, wdot, apc, asnap, adot, aobs, cache, cver, htok>>

Next ==
  \/ \E x \in Writers : WEnd(x) \/ \E k \in Facts : WBegin(x, k)
  \/ Poll \/ AEnd \/ \E k \in Facts, obs \in BOOLEAN : ABegin(k, obs)
  \/ Archive

Spec == Init /\ [][Next]_vars

(*************************** properties ************************************)

\* chaining (§11.6): the cache always equals Q at its version
CacheExact == cache = One(QAt(cver))

\* the cache's version only contains committed dots
CverCommitted == cver \subseteq Committed
=============================================================================
