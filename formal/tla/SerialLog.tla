----------------------------- MODULE SerialLog -----------------------------
(***************************************************************************)
(* Shep §9.4 / §10.3 step 2: what SERIALIZABLE does to the write path when *)
(* every mutation reads its snapshot's version from the log inside its own *)
(* serializable transaction.                                               *)
(*                                                                         *)
(* Mutations touch DISJOINT data, so every conflict comes from the log.    *)
(* Step 2 reads "the visible committed entries above the frontier": a      *)
(* predicate read over the log's key range above the frontier. Step 6      *)
(* inserts into that range.                                                *)
(*                                                                         *)
(* The scheduler is an ideal serialization-graph certifier: a transaction  *)
(* may commit iff the dependency graph over the committed transactions     *)
(* plus itself stays acyclic. This admits every conflict-serializable      *)
(* history, so it is MORE permissive than Postgres SSI (which also aborts  *)
(* on "dangerous structures" that are not cycles). Whatever this model     *)
(* forbids, Postgres forbids too.                                          *)
(*                                                                         *)
(* CtxFromLogRead = TRUE  : the design as written (step 2 reads the log).  *)
(* CtxFromLogRead = FALSE : proposed fix: the context is derived from the  *)
(*   transaction's Postgres snapshot (pg_current_snapshot()) and the log   *)
(*   outside SSI tracking, so it creates no read dependency.               *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Txns, MaxDot, CtxFromLogRead

VARIABLES pc, snap, dot, ctr, corder, ncommit
vars == <<pc, snap, dot, ctr, corder, ncommit>>

Committed == {t \in Txns : pc[t] = "committed"}

Init ==
  /\ pc = [t \in Txns |-> "idle"]
  /\ snap = [t \in Txns |-> {}]
  /\ dot = [t \in Txns |-> 0]
  /\ ctr = 0
  /\ corder = [t \in Txns |-> 0]   \* position in real-time commit order
  /\ ncommit = 0

\* step 1-2: begin; the snapshot sees the transactions committed so far
Begin(t) ==
  /\ pc[t] \in {"idle", "aborted"}
  /\ snap' = [snap EXCEPT ![t] = Committed]
  /\ pc' = [pc EXCEPT ![t] = "active"]
  /\ UNCHANGED <<dot, ctr, corder, ncommit>>

\* steps 5-6: nextval and insert into the log
Insert(t) ==
  /\ pc[t] = "active"
  /\ ctr < MaxDot
  /\ ctr' = ctr + 1
  /\ dot' = [dot EXCEPT ![t] = ctr + 1]
  /\ pc' = [pc EXCEPT ![t] = "inserted"]
  /\ UNCHANGED <<snap, corder, ncommit>>

(* Dependency edges between two transactions a /= b of set S.              *)
(*  wr: a -> b if b's log read saw a's entry (a in snap[b]).               *)
(*  rw: a -> b if a's log read did NOT see b's entry, which lies in the    *)
(*      range a read (b had not committed when a took its snapshot, so b's *)
(*      dot was above a's frontier).                                       *)
Edge(a, b) ==
  /\ a /= b
  /\ CtxFromLogRead
  /\ (a \in snap[b] \/ b \notin snap[a])

RECURSIVE Reach(_, _, _)
\* nodes reachable from x in S, in at most k steps
Reach(x, S, k) ==
  IF k = 0 THEN {}
  ELSE LET nxt == {y \in S : Edge(x, y)}
       IN nxt \cup UNION {Reach(y, S, k - 1) : y \in nxt}

Acyclic(S) == \A x \in S : x \notin Reach(x, S, Cardinality(S))

TryCommit(t) ==
  /\ pc[t] = "inserted"
  /\ IF Acyclic(Committed \cup {t})
       THEN /\ pc' = [pc EXCEPT ![t] = "committed"]
            /\ ncommit' = ncommit + 1
            /\ corder' = [corder EXCEPT ![t] = ncommit + 1]
       ELSE /\ pc' = [pc EXCEPT ![t] = "aborted"]   \* serialization failure
            /\ UNCHANGED <<ncommit, corder>>
  /\ UNCHANGED <<snap, dot, ctr>>

Next == \E t \in Txns : Begin(t) \/ Insert(t) \/ TryCommit(t)

Spec == Init /\ [][Next]_vars

(* If this holds, no two overlapping mutations on one node ever both       *)
(* commit: one of every concurrent pair is aborted, even with disjoint     *)
(* data. Write throughput per node is that of a serial executor.           *)
NoOverlappingCommits ==
  \A a, b \in Committed : a /= b => (a \in snap[b] \/ b \in snap[a])

(* If this holds, committed dots always commit in dot order and every      *)
(* commit's context contains every lower committed dot: the out-of-order   *)
(* commit of §9.2 ("11 commits first, 10 commits later") never happens.    *)
CommitInDotOrder ==
  \A a, b \in Committed : dot[a] < dot[b] => a \in snap[b]

(* §9.2's scenario: a higher dot commits before a lower one, and both     *)
(* commit. If this holds, that scenario is impossible.                     *)
NoOutOfOrderCommit ==
  \A a, b \in Committed : dot[a] < dot[b] => corder[a] < corder[b]

Sym == Permutations(Txns)
=============================================================================
