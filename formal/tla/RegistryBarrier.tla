--------------------------- MODULE RegistryBarrier ---------------------------
(***************************************************************************)
(* Shep design §10.3 (registry barriers, backfill at a cutover), §3        *)
(* (index entries and ghosts derived by the materializer; each index's    *)
(* creation dot), §11.7 (freshness checks and indexes newer than V).       *)
(*                                                                         *)
(* The meta partition M holds registry changes, each at a dot (M, j):      *)
(*   "ix"    adds an index on field f (its creation dot is (M, j));        *)
(*   "tight" tightens the write policy: writing f = "b" is denied.         *)
(* One data partition D holds nodes n1, n2 with field f in {a, b}. D's     *)
(* entry 1 creates them (n1 = a, n2 = b). D's log also holds barriers.     *)
(*                                                                         *)
(* A version of D is a prefix k of D's log plus a registry prefix r (the   *)
(* registry dots (M, 1..r) its context names). A barrier's context names   *)
(* its registry dot, so a version whose prefix covers a barrier names at   *)
(* least that registry dot.                                                *)
(*                                                                         *)
(* Front ends: one per write request (set n.f := v), each with a cached    *)
(* registry prefix that it refreshes at any time (so caches are of every   *)
(* staleness). It reads at S = (k, r = its registry), evaluates the write  *)
(* policy under its registry (denied, or a no-op if the value is the       *)
(* same), and proposes (delta, read set {n.f}, S). D's leader rejects with *)
(* stale_registry if S lacks the barrier's registry dot (the front end     *)
(* refreshes and re-executes), with contended if an entry after S wrote    *)
(* n.f, or appends.                                                        *)
(*                                                                         *)
(* The materializer applies D's log in order. It derives index entries and *)
(* ghosts: a move of n from v to v' writes a ghost at (v, n) and a live    *)
(* entry at (v', n), both stamped with the move's dot. Backfill writes a   *)
(* live entry for every node, stamped with its field's stamp.              *)
(*                                                                         *)
(* Freshness check: unchanged(Q(v), since V) at a snapshot S containing V, *)
(* Q(v) = the nodes with f = v. With the index in force at S, it walks the *)
(* index range v at S (live entries and ghosts) and tests every stamp      *)
(* against V, if it trusts the index for V; otherwise it answers changed.  *)
(* Without the index, it scans every node's field stamp.                   *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  ReqOf,        \* request -> [n |-> node, v |-> value]
  Changes,      \* registry changes that may happen: subset of {"ix", "tight"}
  Barriers,     \* TRUE (design): barriers; FALSE: a change takes effect when
                \*   it commits, and the materializer maintains the index for
                \*   an entry iff the entry's S names the index
  BarrierRule,  \* "max": the leader requires the highest registry dot among
                \*   D's barriers; "last": the last-appended barrier's dot
  Cutover,      \* "barrier" (design): backfill reads D's state at the
                \*   index's barrier; "none": backfill reads whatever state
                \*   the materializer has applied when it runs, and
                \*   maintenance starts at the barrier
  EvalAtS,      \* TRUE (design, §10.3 step 1): a front end evaluates under the
                \*   registry at S, refreshing when S's closure names a newer
                \*   registry dot than its cache; FALSE: under its cache
  CheckRule,    \* how a freshness check decides whether to trust the index
                \*   for V: "barrier" (V contains the index's barrier dot in
                \*   D), "creation" (V contains its creation dot, §11.7),
                \*   "none" (always trusts it: review C13)
  MaxLen        \* bound on D's log length

Reqs == DOMAIN ReqOf
Nodes == {"n1", "n2"}
Vals == {"a", "b"}
Init0 == [n1 |-> "a", n2 |-> "b"]

\* request maps for the configs
RQ1 == ("w1" :> [n |-> "n1", v |-> "b"])
RQ2 == ("w1" :> [n |-> "n1", v |-> "b"]) @@ ("w2" :> [n |-> "n2", v |-> "a"])

VARIABLES
  mlog,     \* the meta partition's registry changes, in order
  dlog,     \* D's log
  cut,      \* change -> the position in D's log from which it is in force
            \*   (0: not yet)
  fe,       \* request -> front end
  mat       \* the materializer: applied, index store, backfill position

vars == <<mlog, dlog, cut, fe, mat>>

RegIdx(c) == IF \E j \in 1..Len(mlog) : mlog[j] = c
             THEN CHOOSE j \in 1..Len(mlog) : mlog[j] = c ELSE 0
Committed(c) == RegIdx(c) > 0

(* D's entries. kind: "init", "mut" (n.f := v, read at S), "bar" (names   *)
(* the registry change c).                                                 *)
DE(kind, n, v, S, c, u) == [kind |-> kind, n |-> n, v |-> v, S |-> S, c |-> c, u |-> u]
NoS == [k |-> 0, r |-> 0]

\* the value and stamp (a position in D) of n.f at prefix k
RECURSIVE FAt(_, _, _)
FAt(L, n, k) ==
  IF k = 1 THEN [v |-> Init0[n], st |-> 1]
  ELSE IF L[k].kind = "mut" /\ L[k].n = n THEN [v |-> L[k].v, st |-> k]
  ELSE FAt(L, n, k - 1)
Val(n, k) == FAt(dlog, n, k).v
Stamp(n, k) == FAt(dlog, n, k).st
Q(v, k) == {n \in Nodes : Val(n, k) = v}

\* the registry dots a version of D at prefix k must name (barrier contexts)
RegMin(k) == LET B == {i \in 1..k : dlog[i].kind = "bar"} IN
             IF B = {} THEN 0
             ELSE CHOOSE m \in {RegIdx(dlog[i].c) : i \in B} :
                    \A i \in B : RegIdx(dlog[i].c) <= m

\* the registry index D's leader requires
Barrs == {i \in 1..Len(dlog) : dlog[i].kind = "bar"}
Required ==
  IF Barrs = {} THEN 0
  ELSE IF BarrierRule = "max" THEN RegMin(Len(dlog))
  ELSE LET last == CHOOSE i \in Barrs : \A j \in Barrs : j <= i IN RegIdx(dlog[last].c)

Denied(r, v) == Committed("tight") /\ RegIdx("tight") <= r /\ v = "b"

(*************************** index derivation ******************************)

IxE(v, n, st, live) == [v |-> v, n |-> n, st |-> st, live |-> live]
BackfillAt(L, k) == {IxE(FAt(L, n, k).v, n, FAt(L, n, k).st, TRUE) : n \in Nodes}
MoveAt(L, p) ==
  LET n == L[p].n IN
  {IxE(FAt(L, n, p - 1).v, n, p, FALSE), IxE(L[p].v, n, p, TRUE)}

IxBar(L, k) == \E i \in 1..k : L[i].kind = "bar" /\ L[i].c = "ix"
\* is the index maintained for the entry at p? (barriers: after the index's
\* barrier; no barriers: iff the entry's S names the index's creation dot)
Maintained(L, p) ==
  IF Barriers THEN IxBar(L, p - 1)
  ELSE Committed("ix") /\ L[p].u >= RegIdx("ix")

(* Replay: derive the index from D's log alone, applying positions 1..k.   *)
(* With barriers, backfill happens at the index's barrier. Without them    *)
(* the log does not say where it happened; replay backfills at the first   *)
(* entry whose S names the index.                                          *)
ReplayBackfillPos(L, k) ==
  IF Barriers
  THEN IF IxBar(L, k) THEN CHOOSE i \in 1..k : L[i].kind = "bar" /\ L[i].c = "ix" ELSE 0
  ELSE IF \E i \in 1..k : L[i].kind = "mut" /\ Maintained(L, i)
       THEN CHOOSE i \in 1..k : L[i].kind = "mut" /\ Maintained(L, i)
                                /\ \A j \in 1..(i - 1) : ~ (L[j].kind = "mut" /\ Maintained(L, j))
       ELSE 0
Replay(L, k) ==
  LET b == ReplayBackfillPos(L, k) IN
  (IF b = 0 THEN {} ELSE BackfillAt(L, b))
  \cup UNION {MoveAt(L, p) : p \in {p \in 1..k : L[p].kind = "mut" /\ Maintained(L, p)}}

(*************************** actions ***************************************)

FE0 == [pc |-> "idle", reg |-> 0, S |-> NoS, u |-> 0]

Init ==
  /\ mlog = << >>
  /\ dlog = << DE("init", "-", "-", NoS, "-", 0) >>
  /\ cut = [c \in {"ix", "tight"} |-> 0]
  /\ fe = [q \in Reqs |-> FE0]
  /\ mat = [applied |-> 1, ix |-> {}, bpos |-> 0]

\* a registry change commits in M
MChange(c) ==
  /\ c \in Changes /\ ~ Committed(c)
  /\ mlog' = Append(mlog, c)
  /\ cut' = IF Barriers THEN cut ELSE [cut EXCEPT ![c] = Len(dlog) + 1]
  /\ UNCHANGED <<dlog, fe, mat>>

\* the meta leader appends a barrier naming the change to D (any order)
AppendBarrier(c) ==
  /\ Barriers /\ Committed(c) /\ cut[c] = 0
  /\ Len(dlog) < MaxLen
  /\ dlog' = Append(dlog, DE("bar", "-", "-", NoS, c, 0))
  /\ cut' = [cut EXCEPT ![c] = Len(dlog) + 1]
  /\ UNCHANGED <<mlog, fe, mat>>

\* a front end refreshes its cached registry at any time
FERefresh(q) ==
  /\ fe[q].pc = "idle" /\ fe[q].reg # Len(mlog)
  /\ fe' = [fe EXCEPT ![q].reg = Len(mlog)]
  /\ UNCHANGED <<mlog, dlog, cut, mat>>

\* read at prefix k; S names at least the registry dots of the barriers it
\* covers. The front end evaluates the policy under u: the registry at S
\* (EvalAtS) or its cache.
FERead(q) ==
  /\ fe[q].pc = "idle"
  /\ \E k \in 1..Len(dlog) :
       LET r == IF RegMin(k) > fe[q].reg THEN RegMin(k) ELSE fe[q].reg
           u == IF EvalAtS THEN r ELSE fe[q].reg
           n == ReqOf[q].n
           v == ReqOf[q].v
       IN fe' = [fe EXCEPT ![q] =
                  [pc |-> CASE Denied(u, v)  -> "denied"
                            [] Val(n, k) = v -> "done"     \* moves nothing
                            [] OTHER         -> "prop",
                   reg |-> u, S |-> [k |-> k, r |-> r], u |-> u]]
  /\ UNCHANGED <<mlog, dlog, cut, mat>>

FEPropose(q) ==
  /\ fe[q].pc = "prop"
  /\ LET S == fe[q].S
         n == ReqOf[q].n
     IN CASE Barriers /\ Required > S.r ->                  \* stale_registry
               /\ fe' = [fe EXCEPT ![q] = [FE0 EXCEPT !.reg = Len(mlog)]]
               /\ UNCHANGED dlog
          [] \E p \in (S.k + 1)..Len(dlog) : dlog[p].kind = "mut" /\ dlog[p].n = n ->
               /\ fe' = [fe EXCEPT ![q] = [FE0 EXCEPT !.reg = fe[q].reg]]   \* contended
               /\ UNCHANGED dlog
          [] OTHER ->
               /\ Len(dlog) < MaxLen
               /\ dlog' = Append(dlog, DE("mut", n, ReqOf[q].v, S, "-", fe[q].u))
               /\ fe' = [fe EXCEPT ![q].pc = "done"]
  /\ UNCHANGED <<mlog, cut, mat>>

\* the materializer applies D's next entry, deriving index entries
Apply ==
  /\ mat.applied < Len(dlog)
  /\ LET p == mat.applied + 1
         e == dlog[p]
         bf == Cutover = "barrier" /\ e.kind = "bar" /\ e.c = "ix"
     IN mat' = [applied |-> p,
                ix |-> mat.ix \cup (IF bf THEN BackfillAt(dlog, p) ELSE {})
                              \cup (IF e.kind = "mut" /\ Maintained(dlog, p)
                                    THEN MoveAt(dlog, p) ELSE {}),
                bpos |-> IF bf THEN p ELSE mat.bpos]
  /\ UNCHANGED <<mlog, dlog, cut, fe>>

\* backfill without a cutover (and without barriers): once the index is
\* committed, read whatever state is applied now
Backfill ==
  /\ Cutover = "none" \/ ~ Barriers
  /\ Committed("ix") /\ mat.bpos = 0
  /\ mat' = [mat EXCEPT !.ix = @ \cup BackfillAt(dlog, mat.applied), !.bpos = mat.applied]
  /\ UNCHANGED <<mlog, dlog, cut, fe>>

Next ==
  \/ \E c \in Changes : MChange(c) \/ AppendBarrier(c)
  \/ \E q \in Reqs : FERefresh(q) \/ FERead(q) \/ FEPropose(q)
  \/ Apply \/ Backfill

Spec == Init /\ [][Next]_vars

(*************************** properties ************************************)

TypeOK ==
  /\ \A q \in Reqs : fe[q].pc \in {"idle", "prop", "done", "denied"}
  /\ mat.applied <= Len(dlog)

Muts == {p \in 1..Len(dlog) : dlog[p].kind = "mut"}

(* Every entry committed after a change is in force (after its barrier)    *)
(* was computed under a registry containing the change's dot: S contains   *)
(* it, and so does the registry the front end evaluated under.             *)
BarrierCtx ==
  \A p \in Muts, c \in {"ix", "tight"} :
    cut[c] > 0 /\ cut[c] < p => dlog[p].S.r >= RegIdx(c) /\ dlog[p].u >= RegIdx(c)

\* No write the tightened policy denies commits after it is in force.
PolicyEnforced ==
  \A p \in Muts : cut["tight"] > 0 /\ cut["tight"] < p => dlog[p].v # "b"

\* the index's versions visible at prefix k: per key, the newest stamp in k
VisAt(I, v, n, k) ==
  LET C == {x \in I : x.v = v /\ x.n = n /\ x.st <= k} IN
  IF C = {} THEN [none |-> TRUE]
  ELSE LET m == CHOOSE x \in C : \A y \in C : y.st <= x.st
                                  /\ (y.st = x.st => (~ x.live \/ y.live))
       IN [none |-> FALSE, st |-> m.st, live |-> m.live]

\* where the index starts: its barrier, and its backfill
IxStart == IF cut["ix"] >= mat.bpos THEN cut["ix"] ELSE mat.bpos
IxReady == Committed("ix") /\ cut["ix"] > 0 /\ mat.bpos > 0 /\ mat.applied >= IxStart

(* Backfill plus maintenance equals the full index at every version k from *)
(* where the index starts up to what the materializer applied: every       *)
(* node's live entry is at its value with its field's stamp, no other     *)
(* entry of the node is live, and every move out of a value after the      *)
(* start left a version (a ghost, or a later live entry) at the old value. *)
IndexExact ==
  IxReady =>
    \A k \in IxStart..mat.applied :
      /\ \A n \in Nodes, v \in Vals :
           LET x == VisAt(mat.ix, v, n, k) IN
           IF v = Val(n, k)
           THEN ~ x.none /\ x.live /\ x.st = Stamp(n, k)
           ELSE x.none \/ ~ x.live
      /\ \A p \in Muts : p > IxStart /\ p <= k =>
           LET old == Val(dlog[p].n, p - 1)
               x == VisAt(mat.ix, old, dlog[p].n, k)
           IN old # dlog[p].v => ~ x.none /\ x.st >= p

\* Replaying D's log reproduces the materializer's derived index.
ReplayEqual == mat.ix = Replay(dlog, mat.applied)

(* Freshness soundness. For every version V = (k, r) a client can hold and *)
(* every snapshot s >= k the materializer serves: if the check says        *)
(* unchanged, Q(v) at V equals Q(v) at s.                                  *)
UseIx(s) == IxReady /\ s >= IxStart
Trust(k, r) ==
  CASE CheckRule = "none"     -> TRUE
    [] CheckRule = "creation" -> r >= RegIdx("ix")
    [] CheckRule = "barrier"  -> k >= IxStart
Unchanged(k, r, s, v) ==
  IF UseIx(s)
  THEN Trust(k, r) /\ \A n \in Nodes : LET x == VisAt(mat.ix, v, n, s) IN x.none \/ x.st <= k
  ELSE \A n \in Nodes : Stamp(n, s) <= k
FreshnessSound ==
  \A k \in 1..mat.applied, s \in 1..mat.applied, v \in Vals :
    k <= s =>
      \A r \in RegMin(k)..Len(mlog) : Unchanged(k, r, s, v) => Q(v, k) = Q(v, s)

Bound == Len(dlog) <= MaxLen
=============================================================================
