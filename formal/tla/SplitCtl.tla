------------------------------ MODULE SplitCtl ------------------------------
(***************************************************************************)
(* Shep design §9.6 (the controller-driven split, the WAL controller, its *)
(* reconcilers, front ends' metadata caches), §8 (a two-phase commit in    *)
(* flight across the split), §10.3 (validation, idempotency).              *)
(*                                                                         *)
(* The protocol modelled is the one with the review's fixes, each behind a *)
(* constant so the unfixed protocol can be shown to fail:                  *)
(*  - routing: moved from a partition past its split entry carries the     *)
(*    split entry, and front ends route by it (RouteBy = "entry"), rather  *)
(*    than only by the controller map, which changes at finish_split;      *)
(*  - handoff: P's leader, or any later leader of P that finds a committed *)
(*    split entry, sends each child its handoff (HandoffResume); a child's *)
(*    first entry is idempotent by split revision; finish_split only       *)
(*    retires P and cleans up;                                             *)
(*  - cancel: P's log decides (CancelRule = "parent"): a cancel is a       *)
(*    split_aborted entry in P's log, exclusive with the split entry, and  *)
(*    cancel_split enters the controller log only after P reports it;      *)
(*  - drain: P rejects new prepares with a retryable `splitting`; T        *)
(*    retries, and after the split prepares in the child;                  *)
(*  - idempotency: a child's first entry references P's idempotency table  *)
(*    at i, which the child installs before it accepts proposals           *)
(*    (IdemInstall).                                                       *)
(*                                                                         *)
(* The controller log holds commands; a command's offset is its revision.  *)
(* Nodes: N1 hosts P's replica, N2 hosts C1's and C2's. Each node applies  *)
(* the controller log in order, and its reconciler creates and removes     *)
(* replicas per the table at its applied revision. P holds k1 (G1) and k2  *)
(* (G2); C1 gets G1, C2 gets G2. T is a two-phase commit homed at H with a *)
(* part writing k1. Crashes: P's leader (drain flag and pending handoff    *)
(* are leader memory).                                                     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  ReqKey,        \* request -> key
  With2PC,       \* include T
  RouteBy,       \* "entry" (fix) or "map" (front ends route by the map only)
  HandoffResume, \* TRUE (fix): any leader of P with a committed split entry
                 \*   sends the handoff; FALSE: only the leader that
                 \*   appended it, from memory
  CancelRule,    \* "parent" (fix) or "controller" (cancel_split alone)
  RemoveOnStart, \* TRUE: reconcilers remove P's replicas at start_split
  ChildEarly,    \* TRUE: a child accepts before its first entry commits
  IdemInstall,   \* TRUE (fix): a child accepts only after installing P's
                 \*   idempotency table; FALSE: it may accept before
  TooOld,        \* TRUE: a child rejects an S without (P, i)
  DrainGuard,    \* TRUE: the split entry waits for every prepare in P
  LostReplies,   \* a reply can be lost; the front end re-sends
  FreshReads,    \* TRUE: reads are at the partition's latest index (for the
                 \*   liveness checks: fairness can't stop a front end from
                 \*   re-reading a stale index forever otherwise)
  Crashes,       \* bound on P-leader crashes
  MaxLen         \* bound on any log's length

RK1  == ("r1" :> "k1")
RK11 == ("r1" :> "k1") @@ ("r2" :> "k1")

Reqs == DOMAIN ReqKey
Keys == {"k1", "k2"}
Data == {"P", "C1", "C2"}
Kids == {"C1", "C2"}
Kid(k) == IF k = "k1" THEN "C1" ELSE "C2"
Host(p) == IF p = "P" THEN "N1" ELSE "N2"
Nodes == {"N1", "N2"}
NoDot == <<"-", 0>>

VARIABLES
  clog,     \* the controller log: "start", "cancel", "finish"
  nrev,     \* node -> applied revision
  rep,      \* replicas that exist
  plog,     \* partition -> its log (P, C1, C2, H)
  drain,    \* P's leader is draining (leader memory)
  hoPend,   \* P's leader owes the children their handoff (leader memory)
  cancelReq,\* the balancer asked P's leader to abort the split
  inst,     \* children that installed P's idempotency table
  crashes,
  fe,       \* request -> front end
  tx,       \* T
  clock     \* commit counter

vars == <<clog, nrev, rep, plog, drain, hoPend, cancelReq, inst, crashes, fe, tx, clock>>

(*************************** the partition table ***************************)

SplitAt(r) ==
  IF \E j \in 1..r : clog[j] = "finish" THEN "finished"
  ELSE IF \E j \in 1..r : clog[j] = "cancel" THEN "cancelled"
  ELSE IF \E j \in 1..r : clog[j] = "start" THEN "started"
  ELSE "none"
Cur == Len(clog)
MapRoute(r, k) == IF SplitAt(r) = "finished" THEN Kid(k) ELSE "P"
Desired(r) ==
  CASE SplitAt(r) = "none"      -> {"P"}
    [] SplitAt(r) = "started"   -> IF RemoveOnStart THEN Kids ELSE {"P"} \cup Kids
    [] SplitAt(r) = "cancelled" -> {"P"}
    [] SplitAt(r) = "finished"  -> Kids

(*************************** logs ******************************************)

Prefix(p, n) == {<<p, i>> : i \in 1..n}
Ent(d) == plog[d[1]][d[2]]
\* contexts are stored closed (E closes them at append), so one step of
\* closure is the closure
Close(D) == D \cup UNION {Ent(d).ctx : d \in D}
E(kind, key, st, ctx, rq, t, idem) ==
  [kind |-> kind, key |-> key, st |-> st, ctx |-> Close(ctx), rq |-> rq, t |-> t, idem |-> idem]
IdxOf(kind) == IF \E i \in 1..Len(plog["P"]) : plog["P"][i].kind = kind
               THEN CHOOSE i \in 1..Len(plog["P"]) : plog["P"][i].kind = kind ELSE 0
SplitIdx == IdxOf("split")
AbortIdx == IdxOf("split_aborted")
Started(c) == Len(plog[c]) > 0
LMember(d, V) ==
  \/ d \in V
  \/ d[1] = "P" /\ SplitIdx > 0 /\ d[2] <= SplitIdx /\ \E e \in V : e[1] \in Kids

IsWrite(e) == e.kind \in {"mut", "rel"}
WritesOf(p) == {i \in 1..Len(plog[p]) : IsWrite(plog[p][i])}
\* T's prepare at q not yet released
PrepAt(q) == tx.st \in {"prep", "decided"} /\ tx.tgt = q

\* can q accept proposals (it holds k's range and is open)?
Open(q, k) ==
  /\ q \in rep
  /\ IF q = "P" THEN SplitIdx = 0
     ELSE /\ Kid(k) = q
          /\ \/ ChildEarly
             \/ SplitIdx > 0 /\ Started(q) /\ (q \in inst \/ ~ IdemInstall)
Idem(q) == {plog[q][i].rq : i \in 1..Len(plog[q])}
           \cup (IF q \in inst THEN UNION {plog[q][i].idem : i \in 1..Len(plog[q])} ELSE {})

(*************************** actions ***************************************)

FE0 == [pc |-> "idle", rev |-> 0, hint |-> FALSE, tgt |-> "-", S |-> {}]

Init ==
  /\ clog = << >>
  /\ nrev = [n \in Nodes |-> 0]
  /\ rep = {"P"}
  /\ plog = [p \in Data \cup {"H"} |-> << >>]
  /\ drain = FALSE
  /\ hoPend = FALSE
  /\ cancelReq = FALSE
  /\ inst = {}
  /\ crashes = 0
  /\ fe = [r \in Reqs |-> FE0]
  /\ tx = [st |-> IF With2PC THEN "init" ELSE "none", tgt |-> "P", S |-> {}, dec |-> NoDot]
  /\ clock = 0

\* --- the balancer ---
BStart ==
  /\ SplitAt(Cur) = "none" /\ Len(clog) = 0
  /\ clog' = Append(clog, "start")
  /\ UNCHANGED <<nrev, rep, plog, drain, hoPend, cancelReq, inst, crashes, fe, tx, clock>>

\* ask P's leader to split; re-asked until P's log decides
BAsk ==
  /\ SplitAt(Cur) = "started" /\ ~ drain /\ SplitIdx = 0 /\ AbortIdx = 0 /\ ~ cancelReq
  /\ drain' = TRUE
  /\ UNCHANGED <<clog, nrev, rep, plog, hoPend, cancelReq, inst, crashes, fe, tx, clock>>

\* the balancer decides to cancel (before it has seen the split entry)
BCancel ==
  /\ SplitAt(Cur) = "started" /\ ~ \E c \in Kids : Started(c) /\ ~ cancelReq
  /\ IF CancelRule = "controller"
     THEN /\ clog' = Append(clog, "cancel") /\ UNCHANGED cancelReq
     ELSE /\ cancelReq' = TRUE /\ UNCHANGED clog
  /\ UNCHANGED <<nrev, rep, plog, drain, hoPend, inst, crashes, fe, tx, clock>>

\* "parent": P's log decides between split_aborted and the split entry
PAbort ==
  /\ CancelRule = "parent" /\ cancelReq /\ SplitIdx = 0 /\ AbortIdx = 0 /\ "P" \in rep
  /\ Len(plog["P"]) < MaxLen
  /\ plog' = [plog EXCEPT !["P"] = Append(@, E("split_aborted", "-", NoDot, {}, "-", 0, {}))]
  /\ drain' = FALSE
  /\ UNCHANGED <<clog, nrev, rep, hoPend, cancelReq, inst, crashes, fe, tx, clock>>

\* cancel_split is recorded only after P reports split_aborted; if the split
\* entry won, the balancer drops its request and must finish
BRecordCancel ==
  /\ CancelRule = "parent" /\ cancelReq /\ SplitAt(Cur) = "started"
  /\ IF AbortIdx > 0
     THEN clog' = Append(clog, "cancel") /\ cancelReq' = FALSE
     ELSE SplitIdx > 0 /\ cancelReq' = FALSE /\ UNCHANGED clog
  /\ UNCHANGED <<nrev, rep, plog, drain, hoPend, inst, crashes, fe, tx, clock>>

BFinish ==
  /\ SplitAt(Cur) = "started" /\ \A c \in Kids : Started(c)
  /\ clog' = Append(clog, "finish")
  /\ UNCHANGED <<nrev, rep, plog, drain, hoPend, cancelReq, inst, crashes, fe, tx, clock>>

\* --- reconcilers ---
NApply(n) ==
  /\ nrev[n] < Cur
  /\ LET r == nrev[n] + 1
         mine == {q \in Data : Host(q) = n}
     IN /\ nrev' = [nrev EXCEPT ![n] = r]
        /\ rep' = (rep \ mine) \cup (Desired(r) \cap mine)
        /\ drain' = IF n = "N1" /\ SplitAt(r) = "cancelled" THEN FALSE ELSE drain
  /\ UNCHANGED <<clog, plog, hoPend, cancelReq, inst, crashes, fe, tx, clock>>

\* --- P's leader ---
PSplit ==
  /\ drain /\ SplitIdx = 0 /\ AbortIdx = 0 /\ "P" \in rep
  /\ SplitAt(nrev["N1"]) = "started"
  /\ DrainGuard => ~ PrepAt("P")
  /\ Len(plog["P"]) < MaxLen
  /\ plog' = [plog EXCEPT !["P"] = Append(@, E("split", "-", NoDot, Prefix("P", Len(plog["P"])), "-", 0, {}))]
  /\ hoPend' = TRUE
  /\ UNCHANGED <<clog, nrev, rep, drain, cancelReq, inst, crashes, fe, tx, clock>>

\* P's leader sends child c its handoff: the first entry, with context
\* {P: i} and a reference to P's idempotency table exactly at i, filtered
\* to the keys whose anchor group c holds (FINDINGS-2pc 2PC-10, 2PC-11).
\* Here a request's anchor is the group of the key it writes, and T's
\* release writes k1.
IdemAt(c) == {plog["P"][j].rq : j \in {x \in WritesOf("P") : x < SplitIdx /\ Kid(plog["P"][x].key) = c}}
PHandoff(c) ==
  /\ SplitIdx > 0 /\ c \in rep /\ ~ Started(c) /\ "P" \in rep
  /\ hoPend \/ HandoffResume
  /\ plog' = [plog EXCEPT ![c] = Append(@, E("first", "-", NoDot, Prefix("P", SplitIdx), "-", 0, IdemAt(c)))]
  /\ UNCHANGED <<clog, nrev, rep, drain, hoPend, cancelReq, inst, crashes, fe, tx, clock>>

\* a child installs P's idempotency table
CInstall(c) ==
  /\ Started(c) /\ c \notin inst /\ c \in rep
  /\ inst' = inst \cup {c}
  /\ UNCHANGED <<clog, nrev, rep, plog, drain, hoPend, cancelReq, crashes, fe, tx, clock>>

\* P's leader changes: the new leader lost the drain flag and pending handoff
PCrash ==
  /\ crashes < Crashes /\ (drain \/ hoPend)
  /\ drain' = FALSE /\ hoPend' = FALSE /\ crashes' = crashes + 1
  /\ UNCHANGED <<clog, nrev, rep, plog, cancelReq, inst, fe, tx, clock>>

\* --- front ends ---
Route(f, k) == IF f.hint THEN Kid(k) ELSE MapRoute(f.rev, k)
\* answer moved / not ready: catch up to the answering node's revision; a
\* partition past its split entry also sends the split entry (RouteBy)
Bounce(r, t) ==
  LET v == IF nrev[Host(t)] > fe[r].rev THEN nrev[Host(t)] ELSE fe[r].rev
      h == fe[r].hint \/ (RouteBy = "entry" /\ t = "P" /\ SplitIdx > 0)
  IN [fe[r] EXCEPT !.rev = v, !.hint = h]

FERead(r) ==
  /\ fe[r].pc = "idle"
  /\ LET t == Route(fe[r], ReqKey[r]) IN
     IF t \in rep /\ (t = "P" \/ Started(t))
     THEN \E n \in (IF FreshReads THEN Len(plog[t]) ELSE IF t = "P" THEN 0 ELSE 1)..Len(plog[t]) :
            fe' = [fe EXCEPT ![r].pc = "prop", ![r].tgt = t, ![r].S = Close(Prefix(t, n))]
     ELSE /\ fe' = [fe EXCEPT ![r] = [Bounce(r, t) EXCEPT !.pc = "idle"]]
          /\ fe' # fe
  /\ UNCHANGED <<clog, nrev, rep, plog, drain, hoPend, cancelReq, inst, crashes, tx, clock>>

FEPropose(r) ==
  /\ fe[r].pc = "prop"
  /\ LET t == fe[r].tgt
         k == ReqKey[r]
         S == fe[r].S
     IN CASE ~ Open(t, k) ->
               \* moved or not ready: re-execute, or re-send the same proposal
               \E again \in {"re-execute", "re-send"} :
                 LET b == Bounce(r, t) IN
                 /\ fe' = [fe EXCEPT ![r] = IF again = "re-execute"
                                            THEN [b EXCEPT !.pc = "idle", !.tgt = "-", !.S = {}]
                                            ELSE [b EXCEPT !.pc = "prop", !.tgt = Route(b, k)]]
                 /\ UNCHANGED <<plog, clock>>
          [] r \in Idem(t) ->                                              \* original outcome
               fe' = [fe EXCEPT ![r].pc = "done"] /\ UNCHANGED <<plog, clock>>
          [] t \in Kids /\ TooOld /\ (~ LMember(<<"P", SplitIdx>>, S) \/ ~ \E d \in S : d[1] = t) ->
               fe' = [fe EXCEPT ![r] = [fe[r] EXCEPT !.pc = "idle", !.tgt = "-", !.S = {}]]
               /\ UNCHANGED <<plog, clock>>                                \* too_old
          [] (k = "k1" /\ PrepAt(t))
             \/ \E i \in WritesOf(t) : plog[t][i].key = k /\ plog[t][i].st \notin S ->
               fe' = [fe EXCEPT ![r] = [fe[r] EXCEPT !.pc = "idle", !.tgt = "-", !.S = {}]]
               /\ UNCHANGED <<plog, clock>>                                \* contended
          [] OTHER ->
               /\ Len(plog[t]) < MaxLen
               /\ plog' = [plog EXCEPT ![t] = Append(@, E("mut", k, <<t, Len(plog[t]) + 1>>, S, r, clock + 1, {}))]
               /\ clock' = clock + 1
               /\ \E out \in IF LostReplies THEN {"done", "unknown"} ELSE {"done"} :
                    fe' = [fe EXCEPT ![r].pc = out]
  /\ UNCHANGED <<clog, nrev, rep, drain, hoPend, cancelReq, inst, crashes, tx>>

\* outcome unknown: re-send the same proposal to the same partition
FEResend(r) ==
  /\ fe[r].pc = "unknown"
  /\ fe' = [fe EXCEPT ![r].pc = "prop"]
  /\ UNCHANGED <<clog, nrev, rep, plog, drain, hoPend, cancelReq, inst, crashes, tx, clock>>

\* --- T (homed at H; its part writes k1 where k1 lives) ---
TRetry(t2) == tx' = [tx EXCEPT !.st = "init", !.tgt = t2, !.S = {}]

TRead ==
  /\ tx.st = "init" /\ tx.tgt \in rep /\ (tx.tgt = "P" \/ Started(tx.tgt))
  /\ \E n \in (IF FreshReads THEN Len(plog[tx.tgt]) ELSE IF tx.tgt = "P" THEN 0 ELSE 1)..Len(plog[tx.tgt]) :
       tx' = [tx EXCEPT !.st = "read", !.S = Close(Prefix(tx.tgt, n))]
  /\ UNCHANGED <<clog, nrev, rep, plog, drain, hoPend, cancelReq, inst, crashes, fe, clock>>

\* the participant's replica is gone (P retired): catch up to the answering
\* node's revision and route by the map
TRoute ==
  /\ tx.st = "init" /\ tx.tgt \notin rep
  /\ LET t2 == MapRoute(nrev[Host(tx.tgt)], "k1") IN
     /\ t2 # tx.tgt
     /\ tx' = [tx EXCEPT !.tgt = t2]
  /\ UNCHANGED <<clog, nrev, rep, plog, drain, hoPend, cancelReq, inst, crashes, fe, clock>>

TPrepare ==
  /\ tx.st = "read"
  /\ LET q == tx.tgt IN
     CASE q = "P" /\ SplitIdx > 0 -> TRetry("C1") /\ UNCHANGED plog          \* moved
       [] q = "P" /\ drain        -> TRetry("P") /\ UNCHANGED plog           \* splitting
       [] ~ Open(q, "k1")         -> TRetry(q) /\ UNCHANGED plog
       [] q \in Kids /\ (~ LMember(<<"P", SplitIdx>>, tx.S) \/ ~ \E d \in tx.S : d[1] = q) ->
            TRetry(q) /\ UNCHANGED plog                                      \* too_old
       [] \E i \in WritesOf(q) : plog[q][i].key = "k1" /\ plog[q][i].st \notin tx.S ->
            TRetry(q) /\ UNCHANGED plog                                      \* contended
       [] OTHER ->
            /\ Len(plog[q]) < MaxLen
            /\ plog' = [plog EXCEPT ![q] = Append(@, E("prep", "-", NoDot, tx.S, "T", 0, {}))]
            /\ tx' = [tx EXCEPT !.st = "prep"]
  /\ UNCHANGED <<clog, nrev, rep, drain, hoPend, cancelReq, inst, crashes, fe, clock>>

TDecide ==
  /\ tx.st = "prep"
  /\ LET d == <<"H", Len(plog["H"]) + 1>>
         pp == {<<tx.tgt, i>> : i \in {j \in 1..Len(plog[tx.tgt]) : plog[tx.tgt][j].kind = "prep"}}
     IN /\ plog' = [plog EXCEPT !["H"] = Append(@, E("dec", "-", d, Close(tx.S \cup pp), "T", 0, {}))]
        /\ tx' = [tx EXCEPT !.st = "decided", !.dec = d]
  /\ UNCHANGED <<clog, nrev, rep, drain, hoPend, cancelReq, inst, crashes, fe, clock>>

\* the release in the participant (refused once it has a split entry)
TRelease ==
  /\ tx.st = "decided"
  /\ LET q == tx.tgt IN
     /\ q \in rep /\ ~ (q = "P" /\ SplitIdx > 0) /\ Len(plog[q]) < MaxLen
     /\ plog' = [plog EXCEPT ![q] = Append(@, E("rel", "k1", tx.dec, Close({tx.dec}), "T", clock + 1, {}))]
  /\ clock' = clock + 1
  /\ tx' = [tx EXCEPT !.st = "released"]
  /\ UNCHANGED <<clog, nrev, rep, drain, hoPend, cancelReq, inst, crashes, fe>>

Next ==
  \/ BStart \/ BAsk \/ BCancel \/ PAbort \/ BRecordCancel \/ BFinish
  \/ \E n \in Nodes : NApply(n)
  \/ PSplit \/ PCrash \/ \E c \in Kids : PHandoff(c) \/ CInstall(c)
  \/ \E r \in Reqs : FERead(r) \/ FEPropose(r) \/ FEResend(r)
  \/ With2PC /\ (TRead \/ TRoute \/ TPrepare \/ TDecide \/ TRelease)

Spec == Init /\ [][Next]_vars

(* Fairness for the liveness checks: everything but crashes, the cancel   *)
(* decision, and C2's handoff (C2's node may stay down: one child being    *)
(* slow must not block the other's range).                                 *)
Fair ==
  /\ WF_vars(BStart) /\ WF_vars(BAsk) /\ WF_vars(PAbort) /\ WF_vars(BRecordCancel)
  /\ WF_vars(BFinish) /\ \A n \in Nodes : WF_vars(NApply(n))
  /\ WF_vars(PSplit) /\ WF_vars(PHandoff("C1")) /\ \A c \in Kids : WF_vars(CInstall(c))
  /\ \A r \in Reqs : WF_vars(FERead(r) \/ FEPropose(r) \/ FEResend(r))
  /\ WF_vars(TRead \/ TRoute \/ TPrepare \/ TDecide \/ TRelease)
LiveSpec == Spec /\ Fair

Bound == \A p \in DOMAIN plog : Len(plog[p]) <= MaxLen

(*************************** properties ************************************)

WRecs == UNION {{[key |-> plog[p][i].key, st |-> plog[p][i].st, p |-> p, i |-> i,
                  rq |-> plog[p][i].rq, t |-> plog[p][i].t, S |-> plog[p][i].ctx] :
                  i \in WritesOf(p)} : p \in Data}

Holds(w) ==
  \/ w.p = "P" /\ (SplitIdx = 0 \/ w.i < SplitIdx)
     /\ ("P" \in rep \/ (SplitIdx > 0 /\ Kid(w.key) \in rep /\ Started(Kid(w.key))))
  \/ w.p \in Kids /\ w.p = Kid(w.key) /\ w.p \in rep

\* No acknowledged write is lost (T: once decided, its part is applied
\* where k1 lives, or its release can still come).
NoLostAck ==
  /\ \A r \in Reqs : fe[r].pc = "done" => \E w \in WRecs : w.rq = r /\ Holds(w)
  /\ tx.st \in {"decided", "released"} =>
       \/ \E w \in WRecs : w.rq = "T" /\ Holds(w)
       \/ tx.st = "decided" /\ tx.tgt \in rep /\ ~ (tx.tgt = "P" /\ SplitIdx > 0)

\* No idempotency key commits twice across the split.
Inv_OneCommitPerKey ==
  /\ \A r \in Reqs : Cardinality({w \in WRecs : w.rq = r}) <= 1
  /\ Cardinality({w \in WRecs : w.rq = "T"}) <= 1

\* No key is owned by two live partitions: at most one accepts it.
OneOwner == \A k \in Keys : Cardinality({q \in Data : Open(q, k)}) <= 1

\* The table and P's log agree: a range the table gives to P, with no
\* split in progress, is one P still serves.
NoOrphanedRange == SplitAt(Cur) \in {"none", "cancelled"} => SplitIdx = 0

\* Validation completeness carries over to the children.
ValidationComplete ==
  \A w1, w2 \in WRecs : (w1.key = w2.key /\ w1.t < w2.t) => LMember(w1.st, w2.S)

\* No prepared transaction is stranded by the split entry.
NoStrandedPrepare == SplitIdx > 0 => ~ PrepAt("P")

TypeOK == rep \subseteq Data

\* --- liveness (with LiveSpec) ---
FEProgress == \A r \in Reqs : <>(fe[r].pc = "done")
DrainTerminates == (SplitAt(Cur) = "started") ~> (SplitIdx > 0 \/ AbortIdx > 0)
TFinishes == With2PC => <>(tx.st = "released")
HandoffC1 == (SplitIdx > 0) ~> (Started("C1") \/ SplitAt(Cur) = "cancelled")
=============================================================================
