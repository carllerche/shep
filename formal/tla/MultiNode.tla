----------------------------- MODULE MultiNode -----------------------------
(***************************************************************************)
(* Shep §9.5-§9.6: causal contexts across Postgres nodes.                  *)
(*                                                                         *)
(* Versions are modelled by what they denote: sets of dots <<node, n>>.    *)
(* (DotFrontier.tla checks that the DVV encoding denotes the right set.)   *)
(* A mutation runs on one node: its context is that node's snapshot        *)
(* version merged with the client's at_least token; its response token is *)
(* the context plus its own dot. Commits on one node may be out of order.  *)
(* A read that spans nodes takes one snapshot per node, one after the      *)
(* other, and merges their versions (§9.6).                                *)
(*                                                                         *)
(* FixMultiRead = FALSE: the design as written.                            *)
(* FixMultiRead = TRUE : proposed fix: a multi-node read returns (and      *)
(*   rolls its data back to) the largest causally closed subset of the     *)
(*   merged version, using the log the way §11.5 already does.             *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Nodes, Clients, MaxPerNode, FixMultiRead, First, Second

ASSUME First \in Nodes /\ Second \in Nodes /\ First /= Second

AllDots == Nodes \X (1..MaxPerNode)

VARIABLES
  ctr,     \* ctr[x]: node x's counter
  ent,     \* ent[d]: log entry for dot d: [st, ctx]
  ctime,   \* ctime[d]: position of d's commit in real-time order (0 = not committed)
  nc,      \* number of commits so far
  cpc,     \* client state
  cdot,    \* dot of the client's in-flight mutation
  tok,     \* client's merged token (Db handle)
  mrA,     \* first snapshot of an in-progress multi-node read
  mres     \* version returned by the client's last multi-node read

vars == <<ctr, ent, ctime, nc, cpc, cdot, tok, mrA, mres>>

NoDot == <<"none", 0>>

Init ==
  /\ ctr = [x \in Nodes |-> 0]
  /\ ent = [d \in AllDots |-> [st |-> "none", ctx |-> {}]]
  /\ ctime = [d \in AllDots |-> 0]
  /\ nc = 0
  /\ cpc = [c \in Clients |-> "idle"]
  /\ cdot = [c \in Clients |-> NoDot]
  /\ tok = [c \in Clients |-> {}]
  /\ mrA = [c \in Clients |-> {}]
  /\ mres = [c \in Clients |-> {}]

IsCommitted(d) == ent[d].st = "committed"
CommittedOn(x) == {d \in AllDots : d[1] = x /\ IsCommitted(d)}
CommittedDots == {d \in AllDots : IsCommitted(d)}
CtxOf(d) == ent[d].ctx

Closed(S) == \A d \in S : CtxOf(d) \subseteq S

\* the largest causally closed subset of S
RECURSIVE Core(_)
Core(S) == LET S2 == {d \in S : CtxOf(d) \subseteq S}
           IN IF S2 = S THEN S ELSE Core(S2)

(* steps 1-6: snapshot + at_least = context; nextval; insert (pending) *)
BeginMut(c, x) ==
  /\ cpc[c] = "idle"
  /\ ctr[x] < MaxPerNode
  /\ LET d == <<x, ctr[x] + 1>> IN
       /\ ctr' = [ctr EXCEPT ![x] = ctr[x] + 1]
       /\ ent' = [ent EXCEPT ![d] = [st |-> "pending", ctx |-> CommittedOn(x) \cup tok[c]]]
       /\ cdot' = [cdot EXCEPT ![c] = d]
  /\ cpc' = [cpc EXCEPT ![c] = "mut"]
  /\ UNCHANGED <<ctime, nc, tok, mrA, mres>>

CommitMut(c) ==
  LET d == cdot[c] IN
  /\ cpc[c] = "mut"
  /\ ent' = [ent EXCEPT ![d].st = "committed"]
  /\ nc' = nc + 1
  /\ ctime' = [ctime EXCEPT ![d] = nc + 1]
  /\ tok' = [tok EXCEPT ![c] = tok[c] \cup ent[d].ctx \cup {d}]
  /\ cpc' = [cpc EXCEPT ![c] = "idle"]
  /\ UNCHANGED <<ctr, cdot, mrA, mres>>

\* abort: the dot becomes void (DotFrontier.tla covers how)
AbortMut(c) ==
  LET d == cdot[c] IN
  /\ cpc[c] = "mut"
  /\ ent' = [ent EXCEPT ![d] = [st |-> "void", ctx |-> {}]]
  /\ cpc' = [cpc EXCEPT ![c] = "idle"]
  /\ UNCHANGED <<ctr, ctime, nc, cdot, tok, mrA, mres>>

\* a single-node read (on a primary every committed dot is visible)
ReadOne(c, x) ==
  /\ cpc[c] = "idle"
  /\ tok' = [tok EXCEPT ![c] = tok[c] \cup CommittedOn(x)]
  /\ UNCHANGED <<ctr, ent, ctime, nc, cpc, cdot, mrA, mres>>

\* a read spanning two nodes: two snapshots, taken one after the other
MultiReadFirst(c) ==
  /\ cpc[c] = "idle"
  /\ mrA' = [mrA EXCEPT ![c] = CommittedOn(First)]
  /\ cpc' = [cpc EXCEPT ![c] = "mread"]
  /\ UNCHANGED <<ctr, ent, ctime, nc, cdot, tok, mres>>

MultiReadSecond(c) ==
  /\ cpc[c] = "mread"
  /\ LET merged == mrA[c] \cup CommittedOn(Second)
         r == IF FixMultiRead THEN Core(merged) ELSE merged
     IN /\ mres' = [mres EXCEPT ![c] = r]
        /\ tok' = [tok EXCEPT ![c] = tok[c] \cup r]
  /\ cpc' = [cpc EXCEPT ![c] = "idle"]
  /\ UNCHANGED <<ctr, ent, ctime, nc, cdot, mrA>>

Next ==
  \E c \in Clients :
     \/ \E x \in Nodes : BeginMut(c, x) \/ ReadOne(c, x)
     \/ CommitMut(c) \/ AbortMut(c)
     \/ MultiReadFirst(c) \/ MultiReadSecond(c)

Spec == Init /\ [][Next]_vars

(*************************** properties ************************************)

\* every context (pending or committed entry) contains only committed dots
CtxCommitted ==
  \A d \in AllDots : ent[d].st \in {"pending", "committed"} => CtxOf(d) \subseteq CommittedDots

\* causal order is well defined: every dot in a context committed strictly
\* before the entry itself (so "context happened-before" is acyclic and
\* consistent with real time); in particular no dot is in its own context
CausalOrderWellFounded ==
  \A d \in CommittedDots : \A e \in CtxOf(d) : ctime[e] < ctime[d]

\* contexts are causally closed, so a causal application order exists that
\* never waits on a dot outside the log
ContextsClosed == \A d \in CommittedDots : Closed(CtxOf(d) \cup {d})

\* tokens only name committed dots
TokensCommitted == \A c \in Clients : tok[c] \subseteq CommittedDots

\* the version a multi-node read reports (and whose data it returns) is
\* causally closed: it never shows an effect without its cause
MultiReadCausal == \A c \in Clients : Closed(mres[c])

TokensClosed == \A c \in Clients : Closed(tok[c])

Safety == CtxCommitted /\ CausalOrderWellFounded /\ TokensCommitted

Sym == Permutations(Clients)
=============================================================================
