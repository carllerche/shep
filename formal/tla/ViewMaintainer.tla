--------------------------- MODULE ViewMaintainer ---------------------------
(***************************************************************************)
(* Shep §12: a materialized query maintained from several nodes' logs.     *)
(*                                                                         *)
(* Each PG node x owns facts <<x,1>>, <<x,2>>; a mutation on x toggles    *)
(* one of them, with a +1/-1 Z-set delta computed from its snapshot.       *)
(* Concurrent toggles of the same fact conflict (first committer wins), as *)
(* in Postgres; toggles of different facts commit concurrently, in any     *)
(* order (this assumes the fix for finding F1). Contexts are snapshot      *)
(* version + the client's at_least token (§9.5).                           *)
(*                                                                         *)
(* The view is Q = count of present facts on the nodes in ReadNodes. Its   *)
(* result is a one-row Z-set {count |-> 1}, so Q is NOT linear.            *)
(*                                                                         *)
(* Knobs (the design is ambiguous on each):                                *)
(*  Tailing = "cursor": the maintainer keeps a max-n cursor per node and   *)
(*     reports "everything <= cursor" as its version (a naive tail).       *)
(*  Tailing = "window": it keeps its progress as a DVV and rescans above   *)
(*     the node's frontier (the out-of-order window).                      *)
(*  TailAllNodes = FALSE: it reads only the logs of ReadNodes (§12 text).  *)
(*  TailAllNodes = TRUE : proposed fix: it also follows (dot, context) of  *)
(*     every other node and applies those dots as no-ops.                  *)
(*  ChangesMode = "sumSteps": changes(view, since V) sums the view's       *)
(*     recorded output deltas for the dots not in V.                       *)
(*  ChangesMode = "chainOnly": proposed fix: answer with a delta only when *)
(*     V is one of the view's own past versions, else resync.              *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS Nodes, ReadNodes, Clients, MaxPerNode, MaxTotal, Tailing, TailAllNodes, ChangesMode

AllDots == Nodes \X (1..MaxPerNode)
Counts == 0..(2 * Cardinality(ReadNodes))

VARIABLES
  ctr, ent, cpc, cdot, tok,      \* writers, as in MultiNode.tla
  app,      \* dots the maintainer has applied
  cur,      \* per-node cursor (Tailing = "cursor")
  vcount,   \* the view's maintained count
  stepOut,  \* stepOut[d] = <<old count, new count>> when d was applied
  chain,    \* the view's past versions
  chOK,     \* history: every changes(view, since) answer was exact
  rq        \* rq[c]: the version an outstanding at_least read waits for

vars == <<ctr, ent, cpc, cdot, tok, app, cur, vcount, stepOut, chain, chOK, rq>>

NoDot == <<"none", 0>>
None == {<<"none", 0>>}   \* "no outstanding read"

Init ==
  /\ ctr = [x \in Nodes |-> 0]
  /\ ent = [d \in AllDots |-> [st |-> "none", ctx |-> {}, w |-> 0, k |-> 0]]
  /\ cpc = [c \in Clients |-> "idle"]
  /\ cdot = [c \in Clients |-> NoDot]
  /\ tok = [c \in Clients |-> {}]
  /\ app = {}
  /\ cur = [x \in Nodes |-> 0]
  /\ vcount = 0
  /\ stepOut = [d \in AllDots |-> <<0, 0>>]
  /\ chain = {{}}
  /\ chOK = TRUE
  /\ rq = [c \in Clients |-> None]

IsCommitted(d) == ent[d].st = "committed"
CommittedOn(x) == {d \in AllDots : d[1] = x /\ IsCommitted(d)}
CommittedDots == {d \in AllDots : IsCommitted(d)}

RECURSIVE SumW(_)
SumW(S) == IF S = {} THEN 0
           ELSE LET d == CHOOSE d \in S : TRUE IN ent[d].w + SumW(S \ {d})

\* weight of fact <<x,k>> in the state at version S (Z-set sum of deltas)
Weight(x, k, S) == SumW({d \in S : d[1] = x /\ ent[d].k = k})
\* Q at version S
QAt(S) == SumW({d \in S : d[1] \in ReadNodes})

(*************************** writers ***************************************)

RECURSIVE SumCtr(_)
SumCtr(S) == IF S = {} THEN 0 ELSE LET x == CHOOSE x \in S : TRUE IN ctr[x] + SumCtr(S \ {x})

BeginMut(c, x, k) ==
  /\ cpc[c] = "idle"
  /\ ctr[x] < MaxPerNode
  /\ SumCtr(Nodes) < MaxTotal
  /\ LET d == <<x, ctr[x] + 1>>
         snap == CommittedOn(x)
         w == IF Weight(x, k, snap) > 0 THEN -1 ELSE 1
     IN /\ ctr' = [ctr EXCEPT ![x] = ctr[x] + 1]
        /\ ent' = [ent EXCEPT ![d] = [st |-> "pending", ctx |-> snap \cup tok[c], w |-> w, k |-> k]]
        /\ cdot' = [cdot EXCEPT ![c] = d]
  /\ cpc' = [cpc EXCEPT ![c] = "mut"]
  /\ UNCHANGED <<tok, app, cur, vcount, stepOut, chain, chOK, rq>>

\* first committer wins on the fact's row: abort if a dot that toggled the
\* same fact committed since our snapshot
CommitMut(c) ==
  LET d == cdot[c] IN
  /\ cpc[c] = "mut"
  /\ IF {e \in CommittedOn(d[1]) : ent[e].k = ent[d].k} \subseteq ent[d].ctx
       THEN /\ ent' = [ent EXCEPT ![d].st = "committed"]
            /\ tok' = [tok EXCEPT ![c] = tok[c] \cup ent[d].ctx \cup {d}]
       ELSE /\ ent' = [ent EXCEPT ![d] = [st |-> "void", ctx |-> {}, w |-> 0, k |-> 0]]
            /\ tok' = tok
  /\ cpc' = [cpc EXCEPT ![c] = "idle"]
  /\ UNCHANGED <<ctr, cdot, app, cur, vcount, stepOut, chain, chOK, rq>>

ReadOne(c, x) ==
  /\ cpc[c] = "idle"
  /\ tok' = [tok EXCEPT ![c] = tok[c] \cup CommittedOn(x)]
  /\ UNCHANGED <<ctr, ent, cpc, cdot, app, cur, vcount, stepOut, chain, chOK, rq>>

(*************************** maintainer ************************************)

Tailed == IF TailAllNodes THEN Nodes ELSE ReadNodes

\* the version the view reports
ViewVersion ==
  IF Tailing = "window" THEN app
  ELSE {d \in CommittedDots : d[2] <= cur[d[1]]}

\* entries the maintainer can see
Visible(d) ==
  /\ IsCommitted(d)
  /\ d[1] \in Tailed
  /\ d \notin app
  /\ (Tailing = "cursor" => d[2] > cur[d[1]])

\* §12: apply an entry once every dot in its context has been applied
Apply(d) ==
  /\ Visible(d)
  /\ ent[d].ctx \subseteq app
  /\ LET new == IF d[1] \in ReadNodes THEN vcount + ent[d].w ELSE vcount IN
       /\ vcount' = new
       /\ stepOut' = [stepOut EXCEPT ![d] = <<vcount, new>>]
  /\ app' = app \cup {d}
  /\ cur' = [cur EXCEPT ![d[1]] = IF d[2] > cur[d[1]] THEN d[2] ELSE cur[d[1]]]
  /\ chain' = chain \cup {app \cup {d}}
  /\ UNCHANGED <<ctr, ent, cpc, cdot, tok, chOK, rq>>

(*************************** view reads ************************************)

\* Z-set {k |-> 1} for a count, and Z-set arithmetic
One(k) == [i \in Counts |-> IF i = k THEN 1 ELSE 0]
ZAdd(a, b) == [i \in Counts |-> a[i] + b[i]]
ZSub(a, b) == [i \in Counts |-> a[i] - b[i]]
Zero == [i \in Counts |-> 0]
RECURSIVE ZSum(_)
ZSum(S) == IF S = {} THEN Zero
           ELSE LET d == CHOOSE d \in S : TRUE
                IN ZAdd(ZSub(One(stepOut[d][2]), One(stepOut[d][1])), ZSum(S \ {d}))

\* an at_least read: issue, then wait until the view covers the token
ReadViewIssue(c) ==
  /\ rq[c] = None
  /\ rq' = [rq EXCEPT ![c] = tok[c]]
  /\ UNCHANGED <<ctr, ent, cpc, cdot, tok, app, cur, vcount, stepOut, chain, chOK>>

ReadViewDone(c) ==
  /\ rq[c] /= None
  /\ rq[c] \subseteq ViewVersion
  /\ rq' = [rq EXCEPT ![c] = None]
  /\ tok' = [tok EXCEPT ![c] = tok[c] \cup ViewVersion]
  /\ UNCHANGED <<ctr, ent, cpc, cdot, app, cur, vcount, stepOut, chain, chOK>>

\* changes(view, since: tok[c]), answered from the view's recorded output deltas
ViewChanges(c) ==
  /\ tok[c] \subseteq app
  /\ LET V == tok[c]
         exact == ZSub(One(QAt(app)), One(QAt(V)))
         ans == IF ChangesMode = "sumSteps" \/ V \in chain
                  THEN ZSum(app \ V)
                  ELSE exact            \* resync: full result, always exact
     IN chOK' = (chOK /\ ans = exact)
  /\ UNCHANGED <<ctr, ent, cpc, cdot, tok, app, cur, vcount, stepOut, chain, rq>>

Next ==
  \/ \E c \in Clients :
        \/ \E x \in Nodes : (\E k \in {1, 2} : BeginMut(c, x, k)) \/ ReadOne(c, x)
        \/ CommitMut(c)
        \/ ReadViewIssue(c) \/ ReadViewDone(c) \/ ViewChanges(c)
  \/ \E d \in AllDots : Apply(d)

Spec == Init /\ [][Next]_vars /\ WF_vars(\E d \in AllDots : Apply(d))
        /\ \A c \in Clients : WF_vars(CommitMut(c)) /\ WF_vars(ReadViewDone(c))

(*************************** properties ************************************)

Closed(S) == \A d \in S : ent[d].ctx \subseteq S

\* the applied set is causally closed
AppliedClosed == Closed(app)

\* the view's state equals Q evaluated at exactly the version it reports
ViewExact == vcount = QAt(ViewVersion)

\* changes(view, since) answers were exactly Q(now) - Q(since)
ChangesExact == chOK

Inv == AppliedClosed /\ ViewExact

\* every committed entry on a node the view reads is eventually applied
EventuallyApplied ==
  \A d \in AllDots : (IsCommitted(d) /\ d[1] \in ReadNodes) ~> (d \in app)

\* every at_least read eventually returns
ReadsReturn == \A c \in Clients : (rq[c] /= None) ~> (rq[c] = None)

Sym == Permutations(Clients)
=============================================================================
