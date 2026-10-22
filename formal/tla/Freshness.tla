----------------------------- MODULE Freshness -----------------------------
(***************************************************************************)
(* Shep design §3 (dots and ghosts) and §11.7 (freshness checks), on ONE   *)
(* Postgres node.                                                          *)
(*                                                                         *)
(* A small graph:                                                          *)
(*  - nodes 1..NN of one type, with an indexed field f in {a, b} and a     *)
(*    lifecycle (soft delete / restore). Index entries are partial on      *)
(*    "not deleted" (§3). Index order is node order.                       *)
(*  - one edge type, members: (node, actor), used by the read policy.      *)
(*  - two policy heads, each a logged row with a dot:                      *)
(*      pol: the node type's own read policy: "viaDef" (allow if def says  *)
(*           so) or "open" (allow everyone)                                *)
(*      def: an imported definition / delegate target's policy (§6.1):     *)
(*           "member" (allow if viewer in node.members) or "anyone"        *)
(*  - the declared query Q(u) = the first MaxFirst nodes, in index order,  *)
(*    with f = a that are visible to actor u (§6.5: not deleted and the    *)
(*    policy allows).                                                      *)
(*                                                                         *)
(* Storage is multi-version: the committed history clog is the list of     *)
(* committed mutations in commit order, each with the fields it wrote.     *)
(* The value of a field at a version V (a set of commits) is the one       *)
(* written by its last writer in V, and its dot is that writer's dot. So   *)
(* "the state at V" is exact, for snapshot versions, write tokens, and     *)
(* per-partition vectors alike. Commit 0 is the initial state, which every *)
(* version contains.                                                       *)
(*                                                                         *)
(* Dots are (partition, log index), contiguous per partition (no holes).   *)
(* With MultiPart, node n's facts live on partition n and the policy heads *)
(* on partition 1; a mutation writes only its home partition. A version is *)
(* a vector of per-partition indexes.                                      *)
(*                                                                         *)
(* Concurrency control, CC:                                                *)
(*  "occ": a mutation reads at snapshot S; its home partition's sequencer  *)
(*    appends it iff no entry committed in (S, commit] wrote into its read *)
(*    set (fact-level items, index-range keys, ghost-range keys). Serial   *)
(*    order is commit order.                                               *)
(*  "ssi": one Postgres node at SERIALIZABLE, as an ideal certifier over   *)
(*    physical items (rows, index keys, ghost-table keys, edge rows,       *)
(*    policy-head rows): snapshot reads, first-committer-wins, and a       *)
(*    commit is allowed iff the wr/ww/rw dependency graph stays acyclic.   *)
(*    That is MORE permissive than Postgres SSI.                           *)
(*                                                                         *)
(* Mutations: set f, soft delete, restore, link and unlink membership,     *)
(* deploy either policy head, and a guarded mutation that does             *)
(* `require unchanged(Q, u, V)` and then optionally moves a node out of    *)
(* Q's range (a "claim"). A mutation that fails validation retries at a    *)
(* new snapshot (and gets a new dot when it commits).                      *)
(*                                                                         *)
(* The constants select the rule the check implements; see the cfgs.      *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  Txns,           \* client requests (each is one mutation, retried on abort)
  NN,             \* number of nodes
  Actors,         \* actors
  MaxFirst,       \* Q's declared max_first (its LIMIT)
  IndexGhosts,    \* TRUE: an entry that leaves the index leaves a ghost row
  GhostEdges,     \* TRUE: unlink keeps the edge row as a ghost
  EdgeSetDot,     \* TRUE: link/unlink also move the edge-set dot on the source row
  Visit,          \* "all": every candidate the walk reaches;
                  \* "returned": only the rows Q returns at S
  PolicyInputs,   \* "edge": the membership edge row consulted;
                  \* "edgeset": the source's edge-set dot; "skip": neither
  PolicyDots,     \* "all": own policy head and the definitions it uses;
                  \* "own": only the queried type's own head; "none"
  EntryDots,      \* "field+lifecycle": a live index entry's dots are the
                  \* indexed field's and the lifecycle's (the partial-index
                  \* predicate); "field": only the indexed field's
  Walk,           \* "all": the whole range; "toStop": until Q at S has
                  \* MaxFirst rows; "maxFirst": the first MaxFirst candidates
  PolicyTracked,  \* TRUE: the check reads the policy heads inside the txn
  StaleSnapshot,  \* TRUE: also run checks at snapshots S that miss dots of V
  GuardWrites,    \* TRUE: a guarded mutation may also move a node out of range
  CC,             \* concurrency control: "ssi" (Postgres SERIALIZABLE, ideal
                  \*   certifier) or "occ" (the partition's sequencer validates
                  \*   that no entry committed in (S, commit] wrote into the
                  \*   mutation's read set)
  MultiPart,      \* FALSE: one partition; TRUE: node n lives on partition n
                  \*   (the policy heads live on partition 1)
  ValidateRemote, \* occ only: TRUE: validation also covers read-set items on
                  \*   other partitions; FALSE: only the home partition's log
  ReadSetRanges   \* TRUE: the check's read set includes the index range and
                  \*   its ghost range, not just the rows it found

ASSUME EdgeSetDot \/ PolicyInputs # "edgeset"
ASSUME CC \in {"ssi", "occ"} /\ (CC = "ssi" => ~ MultiPart)

Nodes == 1..NN
Vals == {"a", "b"}
QVal == "a"

Fields ==
  {<<"f", n>> : n \in Nodes} \cup {<<"del", n>> : n \in Nodes}
  \cup {<<"mem", n, u>> : n \in Nodes, u \in Actors}
  \cup {<<"es", n>> : n \in Nodes} \cup {<<"pol">>, <<"def">>}

InitStates ==
  { [x \in Fields |->
       CASE x[1] = "f"   -> fv[x[2]]
         [] x[1] = "del" -> FALSE
         [] x[1] = "mem" -> mv[x[2]]
         [] x[1] = "es"  -> 0
         [] x[1] = "pol" -> "viaDef"
         [] x[1] = "def" -> "member"]
    : fv \in [Nodes -> Vals], mv \in [Nodes -> {"none", "live"}] }
\* (membership is initialised per node for every actor alike)

Ops ==
  [k : {"setf"}, n : Nodes, v : Vals]
  \cup [k : {"del", "restore"}, n : Nodes]
  \cup [k : {"link", "unlink"}, n : Nodes, u : Actors]
  \cup [k : {"pol"}, p : {"viaDef", "open"}]
  \cup [k : {"def"}, p : {"member", "anyone"}]

GuardTargets == IF GuardWrites THEN {0} \cup Nodes ELSE {0}

(* Partitions (§ storage on per-partition logs). A dot is (partition,     *)
(* log index); indexes are contiguous per partition. Every fact belongs to *)
(* exactly one partition, and a mutation writes only its home partition.   *)
Parts == IF MultiPart THEN Nodes ELSE {1}
Part(n) == IF MultiPart THEN n ELSE 1
PolPart == 1
ItemPart(x) == CASE x[1] \in {"row", "erow", "f", "del", "es"} -> Part(x[2])
                 [] x[1] \in {"ix", "gx"}    -> Part(x[3])
                 [] OTHER                     -> PolPart
Home(op) == CASE op.k \in {"setf", "del", "restore", "link", "unlink"} -> Part(op.n)
              [] op.k \in {"pol", "def"} -> PolPart
              [] op.k = "guard" -> IF op.tgt = 0 THEN op.home ELSE Part(op.tgt)
GuardOps(k) == {g \in [k : {"guard"}, u : Actors, ver : 0..k, tgt : GuardTargets, home : Parts] :
                  g.tgt = 0 \/ g.home = Part(g.tgt)}

VARIABLES
  init,   \* the initial state (dot 0)
  clog,   \* committed mutations in commit order; the i-th has dot i
  pc,     \* per request: "idle", "act", "ok", "failed"
  req,    \* the request (fixed across retries)
  att     \* the current attempt

vars == <<init, clog, pc, req, att>>

Len0 == Len(clog)
Prefix(k) == 0..k
Cur == Prefix(Len0)

(*************************** reading a version ****************************)

RECURSIVE LastW(_, _, _)
LastW(V, x, i) ==
  IF i = 0 THEN 0
  ELSE IF i \in V /\ x \in DOMAIN clog[i].w THEN i ELSE LastW(V, x, i - 1)

Val(V, x)   == LET i == LastW(V, x, Len0) IN IF i = 0 THEN init[x] ELSE clog[i].w[x]
Stamp(V, x) == LastW(V, x, Len0)     \* the dot that last changed x in V

GhostDots(V, v, n) == {i \in V \ {0} : <<v, n>> \in clog[i].gh}

Live(V, v, n) == Val(V, <<"f", n>>) = v /\ ~ Val(V, <<"del", n>>)

UsesMembers(V) == Val(V, <<"pol">>) = "viaDef" /\ Val(V, <<"def">>) = "member"
Allowed(V, n, u) ==
  \/ Val(V, <<"pol">>) = "open"
  \/ Val(V, <<"def">>) = "anyone"
  \/ Val(V, <<"mem", n, u>>) = "live"
Visible(V, n, u) == Live(V, QVal, n) /\ Allowed(V, n, u)

Q(V, u) == {n \in Nodes : Visible(V, n, u)
                          /\ Cardinality({m \in 1..(n - 1) : Visible(V, m, u)}) < MaxFirst}

(*************************** the freshness check (§11.7) ******************)

FirstPos(ok(_)) == IF \E p \in Nodes : ok(p)
                   THEN CHOOSE p \in Nodes : ok(p) /\ \A q \in 1..(p - 1) : ~ ok(q)
                   ELSE NN

\* where Q's own plan at S stops: once it has MaxFirst visible rows
PlanStop(S, u) ==
  LET ok(p) == Cardinality({m \in 1..p : Visible(S, m, u)}) >= MaxFirst IN FirstPos(ok)

HasCand(S, n) == Live(S, QVal, n) \/ GhostDots(S, QVal, n) # {}

\* how far the check's unfiltered walk goes
StopPos(S, u) ==
  CASE Walk = "all"    -> NN
    [] Walk = "toStop" -> PlanStop(S, u)
    [] Walk = "maxFirst" ->
         LET ok(p) == Cardinality({m \in 1..p : HasCand(S, m)}) >= MaxFirst IN FirstPos(ok)

EntryDotsOf(S, n) ==
  {Stamp(S, <<"f", n>>)} \cup (IF EntryDots = "field" THEN {} ELSE {Stamp(S, <<"del", n>>)})

PolicyInputDots(S, n, u) ==
  IF PolicyInputs = "skip" \/ ~ UsesMembers(S) THEN {}
  ELSE IF PolicyInputs = "edgeset" THEN {Stamp(S, <<"es", n>>)}
  ELSE IF Val(S, <<"mem", n, u>>) = "none" THEN {}      \* no row: nothing to see
  ELSE {Stamp(S, <<"mem", n, u>>)}

PolicyHeadDots(S) ==
  CASE PolicyDots = "none" -> {}
    [] PolicyDots = "own"  -> {Stamp(S, <<"pol">>)}
    [] PolicyDots = "all"  -> {Stamp(S, <<"pol">>)}
                              \cup (IF Val(S, <<"pol">>) = "viaDef"
                                    THEN {Stamp(S, <<"def">>)} ELSE {})

RowDots(S, n, u) == EntryDotsOf(S, n) \cup PolicyInputDots(S, n, u)

VisitedDots(S, u) ==
  IF Visit = "returned"
  THEN UNION {RowDots(S, n, u) : n \in Q(S, u)} \cup PolicyHeadDots(S)
  ELSE LET W == 1..StopPos(S, u) IN
       UNION {GhostDots(S, QVal, n) : n \in W}
       \cup UNION {RowDots(S, n, u) : n \in {m \in W : Live(S, QVal, m)}}
       \cup PolicyHeadDots(S)

Unchanged(S, V, u) == VisitedDots(S, u) \subseteq V

(*************************** physical items (SSI granularity) *************)

Row(n) == <<"row", n>>          \* the node row: f, deleted_at, dots (incl. edge-set dots)
\* the conflict item of one node fact: under ssi Postgres tracks the whole
\* row; under occ the sequencer validates facts
FI(x) == IF CC = "ssi" THEN Row(x[2]) ELSE x
RowFacts(n) == {FI(<<"f", n>>), FI(<<"del", n>>)}
Ix(v, n) == <<"ix", v, n>>      \* the live partial-index key (v, n)
Gx(v, n) == <<"gx", v, n>>      \* index_ghosts key (v, n)
Erow(n, u) == <<"erow", n, u>>  \* the edge row (members, n, u)
PolRow == <<"pol">>
DefRow == <<"def">>

\* what the check (and Q's plan under it) reads, as Postgres tracks it
CheckReads(S, u) ==
  LET top == IF StopPos(S, u) >= PlanStop(S, u) THEN StopPos(S, u) ELSE PlanStop(S, u)
      W   == 1..top
      LiveW == {n \in W : Live(S, QVal, n)}
  IN (IF ReadSetRanges THEN {Ix(QVal, n) : n \in W} ELSE {})
     \cup (IF ReadSetRanges /\ IndexGhosts /\ Visit = "all"
           THEN {Gx(QVal, n) : n \in 1..StopPos(S, u)} ELSE {})
     \cup UNION {RowFacts(n) : n \in LiveW}
     \cup (IF PolicyInputs = "edgeset" THEN {FI(<<"es", n>>) : n \in LiveW} ELSE {})
     \cup (IF UsesMembers(S) THEN {Erow(n, u) : n \in LiveW} ELSE {})
     \cup (IF PolicyTracked
           THEN {PolRow} \cup (IF Val(S, <<"pol">>) = "viaDef" THEN {DefRow} ELSE {})
           ELSE {})

(*************************** planning one attempt **************************)

NoW == [x \in {} |-> 0]
NoAtt == [snap |-> 0, w |-> NoW, gh |-> {}, R |-> {}, W |-> {}]
Plan(ok, w, gh, R, W) == [ok |-> ok, w |-> w, gh |-> gh, R |-> R, W |-> W]

SetF(S, n, v) ==
  LET old == Val(S, <<"f", n>>) IN
  IF Val(S, <<"del", n>>) THEN Plan(FALSE, NoW, {}, RowFacts(n), {})   \* updating a deleted node fails
  ELSE IF old = v THEN Plan(TRUE, NoW, {}, RowFacts(n), {})            \* same value: moves nothing
  ELSE Plan(TRUE, (<<"f", n>> :> v),
            IF IndexGhosts THEN {<<old, n>>} ELSE {},
            RowFacts(n), {FI(<<"f", n>>), Ix(old, n), Ix(v, n), Gx(old, n)})

EdgeW(n, u, val) ==
  IF EdgeSetDot THEN (<<"mem", n, u>> :> val) @@ (<<"es", n>> :> "moved")
  ELSE (<<"mem", n, u>> :> val)
\* under ssi moving the edge-set dot updates the source row (a read and a
\* write); under occ it is a blind write of one fact
EdgeR(n, u) == {Erow(n, u)} \cup (IF EdgeSetDot /\ CC = "ssi" THEN {Row(n)} ELSE {})
EdgeWr(n, u) == {Erow(n, u)} \cup (IF EdgeSetDot THEN {FI(<<"es", n>>)} ELSE {})

PlanOp(S, op) ==
  CASE op.k = "setf" -> SetF(S, op.n, op.v)
    [] op.k = "del" ->
         LET f == Val(S, <<"f", op.n>>) IN
         IF Val(S, <<"del", op.n>>) THEN Plan(FALSE, NoW, {}, RowFacts(op.n), {})
         ELSE Plan(TRUE, (<<"del", op.n>> :> TRUE),
                   IF IndexGhosts THEN {<<f, op.n>>} ELSE {},
                   RowFacts(op.n), {FI(<<"del", op.n>>), Ix(f, op.n), Gx(f, op.n)})
    [] op.k = "restore" ->
         LET f == Val(S, <<"f", op.n>>) IN
         IF ~ Val(S, <<"del", op.n>>) THEN Plan(FALSE, NoW, {}, RowFacts(op.n), {})
         ELSE Plan(TRUE, (<<"del", op.n>> :> FALSE), {},
                   RowFacts(op.n), {FI(<<"del", op.n>>), Ix(f, op.n)})
    [] op.k = "link" ->
         IF Val(S, <<"mem", op.n, op.u>>) = "live"
         THEN Plan(TRUE, NoW, {}, {Erow(op.n, op.u)}, {})
         ELSE Plan(TRUE, EdgeW(op.n, op.u, "live"), {}, EdgeR(op.n, op.u), EdgeWr(op.n, op.u))
    [] op.k = "unlink" ->
         IF Val(S, <<"mem", op.n, op.u>>) # "live"
         THEN Plan(TRUE, NoW, {}, {Erow(op.n, op.u)}, {})
         ELSE Plan(TRUE, EdgeW(op.n, op.u, IF GhostEdges THEN "ghost" ELSE "none"), {},
                   EdgeR(op.n, op.u), EdgeWr(op.n, op.u))
    [] op.k = "pol" ->
         IF Val(S, <<"pol">>) = op.p THEN Plan(TRUE, NoW, {}, {PolRow}, {})
         ELSE Plan(TRUE, (<<"pol">> :> op.p), {}, {PolRow}, {PolRow})
    [] op.k = "def" ->
         IF Val(S, <<"def">>) = op.p THEN Plan(TRUE, NoW, {}, {DefRow}, {})
         ELSE Plan(TRUE, (<<"def">> :> op.p), {}, {DefRow}, {DefRow})
    [] op.k = "guard" ->
         LET V   == Prefix(op.ver)
             chk == Unchanged(S, V, op.u)
             wr  == IF op.tgt = 0 THEN Plan(TRUE, NoW, {}, {}, {})
                    ELSE SetF(S, op.tgt, "b")
         IN Plan(chk, wr.w, wr.gh, CheckReads(S, op.u) \cup wr.R, wr.W)

(*************************** transitions ***********************************)

Init ==
  /\ init \in InitStates
  /\ clog = << >>
  /\ pc = [t \in Txns |-> "idle"]
  /\ req = [t \in Txns |-> [k |-> "none"]]
  /\ att = [t \in Txns |-> NoAtt]

\* begin (first statement takes the snapshot), then reads, guards, staging
Begin(t) ==
  /\ pc[t] = "idle"
  /\ \E op \in IF req[t].k = "none"
               THEN Ops \cup GuardOps(Len0)
               ELSE {req[t]} :
       LET S == Cur
           p == PlanOp(S, op)
       IN /\ req' = [req EXCEPT ![t] = op]
          /\ IF p.ok
               THEN /\ pc' = [pc EXCEPT ![t] = "act"]
                    /\ att' = [att EXCEPT ![t] = [snap |-> Len0, w |-> p.w, gh |-> p.gh,
                                                   R |-> p.R, W |-> p.W]]
               ELSE /\ pc' = [pc EXCEPT ![t] = "failed"]  \* a require or guard failed
                    /\ att' = att
  /\ UNCHANGED <<init, clog>>

\* the dependency graph over a list L of attempts (snap = number of commits
\* the attempt's snapshot saw; position i in L is commit i)
InSnap(L, a, b) == a <= L[b].snap               \* a committed before b's snapshot
Conf(x, y) == (x.R \cap y.W) \cup (x.W \cap y.R) \cup (x.W \cap y.W) # {}
Edge(L, a, b) ==
  /\ a # b
  /\ \/ InSnap(L, a, b) /\ Conf(L[a], L[b])
     \/ ~ InSnap(L, a, b) /\ ~ InSnap(L, b, a) /\ L[a].R \cap L[b].W # {}

RECURSIVE ReachFrom(_, _, _)
ReachFrom(L, xs, k) ==
  IF k = 0 THEN xs
  ELSE ReachFrom(L, xs \cup {b \in 1..Len(L) : \E a \in xs : Edge(L, a, b)}, k - 1)
Succs(L, a) == ReachFrom(L, {b \in 1..Len(L) : Edge(L, a, b)}, Len(L))
Preds(L, b) == {a \in 1..Len(L) : b \in Succs(L, a)}
Acyclic(L) == \A a \in 1..Len(L) : a \notin Succs(L, a)

Rec(t) == LET h == Home(req[t]) IN
          [snap |-> att[t].snap, w |-> att[t].w, gh |-> att[t].gh, R |-> att[t].R,
           W |-> att[t].W, t |-> t, op |-> req[t], p |-> h,
           idx |-> 1 + Cardinality({i \in 1..Len0 : clog[i].p = h})]

CommitSSI(t) ==
  /\ LET r == Rec(t)
         L == Append(clog, r)
         fcw == \E i \in 1..Len0 : i > r.snap /\ clog[i].W \cap r.W # {}
     IN IF ~ fcw /\ Acyclic(L)
          THEN /\ clog' = L
               /\ pc' = [pc EXCEPT ![t] = "ok"]
          ELSE /\ clog' = clog                       \* serialization failure:
               /\ pc' = [pc EXCEPT ![t] = "idle"]    \* retry with a new dot
  /\ att' = [att EXCEPT ![t] = NoAtt]
  /\ UNCHANGED <<init, req>>

(* OCC: the home partition's sequencer appends the entry (index = next on  *)
(* that partition) iff no entry committed after the mutation's snapshot    *)
(* wrote into its read set. Items on other partitions are validated only  *)
(* with ValidateRemote.                                                    *)
CommitOCC(t) ==
  LET r == Rec(t)
      Checked == {x \in r.R : ValidateRemote \/ ItemPart(x) = r.p}
      clash == \E i \in 1..Len0 : i > r.snap /\ clog[i].W \cap Checked # {}
  IN /\ Assert(\A x \in r.W : ItemPart(x) = r.p, "write outside the home partition")
     /\ IF clash
          THEN /\ clog' = clog
               /\ pc' = [pc EXCEPT ![t] = "idle"]    \* validation failed: retry
          ELSE /\ clog' = Append(clog, r)
               /\ pc' = [pc EXCEPT ![t] = "ok"]
     /\ att' = [att EXCEPT ![t] = NoAtt]
     /\ UNCHANGED <<init, req>>

Commit(t) ==
  /\ pc[t] = "act"
  /\ IF CC = "ssi" THEN CommitSSI(t) ELSE CommitOCC(t)

Next == \E t \in Txns : Begin(t) \/ Commit(t)

Spec == Init /\ [][Next]_vars

(*************************** properties ************************************)

\* the versions a client can hold: every snapshot version (a commit-order
\* prefix) and every write token (its snapshot plus its own dot)
WriteTok(i) == Prefix(clog[i].snap) \cup {i}
\* with partitions, a version is a vector of per-partition log indexes; a
\* multi-partition read may take each partition at a different point
PartLen(p) == Cardinality({i \in 1..Len0 : clog[i].p = p})
PVer(vec) == {0} \cup {i \in 1..Len0 : clog[i].idx <= vec[clog[i].p]}
Vectors == {PVer(vec) : vec \in {v \in [Parts -> 0..Len0] : \A p \in Parts : v[p] <= PartLen(p)}}
Snaps == {Prefix(k) : k \in 0..Len0} \cup (IF MultiPart THEN Vectors ELSE {})
Versions == Snaps \cup {WriteTok(i) : i \in 1..Len0}

(* Soundness: whenever the check says "unchanged" for (Q, u, V) at a      *)
(* snapshot S, Q as u at V equals Q as u at S. S ranges over every         *)
(* snapshot so far that contains V (or every one, with StaleSnapshot).     *)
Soundness ==
  \A u \in Actors, V \in Versions, S \in Snaps :
    (V \subseteq S \/ StaleSnapshot) /\ Unchanged(S, V, u) => Q(V, u) = Q(S, u)

(* Atomicity of `require unchanged` with the commit. For every committed   *)
(* guarded mutation M, and every serial order consistent with the          *)
(* committed dependency graph, Q as u in the state just before M in that   *)
(* order equals Q as u at the V that M's check accepted: the result it     *)
(* relied on was still current at its serialization point. The states     *)
(* just before M are exactly the sets X of commits that contain every     *)
(* predecessor of M, no successor, and are closed under predecessors.      *)
Guarded(i) == clog[i].op.k = "guard"
SerialPoints(i) ==
  LET P  == Preds(clog, i)
      Sx == Succs(clog, i)
  IN {X \in SUBSET ((1..Len0) \ ({i} \cup Sx)) :
        /\ P \subseteq X
        /\ \A b \in X, a \in 1..Len0 : Edge(clog, a, b) => a \in X}
Atomicity ==
  \A i \in 1..Len0 : Guarded(i) =>
    \A X \in SerialPoints(i) :
      Q(X \cup {0}, clog[i].op.u) = Q(Prefix(clog[i].op.ver), clog[i].op.u)

(* Informational: the same, but at the wall-clock commit point (the       *)
(* commits that finished before M did). EXPECTED TO FAIL: a concurrent     *)
(* writer that M precedes in the serial order (M -rw-> T) may commit first.*)
AtomicityRealTime ==
  \A i \in 1..Len0 : Guarded(i) =>
    Q(Prefix(i - 1), clog[i].op.u) = Q(Prefix(clog[i].op.ver), clog[i].op.u)

(* OCC: `require unchanged` plus the sequencer's validation keep Q        *)
(* unchanged from V through the commit point: Q as u is the same at every  *)
(* commit-order prefix from V up to just before the mutation's entry.      *)
AtomicityOCC ==
  \A i \in 1..Len0 : Guarded(i) =>
    \A k \in clog[i].op.ver..(i - 1) :
      Q(Prefix(k), clog[i].op.u) = Q(Prefix(clog[i].op.ver), clog[i].op.u)

(* Informational: two links or unlinks through one source node never     *)
(* overlap in time. HOLDS when the edge-set dot is a column of the source  *)
(* row (they always conflict), FAILS without it.                           *)
EdgeOp(i) == clog[i].op.k \in {"link", "unlink"} /\ clog[i].w # NoW
NoConcurrentLinksOnOneSource ==
  \A i, j \in 1..Len0 :
    (i < j /\ EdgeOp(i) /\ EdgeOp(j) /\ clog[i].op.n = clog[j].op.n) => i <= clog[j].snap

TypeOK ==
  /\ pc \in [Txns -> {"idle", "act", "ok", "failed"}]
  /\ Len0 <= Cardinality(Txns)

Sym == Permutations(Txns)
=============================================================================
