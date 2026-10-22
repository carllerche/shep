------------------------------- MODULE Split -------------------------------
(***************************************************************************)
(* Shep design (doc version 30) §9.6 (partitions, splits at group          *)
(* boundaries, versions across a split, the split protocol and handoff),   *)
(* §9.4 (tokens), §8 (two-phase commit, anchors, finding a retired home,   *)
(* late messages), §10.3 (validation, idempotency, registry barriers),     *)
(* §3 (physical versions (gen, pos)) and §10.4 (checkpoints, compaction).  *)
(*                                                                         *)
(* Partition P holds two placement groups: key k1 is in G1 (the lower      *)
(* group) and k2 in G2. Split 1, at the G1/G2 boundary, retires P and      *)
(* starts C1 (G1) and C2 (G2). With Resplit, split 2 retires C1 at the     *)
(* boundary between G1 and groups the model leaves empty, and starts D1    *)
(* (G1): a grandchild. H is another partition, holding key h. M is the     *)
(* meta partition: its log holds one registry change. ParentPad entries    *)
(* that write nothing (other groups' work, Raft no-ops) start P's log.     *)
(*                                                                         *)
(* A log is a sequence of entries; an entry's position is its dot <<p,i>>.*)
(* Versions and contexts are explicit sets of dots, so prefixes and        *)
(* extras are exact. Contexts are stored closed, so the causal past of a   *)
(* set of dots is one step of closure under the entries' contexts.         *)
(*                                                                         *)
(* Front ends: one per plain write request (write key k), each with a      *)
(* cached partition map and a cached registry. It reads at S = a closed    *)
(* prefix of the partition its map routes to, plus its registry, and       *)
(* proposes (S, read set {k}, write k, idempotency key). The leader        *)
(* answers moved, stale_registry, too_old or contended (the front end      *)
(* refreshes and re-executes), returns the original outcome for a known    *)
(* idempotency key, or appends. A reply can be lost; the front end         *)
(* re-sends the same proposal.                                             *)
(*                                                                         *)
(* A split (§9.6): drain (the parent stops accepting prepares, waits until *)
(* every prepare it holds is released; may be cancelled) -> split entry at *)
(* i -> each child's first entry (context {parent: i} and the handoff:     *)
(* idempotency window, unforgotten decisions to the child holding the      *)
(* home's lowest group, released transaction IDs, latest registry dot,     *)
(* retention floor; its validation table is empty, so it answers too_old   *)
(* to any S that doesn't name it) -> map change -> finish (retired).       *)
(*                                                                         *)
(* T, a two-phase commit anchored in G1, so its home is P: participants P  *)
(* (writes k1) and H (writes h). Prepare in each (intents), a decision at  *)
(* the home (commit once both prepared; abort on a newer registry; or an   *)
(* abort appended by a timed-out participant, at the home as routed        *)
(* through the lineage), releases (the part becomes visible, stamped with  *)
(* the decision dot). WithDup: after P's release, a late duplicate of T's  *)
(* prepare for the k1 part arrives where k1 now lives.                     *)
(*                                                                         *)
(* The registry change: once committed, its barrier is delivered to the    *)
(* partitions holding the type; the deploy is acknowledged once delivery   *)
(* is complete. With Redeliver, a barrier for a parent that has split goes *)
(* to its children instead.                                                *)
(*                                                                         *)
(* Physical storage: each started partition has a SlateDB database. A      *)
(* key's versions are the writes of its own log plus, through the          *)
(* checkpoint clones, its ancestors' writes before their split entries.    *)
(* A version sorts by KeyOrder: "lineage" (generation, position) or        *)
(* "local" (position only; equal positions collide and the later write     *)
(* shadows the earlier). Compaction keeps a key's physically newest        *)
(* version and raises the floor; a child inherits both with the clone.     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  ReqKey,        \* plain write requests: request -> key ("k1" or "k2")
  With2PC,       \* include the two-phase commit T (participants P and H)
  THome,         \* T's home (its anchor's partition): "P" or "H"
  WithDup,       \* a late duplicate of T's prepare in P arrives after release
  WithReg,       \* include the registry change and its barriers
  Resplit,       \* also split C1 again, into the grandchild D1
  DrainGuard,    \* TRUE (design): the split entry waits until no prepare in
                 \*   the parent is outstanding; FALSE: it ignores them
  FirstCtx,      \* "parent" (design): a child's first entry has context
                 \*   {parent: i}; "none": it lacks it
  MovedCheck,    \* TRUE (design): after its split entry a parent rejects
                 \*   proposals and releases with moved; FALSE: a front end
                 \*   with a stale map can still commit in the parent
  ReleaseCtx,    \* "decision": an intent release's context contains the
                 \*   decision dot; "none": it is empty
  HandIdem,      \* handoff: the parent's idempotency window
  HandDecisions, \* handoff: unforgotten decisions, to the lowest-group child
  HandReleased,  \* handoff: released transaction IDs
  ForgetRule,    \* when the home forgets T's decision (and a split stops
                 \*   handing it off): "released" (§8: once every
                 \*   participant's release committed) or "deadline" (also
                 \*   once T's deadline has passed)
  Redeliver,     \* a barrier for a split parent is delivered to its children
  FoldMode,      \* "covered": a token's entry for a retired parent folds
                 \*   into the children only when it covers the parent up to
                 \*   i; "always": every such entry folds
  KeyOrder,      \* "lineage": (generation, position); "local": position
  ParentPad,     \* entries that write nothing at the start of P's log
  Compaction,    \* allow compaction
  LostReplies,   \* a reply can be lost (outcome unknown)
  AllowCancel,   \* the controller may cancel a drain once
  MaxLen         \* bound on any log's length (state constraint)

\* request maps for the configs
RK1  == ("r1" :> "k1")
RK11 == ("r1" :> "k1") @@ ("r2" :> "k1")
RK12 == ("r1" :> "k1") @@ ("r2" :> "k2")

Reqs == DOMAIN ReqKey
Keys == {"k1", "k2"}
Data == {"P", "C1", "C2", "D1"}
Parts == Data \cup {"H", "M"}
Children == {"C1", "C2", "D1"}
NoDot == <<"-", 0>>

Gen(p) == CASE p = "P" -> 0 [] p = "C1" -> 1 [] p = "C2" -> 1 [] p = "D1" -> 2 [] OTHER -> 0
Parent(c) == IF c = "D1" THEN "C1" ELSE "P"
SP == <<[par |-> "P", kids |-> {"C1", "C2"}], [par |-> "C1", kids |-> {"D1"}]>>
NSplits == IF Resplit THEN 2 ELSE 1
Splits == 1..NSplits
Retirable == {SP[s].par : s \in Splits}
\* the child that holds G1 (the lowest group): k1's, and the home's, heir
G1Kid(p) == IF p = "P" THEN "C1" ELSE "D1"
Desc(p) == CASE p = "P" -> {"C1", "C2", "D1"} [] p = "C1" -> {"D1"} [] OTHER -> {}
Anc(q) == {q} \cup {p \in Parts : q \in Desc(p)}
Chain(k) == CASE k = "k1" -> {"P", "C1", "D1"} [] k = "k2" -> {"P", "C2"} [] OTHER -> {"H"}

\* the partition map after v map changes
MapAt(v, k) ==
  IF v >= 2 /\ k = "k1" THEN "D1"
  ELSE IF v >= 1 THEN (IF k = "k1" THEN "C1" ELSE "C2")
  ELSE "P"

VARIABLES
  log,      \* partition -> sequence of entries
  mapv,     \* the meta partition's partition map: number of map changes
  ctl,      \* split -> "idle" "drain" "split" "mapped" "done"
  cancels,  \* drains cancelled so far
  fe,       \* request -> front end state
  tx,       \* the two-phase commit T
  regAck,   \* the registry deploy's acknowledgement: log lengths then
            \*   (regAck["M"] > 0 once acknowledged)
  dropped,  \* {<<db, <<p, i>>>>}: the write at <<p, i>> was compacted away in db
  floor     \* db -> set of stamps every read at db must contain

vars == <<log, mapv, ctl, cancels, fe, tx, regAck, dropped, floor>>

Dots(p) == {<<p, i>> : i \in 1..Len(log[p])}
AllDots == UNION {Dots(p) : p \in Parts}
Prefix(p, n) == {<<p, i>> : i \in 1..n}
Ent(d) == log[d[1]][d[2]]
Ctx(d) == Ent(d).ctx
Close(D) == D \cup UNION {Ctx(d) : d \in D}

(* An entry. kind: "mut" (writes key; stamp: its own dot), "prep" (T's    *)
(* part), "dec" (T's decision at its home; out = "commit" or "abort";     *)
(* st = its dot), "rel" (T's release: key = the key it writes, or "-" on  *)
(* abort; st = the decision dot), "split", "first" (a child's first entry *)
(* and its handoff: idem, decs, rels, reg), "bar" (a registry barrier),    *)
(* "reg" (the registry change, in M), "noop".                              *)
E(kind, key, st, ctx, rq) ==
  [kind |-> kind, key |-> key, st |-> st, ctx |-> Close(ctx), rq |-> rq,
   out |-> "-", idem |-> {}, decs |-> {}, rels |-> {}, reg |-> 0]

SplitIdx(p) == IF \E i \in 1..Len(log[p]) : log[p][i].kind = "split"
               THEN CHOOSE i \in 1..Len(log[p]) : log[p][i].kind = "split"
               ELSE 0
Started(c) == Len(log[c]) > 0
DBs == {q \in Data : q = "P" \/ Started(q)}

(* §9.6 lineage membership: (p, j) is in V iff it is listed, or p split at *)
(* i, j <= i, and V names a descendant of p.                               *)
LMember(d, V) ==
  \/ d \in V
  \/ d[1] \in Retirable /\ SplitIdx(d[1]) > 0 /\ d[2] <= SplitIdx(d[1])
     /\ \E e \in V : e[1] \in Desc(d[1])
LSub(A, V) == \A d \in A : LMember(d, V)

IsWrite(e) == e.kind \in {"mut", "rel"} /\ e.key # "-"
WritesOf(p) == {i \in 1..Len(log[p]) : IsWrite(log[p][i])}

\* where G1's partition is now: follow the lineage past split parents
Cur(p) == IF p \in Retirable /\ SplitIdx(p) > 0
          THEN (IF G1Kid(p) \in Retirable /\ SplitIdx(G1Kid(p)) > 0 THEN "D1" ELSE G1Kid(p))
          ELSE p

(*************************** the registry **********************************)

RegDot == <<"M", 1>>
RegCommitted == Len(log["M"]) > 0
\* the registry index a partition requires: from its barriers and handoff
Required(t) ==
  IF \E i \in 1..Len(log[t]) : log[t][i].kind = "bar" \/ log[t][i].reg > 0 THEN 1 ELSE 0
HasBarrier(t) == \E i \in 1..Len(log[t]) : log[t][i].kind = "bar"

(*************************** T's bookkeeping *******************************)

DecsIn(q) == UNION {log[q][i].decs : i \in 1..Len(log[q])}
DecIdx(q) == {i \in 1..Len(log[q]) : log[q][i].kind = "dec"}
\* T's decision as partition q holds it: in its log, or handed off
DecIn(q) ==
  IF DecIdx(q) # {} THEN log[q][CHOOSE i \in DecIdx(q) : TRUE].out
  ELSE IF DecsIn(q) # {} THEN (CHOOSE x \in DecsIn(q) : TRUE)[1]
  ELSE "none"
DecDot(q) ==
  IF DecIdx(q) # {} THEN <<q, CHOOSE i \in DecIdx(q) : TRUE>>
  ELSE (CHOOSE x \in DecsIn(q) : TRUE)[2]
\* T's home, routed through the split lineage (§8 "Finding a retired home")
Home == IF THome = "H" THEN "H" ELSE Cur("P")
HomeReady == Home \in {"P", "H"} \/ Started(Home)
Decision == IF HomeReady THEN DecIn(Home) ELSE "none"
CommitDecs == {d \in AllDots : Ent(d).kind = "dec" /\ Ent(d).out = "commit"}
\* q knows T was released: its log, or the handoff
Released(q) == \/ \E i \in 1..Len(log[q]) : log[q][i].kind = "rel" /\ log[q][i].rq = "T"
               \/ \E i \in 1..Len(log[q]) : "T" \in log[q][i].rels
\* prepares p holds that aren't released (the drain waits for them)
Outstanding(p) ==
  \/ p = "P" /\ tx.prepP /\ ~ tx.relP
  \/ tx.dup = p

(*************************** front ends ************************************)

FE0 == [pc |-> "idle", cache |-> 0, reg |-> 0, tgt |-> "-", S |-> {}]

Init ==
  /\ log = [p \in Parts |-> IF p = "P"
                             THEN [i \in 1..ParentPad |-> E("noop", "-", NoDot, {}, "-")]
                             ELSE << >>]
  /\ mapv = 0
  /\ ctl = [s \in Splits |-> "idle"]
  /\ cancels = 0
  /\ fe = [r \in Reqs |-> FE0]
  /\ tx = [st |-> IF With2PC THEN "init" ELSE "none", S |-> {}, prepP |-> FALSE,
           prepH |-> FALSE, relP |-> FALSE, relH |-> FALSE, dup |-> "none",
           expired |-> FALSE]
  /\ regAck = [p \in Parts |-> 0]
  /\ dropped = {}
  /\ floor = [q \in Parts |-> {}]

\* re-execute; the map and registry caches are refreshed independently
Refresh(r) == fe' = [fe EXCEPT ![r] = [FE0 EXCEPT !.cache = mapv, !.reg = fe[r].reg]]
RefreshReg(r) == fe' = [fe EXCEPT ![r] = [FE0 EXCEPT !.cache = fe[r].cache, !.reg = Len(log["M"])]]

\* a front end's cached registry may be refreshed at any time, or not
FERefreshReg(r) ==
  /\ WithReg /\ fe[r].pc = "idle" /\ fe[r].reg # Len(log["M"])
  /\ fe' = [fe EXCEPT ![r].reg = Len(log["M"])]
  /\ UNCHANGED <<log, mapv, ctl, cancels, tx, regAck, dropped, floor>>

\* read the key at a causally closed version of the routed partition
FERead(r) ==
  /\ fe[r].pc = "idle"
  /\ LET t == MapAt(fe[r].cache, ReqKey[r]) IN
     /\ t = "P" \/ Started(t)
     /\ \E n \in (IF t = "P" THEN 0 ELSE 1)..Len(log[t]) :
          LET S == Close(Prefix(t, n) \cup Prefix("M", fe[r].reg)) IN
          /\ LSub(floor[t], S)          \* not below the retention floor
          /\ fe' = [fe EXCEPT ![r].pc = "prop", ![r].tgt = t, ![r].S = S]
  /\ UNCHANGED <<log, mapv, ctl, cancels, tx, regAck, dropped, floor>>

Idem(t) == {log[t][i].rq : i \in 1..Len(log[t])} \cup
           UNION {log[t][i].idem : i \in 1..Len(log[t])}

Intent(t, k) == k = "k1" /\ ((t = "P" /\ tx.prepP /\ ~ tx.relP) \/ tx.dup = t)

\* the leader of fe[r].tgt validates and appends (§10.3, §9.6)
FEPropose(r) ==
  /\ fe[r].pc = "prop"
  /\ LET t == fe[r].tgt
         k == ReqKey[r]
         S == fe[r].S
     IN
     CASE MovedCheck /\ SplitIdx(t) > 0 ->                     \* moved
            /\ Refresh(r) /\ UNCHANGED log
       [] Required(t) > 0 /\ RegDot \notin S ->                \* stale_registry
            /\ RefreshReg(r) /\ UNCHANGED log
       [] r \in Idem(t) ->                                     \* original outcome
            /\ fe' = [fe EXCEPT ![r].pc = "done"] /\ UNCHANGED log
       [] t \in Children /\ (~ LMember(<<Parent(t), SplitIdx(Parent(t))>>, S)
                             \/ ~ \E d \in S : d[1] = t) ->   \* too_old
            /\ Refresh(r) /\ UNCHANGED log
       [] Intent(t, k) \/ \E i \in WritesOf(t) : log[t][i].key = k /\ log[t][i].st \notin S ->
            /\ Refresh(r) /\ UNCHANGED log                     \* contended
       [] OTHER ->
            /\ log' = [log EXCEPT ![t] = Append(@, E("mut", k, <<t, Len(log[t]) + 1>>, S, r))]
            /\ \E out \in IF LostReplies THEN {"done", "unknown"} ELSE {"done"} :
                 fe' = [fe EXCEPT ![r].pc = out]
  /\ UNCHANGED <<mapv, ctl, cancels, tx, regAck, dropped, floor>>

\* outcome unknown: re-send the same proposal, with the same idempotency
\* key, to the same partition's current leader
FEResend(r) ==
  /\ fe[r].pc = "unknown"
  /\ fe' = [fe EXCEPT ![r].pc = "prop"]
  /\ UNCHANGED <<log, mapv, ctl, cancels, tx, regAck, dropped, floor>>

(*************************** splits (§9.6) *********************************)

Draining(p) == \E s \in Splits : SP[s].par = p /\ ctl[s] = "drain"

CtlDrain(s) ==
  /\ ctl[s] = "idle" /\ IF s = 1 THEN TRUE ELSE ctl[s - 1] = "done"
  /\ ctl' = [ctl EXCEPT ![s] = "drain"]
  /\ UNCHANGED <<log, mapv, cancels, fe, tx, regAck, dropped, floor>>

CtlCancel(s) ==
  /\ AllowCancel /\ cancels = 0 /\ ctl[s] = "drain"
  /\ ctl' = [ctl EXCEPT ![s] = "idle"] /\ cancels' = 1
  /\ UNCHANGED <<log, mapv, fe, tx, regAck, dropped, floor>>

CtlSplit(s) ==
  /\ ctl[s] = "drain"
  /\ DrainGuard => ~ Outstanding(SP[s].par)
  /\ LET p == SP[s].par IN
     log' = [log EXCEPT ![p] = Append(@, E("split", "-", NoDot, Prefix(p, Len(log[p])), "-"))]
  /\ ctl' = [ctl EXCEPT ![s] = "split"]
  /\ UNCHANGED <<mapv, cancels, fe, tx, regAck, dropped, floor>>

\* the handoff from parent p at its split
HIdem(p) == ({log[p][j].rq : j \in 1..Len(log[p])} \ {"-"}) \cup UNION {log[p][j].idem : j \in 1..Len(log[p])}
\* unforgotten: T's decision while some participant hasn't released it
Forgotten == tx.relP /\ tx.relH /\ (ForgetRule = "deadline" => tx.expired)
HDecs(p, c) ==
  IF HandDecisions /\ c = G1Kid(p) /\ DecIn(p) # "none" /\ ~ Forgotten
  THEN {<<DecIn(p), DecDot(p)>>} ELSE {}
HRels(p) == IF HandReleased /\ Released(p) THEN {"T"} ELSE {}

ChildStart(s, c) ==
  /\ ctl[s] = "split" /\ c \in SP[s].kids /\ ~ Started(c)
  /\ LET p == SP[s].par
         i == SplitIdx(p)
         e0 == E("first", "-", NoDot, IF FirstCtx = "parent" THEN Prefix(p, i) ELSE {}, "-")
     IN /\ log' = [log EXCEPT ![c] = Append(@, [e0 EXCEPT !.idem = IF HandIdem THEN HIdem(p) ELSE {},
                                                          !.decs = HDecs(p, c),
                                                          !.rels = HRels(p),
                                                          !.reg = Required(p)])]
        /\ dropped' = dropped \cup {<<c, x[2]>> : x \in {y \in dropped : y[1] = p}}
        /\ floor' = [floor EXCEPT ![c] = floor[p]]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, tx, regAck>>

CtlMapChange(s) ==
  /\ ctl[s] = "split" /\ \A c \in SP[s].kids : Started(c)
  /\ ctl' = [ctl EXCEPT ![s] = "mapped"] /\ mapv' = s
  /\ UNCHANGED <<log, cancels, fe, tx, regAck, dropped, floor>>

CtlFinish(s) ==
  /\ ctl[s] = "mapped" /\ ctl' = [ctl EXCEPT ![s] = "done"]
  /\ UNCHANGED <<log, mapv, cancels, fe, tx, regAck, dropped, floor>>

(*************************** the registry change (§10.3) *******************)

RegCommit ==
  /\ WithReg /\ ~ RegCommitted
  /\ log' = [log EXCEPT !["M"] = Append(@, E("reg", "-", NoDot, {}, "-"))]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, tx, regAck, dropped, floor>>

\* where the meta leader delivers the barrier: the partitions its map
\* names; with Redeliver, a parent that has split is replaced by its
\* started children (and the deploy waits for a half-done split)
Live == {q \in Data : (q = "P" \/ Started(q)) /\ SplitIdx(q) = 0}
MapTargets == {MapAt(mapv, k) : k \in Keys}
Targets == IF Redeliver THEN Live ELSE MapTargets
Delivered(q) == Required(q) > 0

Deliver(q) ==
  /\ RegCommitted /\ q \in Targets /\ ~ Delivered(q)
  /\ log' = [log EXCEPT ![q] = Append(@, E("bar", "-", NoDot, {RegDot}, "-"))]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, tx, regAck, dropped, floor>>

RegAckNow ==
  /\ RegCommitted /\ regAck["M"] = 0
  /\ \A q \in Targets : Delivered(q)
  /\ Redeliver => \A s \in Splits : ctl[s] # "split"
  /\ regAck' = [p \in Parts |-> Len(log[p])]
  /\ UNCHANGED <<log, mapv, ctl, cancels, fe, tx, dropped, floor>>

(*************************** the two-phase commit T (§8) *******************)

TRead ==
  /\ tx.st = "init"
  /\ \E n \in 0..Len(log["P"]) :
       LET S == Close(Prefix("P", n) \cup Prefix("M", Len(log["M"]))) IN
       /\ LSub(floor["P"], S)
       /\ tx' = [tx EXCEPT !.st = "read", !.S = S]
  /\ UNCHANGED <<log, mapv, ctl, cancels, fe, regAck, dropped, floor>>

\* P validates its part and appends a prepare with intents on k1. It
\* refuses while it drains or after its split entry; T then never commits.
TPrepareP ==
  /\ tx.st = "read" /\ ~ tx.prepP
  /\ IF \/ Draining("P") \/ SplitIdx("P") > 0
        \/ Required("P") > 0 /\ RegDot \notin tx.S
        \/ \E i \in WritesOf("P") : log["P"][i].key = "k1" /\ log["P"][i].st \notin tx.S
     THEN /\ tx' = [tx EXCEPT !.st = "failed"] /\ UNCHANGED log
     ELSE /\ log' = [log EXCEPT !["P"] = Append(@, E("prep", "-", NoDot, tx.S, "T"))]
          /\ tx' = [tx EXCEPT !.prepP = TRUE]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, regAck, dropped, floor>>

TPrepareH ==
  /\ tx.st = "read" /\ ~ tx.prepH
  /\ log' = [log EXCEPT !["H"] = Append(@, E("prep", "-", NoDot, {}, "T"))]
  /\ tx' = [tx EXCEPT !.prepH = TRUE]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, regAck, dropped, floor>>

PrepDots == UNION {{<<p, i>> : i \in {j \in 1..Len(log[p]) : log[p][j].kind = "prep"}} : p \in {"P", "H"}}

\* the coordinator's decision at the home (as routed through the lineage).
\* It aborts if P's latest barrier is newer than the registry P's part was
\* computed under.
TDecideCommit ==
  /\ tx.prepP /\ tx.prepH /\ HomeReady /\ Decision = "none" /\ ~ tx.expired
  /\ LET out == IF Required("P") > 0 /\ RegDot \notin tx.S THEN "abort" ELSE "commit"
         d == <<Home, Len(log[Home]) + 1>>
     IN log' = [log EXCEPT ![Home] = Append(@, [E("dec", "-", d, tx.S \cup PrepDots, "T")
                                                EXCEPT !.out = out])]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, tx, regAck, dropped, floor>>

\* a timed-out participant (or the coordinator) finds no decision at the
\* home, as routed through the lineage, and appends an abort there
TDecideAbort ==
  /\ (tx.prepP /\ ~ tx.relP) \/ (tx.prepH /\ ~ tx.relH) \/ tx.dup \in Data
  /\ HomeReady /\ Decision = "none"
  /\ ~ (MovedCheck /\ SplitIdx(Home) > 0)
  /\ log' = [log EXCEPT ![Home] = Append(@, [E("dec", "-", <<Home, Len(log[Home]) + 1>>, {}, "T")
                                             EXCEPT !.out = "abort"])]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, tx, regAck, dropped, floor>>

RelEntry(key) ==
  E("rel", IF Decision = "commit" THEN key ELSE "-",
    IF Decision = "commit" THEN DecDot(Home) ELSE NoDot,
    IF ReleaseCtx = "decision" /\ Decision = "commit" THEN {DecDot(Home)} ELSE {}, "T")

TReleaseP ==
  /\ tx.prepP /\ ~ tx.relP /\ Decision # "none"
  /\ ~ (MovedCheck /\ SplitIdx("P") > 0)
  /\ log' = [log EXCEPT !["P"] = Append(@, RelEntry("k1"))]
  /\ tx' = [tx EXCEPT !.relP = TRUE]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, regAck, dropped, floor>>

TReleaseH ==
  /\ tx.prepH /\ ~ tx.relH /\ Decision # "none"
  /\ log' = [log EXCEPT !["H"] = Append(@, RelEntry("h"))]
  /\ tx' = [tx EXCEPT !.relH = TRUE]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, regAck, dropped, floor>>

\* T's deadline passes: from then on no commit decision is accepted
TExpire ==
  /\ tx.st # "init" /\ ~ tx.expired
  /\ tx' = [tx EXCEPT !.expired = TRUE]
  /\ UNCHANGED <<log, mapv, ctl, cancels, fe, regAck, dropped, floor>>

\* a late duplicate of T's prepare for the k1 part, after P's release; it
\* reaches where k1 lives now, which rejects a released transaction ID
TDup ==
  /\ WithDup /\ tx.relP /\ tx.dup = "none"
  /\ LET q == Cur("P") IN
     /\ q = "P" \/ Started(q)
     /\ IF Released(q) \/ (MovedCheck /\ SplitIdx(q) > 0)
        THEN /\ tx' = [tx EXCEPT !.dup = "rejected"] /\ UNCHANGED log
        ELSE /\ log' = [log EXCEPT ![q] = Append(@, E("prep", "-", NoDot, tx.S, "T"))]
             /\ tx' = [tx EXCEPT !.dup = q]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, regAck, dropped, floor>>

\* the duplicate's participant learns the decision and releases it
TDupRelease ==
  /\ tx.dup \in Data /\ Decision # "none"
  /\ ~ (MovedCheck /\ SplitIdx(tx.dup) > 0)
  /\ log' = [log EXCEPT ![tx.dup] = Append(@, RelEntry("k1"))]
  /\ tx' = [tx EXCEPT !.dup = "done"]
  /\ UNCHANGED <<mapv, ctl, cancels, fe, regAck, dropped, floor>>

(*************************** physical versions (§3, §10.4) *****************)

WRecs == UNION {{[key |-> log[p][i].key, st |-> log[p][i].st, p |-> p, i |-> i,
                  rq |-> log[p][i].rq] : i \in WritesOf(p)} : p \in Parts}
WId(w) == <<w.p, w.i>>

\* the writes of key k in db q: its own, and its ancestors' before their
\* split entries (the checkpoint clones)
Cands(q, k) ==
  {w \in WRecs : /\ w.key = k /\ w.p \in Anc(q)
                 /\ w.p = q \/ (SplitIdx(w.p) > 0 /\ w.i < SplitIdx(w.p))}

PKey(w) == IF KeyOrder = "lineage" THEN <<Gen(w.p), w.i>> ELSE <<0, w.i>>
Newer(a, b) == a[1] > b[1] \/ (a[1] = b[1] /\ a[2] > b[2])
\* with local positions only, a later write at an equal position replaces
\* the earlier version (one SlateDB key)
Shadowed(w, C) == \E w2 \in C : PKey(w2) = PKey(w) /\ Gen(w2.p) > Gen(w.p)
Present(q, k) ==
  {w \in Cands(q, k) : <<q, WId(w)>> \notin dropped /\ ~ Shadowed(w, Cands(q, k))}
PNewest(C) == CHOOSE w \in C : \A w2 \in C : w2 = w \/ Newer(PKey(w), PKey(w2))

\* compaction keeps a key's physically newest version and raises the floor
Compact(q, k) ==
  /\ Compaction /\ q \in DBs /\ q \in Chain(k)
  /\ LET C == Present(q, k) IN
     /\ Cardinality(C) >= 2
     /\ dropped' = dropped \cup {<<q, WId(w)>> : w \in C \ {PNewest(C)}}
     /\ floor' = [floor EXCEPT ![q] = @ \cup {w.st : w \in C}]
  /\ UNCHANGED <<log, mapv, ctl, cancels, fe, tx, regAck>>

Next ==
  \/ \E r \in Reqs : FERead(r) \/ FEPropose(r) \/ FEResend(r) \/ FERefreshReg(r)
  \/ \E s \in Splits : \/ CtlDrain(s) \/ CtlCancel(s) \/ CtlSplit(s)
                       \/ CtlMapChange(s) \/ CtlFinish(s)
                       \/ \E c \in Children : ChildStart(s, c)
  \/ RegCommit \/ RegAckNow \/ \E q \in Data : Deliver(q)
  \/ With2PC /\ \/ TRead \/ TPrepareP \/ TPrepareH \/ TDecideCommit \/ TDecideAbort
                \/ TReleaseP \/ TReleaseH \/ TDup \/ TDupRelease \/ TExpire
  \/ \E q \in Data, k \in Keys : Compact(q, k)

Spec == Init /\ [][Next]_vars

Bound == \A p \in Parts : Len(log[p]) <= MaxLen

(*************************** properties ************************************)

TypeOK ==
  /\ mapv \in 0..NSplits
  /\ \A r \in Reqs : fe[r].pc \in {"idle", "prop", "done", "unknown"}

Past(w) == Close(Ctx(w.st))

\* Does partition p hold key k at log position i?
Holds(p, i, k) ==
  /\ p \in Chain(k)
  /\ SplitIdx(p) = 0 \/ i < SplitIdx(p)
  /\ p \in {"P", "H"} \/ i > 1

(* No acknowledged write is lost. An acknowledged plain write was          *)
(* committed where its key lived. Once T has a commit decision, its k1     *)
(* part is applied in P before the split entry (or P hasn't split, so its  *)
(* release can still come), and H's part is not released as an abort.     *)
NoLostAck ==
  /\ \A r \in Reqs : fe[r].pc = "done" =>
       \E w \in WRecs : w.rq = r /\ Holds(w.p, w.i, w.key)
  /\ CommitDecs # {} =>
       /\ \/ \E w \in WRecs : w.key = "k1" /\ w.rq = "T" /\ Holds(w.p, w.i, w.key)
          \/ SplitIdx("P") = 0
       /\ ~ \E i \in 1..Len(log["H"]) : log["H"][i].kind = "rel" /\ log["H"][i].key = "-"

(* Inv_OneCommitPerKey: every idempotency key commits at most once across  *)
(* the lineage (retries that span the split included), T has a single     *)
(* outcome (at most one commit decision, never both commit and abort), and *)
(* each of T's parts is applied at most once.                              *)
Inv_OneCommitPerKey ==
  /\ \A r \in Reqs : Cardinality({w \in WRecs : w.rq = r}) <= 1
  /\ \A k \in {"k1", "h"} : Cardinality({w \in WRecs : w.rq = "T" /\ w.key = k}) <= 1
  /\ Cardinality(CommitDecs) <= 1
  /\ ~ (CommitDecs # {} /\ \E d \in AllDots : Ent(d).kind = "dec" /\ Ent(d).out = "abort")

\* No write commits in a parent after its split entry.
NoCommitAfterSplit ==
  \A p \in Retirable : SplitIdx(p) > 0 =>
    \A i \in (SplitIdx(p) + 1)..Len(log[p]) : ~ IsWrite(log[p][i])

\* Every write in a child is causally after its parent's split entry.
ChildrenAfterSplit ==
  \A w \in WRecs : w.p \in Children => <<Parent(w.p), SplitIdx(Parent(w.p))>> \in Past(w)

\* A key is written by one partition at a time: every write is committed
\* where the key lives at that point of the log (a stale map never leads to
\* a misplaced commit), and two writes to one key are causally ordered.
NoMisplacedCommit == \A w \in WRecs : Holds(w.p, w.i, w.key)
OneWriterPerKey ==
  \A w1, w2 \in WRecs :
    (w1 # w2 /\ w1.key = w2.key) => w1.st \in Past(w2) \/ w2.st \in Past(w1)

(* Registry barriers across a split. The entries computed under a         *)
(* registry are mutations, prepares, and commit decisions (a release only  *)
(* makes visible a part decided earlier: its commit point is its           *)
(* decision). In every data partition, such an entry after a barrier, or   *)
(* after a first entry that carries the registry dot, was computed with S  *)
(* containing the registry dot; and once the deploy is acknowledged, every *)
(* later one anywhere in the lineage (either child included) was.          *)
Computed(e) == e.kind \in {"mut", "prep"} \/ (e.kind = "dec" /\ e.out = "commit")
RegInForce(q, i) ==
  \E j \in 1..(i - 1) : log[q][j].kind = "bar" \/ log[q][j].reg > 0
BarrierCtx ==
  \A q \in Data : \A i \in 1..Len(log[q]) :
    Computed(log[q][i]) /\ RegInForce(q, i) => RegDot \in log[q][i].ctx
BarrierAcked ==
  regAck["M"] > 0 =>
    \A q \in Data \cup {"H"} : \A i \in (regAck[q] + 1)..Len(log[q]) :
      Computed(log[q][i]) /\ (q \in Data \/ log[q][i].kind = "dec") =>
        RegDot \in log[q][i].ctx

(* Versions clients can hold: every read version (a closed prefix of one   *)
(* partition), every context, every write token.                           *)
Tokens ==
  UNION {{Close(Prefix(p, n)) : n \in 0..Len(log[p])} : p \in Parts}
  \cup {Close(Ent(d).ctx) : d \in AllDots}
  \cup {Close(Ent(WId(w)).ctx \cup {w.st}) : w \in WRecs}

\* Lineage membership agrees with causal closure for every retired parent.
LineageConsistent ==
  \A V \in Tokens : LET CV == Close(V) IN
    \A p \in Retirable : \A j \in 1..Len(log[p]) : LMember(<<p, j>>, V) = (<<p, j>> \in CV)

(* Token folding (§9.6): once a parent is retired, a token's entry for it  *)
(* folds into the children (the token names their first entries).          *)
FoldS(s, V) ==
  LET p == SP[s].par IN
  IF ctl[s] # "done" \/ ~ \E d \in V : d[1] = p THEN V
  ELSE IF FoldMode = "always" \/ Prefix(p, SplitIdx(p)) \subseteq V
       THEN {d \in V : d[1] # p} \cup {<<c, 1>> : c \in SP[s].kids}
       ELSE V
Fold(V) == IF NSplits = 1 THEN FoldS(1, V) ELSE FoldS(2, FoldS(1, V))
LExp(V) == V \cup {d \in UNION {Dots(p) : p \in Retirable} : LMember(d, V)}
AnyRetired == \E s \in Splits : ctl[s] = "done"

\* folding never changes whether a write's dot of a retired parent is in
\* the token (dots that stamp nothing, such as the split entry, don't count)
FoldExact ==
  AnyRetired =>
    \A V \in Tokens, p \in Retirable :
      \A j \in WritesOf(p) : LMember(<<p, j>>, V) = LMember(<<p, j>>, Fold(V))
\* folding only adds (safe for at_least), and is monotone, on the dots that
\* stamp facts (a first entry, which folding names, stamps nothing: naming a
\* child at prefix 0 means {p: i}, §9.6)
Stamps(V) == {d \in LExp(V) \cap AllDots : Ent(d).kind \in {"mut", "dec"}}
FoldMonotone ==
  AnyRetired =>
    LET T  == Tokens
        LE == [V \in T |-> Stamps(V)]
        LF == [V \in T |-> Stamps(Fold(V))]
    IN \A V \in T : /\ LE[V] \subseteq LF[V]
                    /\ \A W \in T : LE[V] \subseteq LE[W] => LF[V] \subseteq LF[W]

(* SR-1. In every database that holds k, at every version V above its      *)
(* floor, the physically newest present version of k whose stamp is in V   *)
(* is the causally latest write of k in V, and one is present iff a write  *)
(* of k is in V.                                                           *)
PhysicalWinner ==
  LET T == Tokens IN
  \A q \in DBs, k \in Keys : q \in Chain(k) =>
    LET C  == Cands(q, k)
        Pr == Present(q, k)
    IN \A V \in T : LSub(floor[q], V) =>
         LET L   == {w \in C : LMember(w.st, V)}
             Vis == {w \in Pr : LMember(w.st, V)}
         IN /\ (L = {}) = (Vis = {})
            /\ Vis # {} => LET n == PNewest(Vis) IN
                             \A w2 \in L : w2 = n \/ w2.st \in Past(n)
=============================================================================
