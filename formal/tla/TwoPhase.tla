----------------------------- MODULE TwoPhase -----------------------------
(***************************************************************************)
(* Shep §8 (two-phase commit), §9 (dots, DVVs, §9.7 in-doubt resolution),  *)
(* §10.3 (validation, intents, read-only participants, registry barriers), *)
(* §9.6 (splits).                                                          *)
(*                                                                         *)
(* Split2 (design v49 §9.6 "Protocol: a split", §8 "Finding a retired      *)
(* home", §10.3 idempotency): partition 1 holds two placement groups, g1   *)
(* (its lowest; key x) and g2 (the anchor group of request A; no keys). It *)
(* splits into C1 = NP+1 (g1) and C2 = NP+2 (g2). The drain starts first:  *)
(* partition 1 rejects new prepares with a retryable `splitting` (the     *)
(* attempt aborts; a retry is a fresh attempt) and the split entry waits   *)
(* until no prepare at 1 is unreleased. The decision records go to C1, the *)
(* child holding the home's lowest group; a transaction ID whose home is   *)
(* 1 is routed there. A retry of A after the split is anchored at g2, so   *)
(* its home is C2, which checks the idempotency key against the parent's   *)
(* idempotency table at the split index, installed from a snapshot         *)
(* reference in its first entry.                                           *)
(*                                                                         *)
(* Partitions are logs. Raft is abstract: an append is committed at once,  *)
(* and the leader is the validator. Intents, the validation table and each *)
(* participant's per-transaction state are derived from the log, which is  *)
(* what "they are in the Raft state machine, so a new leader holds them"   *)
(* says; leader changes are therefore invisible here. The log is never     *)
(* truncated, so it doubles as the ground truth of what was decided. What  *)
(* a home can FORGET is its decision record (rec), which is state-machine  *)
(* state and is all that a home consults when asked for a decision.        *)
(*                                                                         *)
(* Keys: x on partition 1, y on 2, z on 3. Each attempt runs a template    *)
(* (PlanRW). An attempt reads at S = every dot committed when it executes  *)
(* (causally closed, since everything is committed), plans participants   *)
(* from where its keys live now, and then:                                 *)
(*   - one participant: a single-partition mutation (Propose);             *)
(*   - several: 2PC. Prepares go out in parallel as messages that can be   *)
(*     delayed (and, with Dup, duplicated) and are delivered whenever,     *)
(*     even after the coordinator crashed or the decision was forgotten.   *)
(* Every attempt is a fresh transaction ID; attempts of one request share  *)
(* its idempotency key (Req).                                              *)
(*                                                                         *)
(* Deadlines are abstract: Expire(a) is the moment a's deadline passes, by *)
(* the clock of whoever checks it (assumed monotone; see FINDINGS-2pc).    *)
(*                                                                         *)
(* Readers are not processes. The invariants quantify over causally        *)
(* closed versions V and over the position of the materializer serving    *)
(* each partition: from the lowest index that holds V's dots there (a      *)
(* lagging replica, or one rebuilt from an older checkpoint) to the end    *)
(* of the log. A part is applied only at its release. A materializer       *)
(* serving V that holds a prepare whose dot is in V but hasn't applied its *)
(* release first reads its own log up to the commit index (MatLookahead), *)
(* and asks the home only if the release isn't there (§9.7).               *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  NP,            \* partitions 1..NP hold keys (x on 1, y on 2, z on 3)
  Attempts,      \* attempts in this run (see PlanRW); "I2" retries "I1", "A2" retries "A"
  Crash,         \* coordinators can crash
  StampMode,     \* "decision": parts stamped with the decision dot (design)
                 \* "local": parts stamped with the participant's release dot
  CtxPrepDots,   \* TRUE: decision context has every prepare dot (design)
                 \* FALSE: it misses the last participant's prepare dot
  ReadOnlyParts, \* TRUE: partitions read by a guard join as read-only participants
  OnIntent,      \* "reject": contended (design); "wait": park until released
  PartHomeView,  \* "prepare": a participant asks the home named in its prepare (design)
                 \* "max": a participant takes the LAST participant as home (two homes)
  HomeRule,      \* "anchor": home = the request's anchor partition, where the
                 \*   idempotency key is recorded and checked (design)
                 \* "first": home = first participant of THIS attempt (per-attempt
                 \*   records in different places)
  MatLookahead,  \* a materializer reads its own log to the commit index before
                 \* asking the home (design)
  ForgetRule,    \* when the home drops a decision record:
                 \* "never"; "released": every prepared participant has released
                 \* (design); "deadline": released AND the deadline has passed
  Deadline,      \* transaction IDs carry deadlines: past it, prepares and commit
                 \* decisions are rejected, and participants forget released IDs.
                 \* FALSE: no deadline; participants forget an ID at its release
  Dup,           \* each prepare message may be delivered twice
  Barriers,      \* one registry barrier may arrive at each partition
  RegCheck,      \* "none": prepares record their registry dot, nothing else;
                 \* "decide": the decision aborts if a participant's latest
                 \*   barrier is newer than its prepare's (design; atomic here)
                 \* "drain": additionally, a barrier waits until no prepare is
                 \*   unreleased at its partition
  Split,         \* partition 1 may split into partition NP+1 (one child)
  SplitCarry,    \* a split carries the parent's decision records to the child
  Outages,       \* partitions can be unavailable (liveness under availability)
  Split2,        \* partition 1 may split into C1 (g1, x) and C2 (g2, A's anchor)
  DrainRule,     \* Split2's drain at 1: "reject" (design): new prepares get
                 \*   `splitting`; the split waits until no prepare at 1 is
                 \*   unreleased. "snapshot": new prepares are accepted, and the
                 \*   split waits only for those held when the drain started.
                 \*   "all": decisions and releases at 1 are rejected too
  LookupRoute,   \* where a transaction ID whose home split is routed:
                 \*   "lowest": the child holding the home's lowest group, where
                 \*   the records go (design); "anchor": the anchor group's child
  IdemTo,        \* which child gets the parent's idempotency keys: "anchor": the
                 \*   child holding each key's anchor group (design); "lowest":
                 \*   the child that gets the decision records
  IdemSnap       \* the idempotency snapshot: "split": at the split index
                 \*   (design); "earlier": any earlier index (e.g. the last
                 \*   periodic snapshot), with nothing replayed after it

ASSUME StampMode \in {"decision", "local"} /\ OnIntent \in {"reject", "wait"}
ASSUME PartHomeView \in {"prepare", "max"} /\ HomeRule \in {"anchor", "first"}
ASSUME ForgetRule \in {"never", "released", "deadline"} /\ NP \in {2, 3}
ASSUME RegCheck \in {"none", "decide", "drain"}
ASSUME ~ (Split /\ Split2) /\ (Split2 => NP = 2)
ASSUME DrainRule \in {"reject", "snapshot", "all"} /\ LookupRoute \in {"lowest", "anchor"}
ASSUME IdemTo \in {"anchor", "lowest"} /\ IdemSnap \in {"split", "earlier"}

C == NP + 1                                    \* the split child of partition 1 (Split2: C1)
C1 == NP + 1
C2 == NP + 2                                   \* Split2: the child holding g2
Splits == Split \/ Split2
Children == IF Split THEN {C} ELSE IF Split2 THEN {C1, C2} ELSE {}
Parts == IF Split THEN 1..(NP + 1) ELSE IF Split2 THEN 1..(NP + 2) ELSE 1..NP
KeyHome == [k \in {"x", "y", "z"} |-> CASE k = "x" -> 1 [] k = "y" -> 2 [] k = "z" -> 3]
Keys == {k \in {"x", "y", "z"} : KeyHome[k] <= NP}
NoDot == <<0, 0>>

Req(a) == CASE a \in {"I1", "I2"} -> "I" [] a \in {"A", "A2"} -> "A" [] OTHER -> a
Prior(a) == CASE a = "I2" -> "I1" [] a = "A2" -> "A" [] OTHER -> "none"
\* the anchor: the partition of the group named by the request's arguments
Anchor(r) == CASE r \in {"A", "B", "R1", "MX", "C"} -> 1
               [] r \in {"R2", "MY", "I"} -> 2
               [] r = "MZ" -> 3
\* Split2: requests anchored at partition 1 whose anchor group is g2
AnchorG2(r) == Split2 /\ r \in {"A", "B"}
AnchorChild(r) == IF AnchorG2(r) THEN C2 ELSE C1

\* reads and writes of an attempt, given what it read of y. "I" finds the
\* node it writes through y, so its participants are discovered at runtime.
PlanRW(a, yv) ==
  CASE Req(a) = "A"  -> [R |-> {"x", "y"}, W |-> {"x", "y"}]
    [] Req(a) = "B"  -> [R |-> {"x", "y"}, W |-> {"x", "y"}]
    [] Req(a) = "R1" -> [R |-> {"x", "y"}, W |-> {"x"}]
    [] Req(a) = "R2" -> [R |-> {"x", "y"}, W |-> {"y"}]
    [] Req(a) = "MX" -> [R |-> {"x"}, W |-> {"x"}]
    [] Req(a) = "MY" -> [R |-> {"y"}, W |-> {"y"}]
    [] Req(a) = "MZ" -> [R |-> {"z"}, W |-> {"z"}]
    [] Req(a) = "C"  -> [R |-> {"x", "y", "z"}, W |-> {"x", "z"}]
    [] Req(a) = "I"  -> IF yv = "init" THEN [R |-> {"x", "y"}, W |-> {"x", "y"}]
                                       ELSE [R |-> {"y", "z"}, W |-> {"y", "z"}]

NoPlan == [R |-> {}, W |-> {}, kp |-> [k \in Keys |-> 0], P |-> {}, home |-> 0]

VARIABLES
  log,      \* log[p]: the partition's committed entries
  rec,      \* rec[p]: attempts whose decision record p's state machine holds
  retired,  \* partition 1 has split
  up,       \* up[p]: p's quorum is available
  expired,  \* attempts whose deadline has passed
  pc,       \* coordinator: idle, single, parked, preparing, releasing, done, failed, crashed
  snap,     \* snap[a]: read version S, as a prefix vector
  rd,       \* rd[a][k]: the attempt whose write of k a read ("init"; "none" if unread)
  plan,     \* plan[a]: [R, W, kp (key -> partition), P (participants), home]
  msgs,     \* msgs[a][q]: undelivered copies of a's prepare to q
  rej,      \* some prepare of a was rejected
  park,     \* OnIntent = "wait": partitions where a's proposal is parked
  sp,       \* Split2: [draining, drainSet (prepared at 1 when the drain
            \*   started), snapIdx (the idempotency snapshot's index at 1)]
  stale,    \* ghost: committed although a read was no longer current
  regbad    \* ghost: committed although a participant's barrier was newer
            \*        than the registry its part was validated under

vars == <<log, rec, retired, up, expired, pc, snap, rd, plan, msgs, rej, park, stale, regbad, sp>>

(*************************** logs, dots, contexts **************************)

E(d) == log[d[1]][d[2]]
Idx(p) == 1..Len(log[p])
AllDots == UNION {{<<p, i>> : i \in Idx(p)} : p \in Parts}
Dots(S) == UNION {{<<p, i>> : i \in 1..S[p]} : p \in Parts}
InS(d, S) == d[2] <= S[d[1]]
Ctx(d) == E(d).ctx
Closed(V) == \A d \in V : Ctx(d) \subseteq V
RECURSIVE Clo(_)
Clo(X) == LET Y == X \cup UNION {Ctx(d) : d \in X} IN IF Y = X THEN X ELSE Clo(Y)
Max(S) == CHOOSE x \in S : \A y \in S : y <= x
Min(S) == CHOOSE x \in S : \A y \in S : x <= y
MaxIdx(V, q) == IF \E d \in V : d[1] = q THEN Max({d[2] : d \in {e \in V : e[1] = q}}) ELSE 0

Route(p) == IF Splits /\ p = 1 /\ retired THEN C ELSE p
Lineage(p) == IF Split /\ p \in {1, C} THEN {1, C}
              ELSE IF Split2 /\ p = 1 THEN {1, C1, C2}
              ELSE IF Split2 /\ p \in {C1, C2} THEN {1, p}
              ELSE {p}
\* where a transaction ID's home is routed (Split2: LookupRoute)
HRoute(h) == IF Split2 /\ h = 1 /\ retired /\ LookupRoute = "anchor" THEN C2 ELSE Route(h)
\* the anchor partition of a request, now
AnchorRoute(r) == IF AnchorG2(r) /\ retired THEN C2 ELSE Route(Anchor(r))
KP(k) == Route(KeyHome[k])
SplitAt == IF Splits /\ retired THEN CHOOSE i \in Idx(1) : log[1][i].k = "split" ELSE 0

Entry(k, a, ctx, out, dd, w, reg) ==
  [k |-> k, a |-> a, key |-> Req(a), ctx |-> ctx, out |-> out, dd |-> dd, w |-> w, reg |-> reg]

PrepIdx(a, q) == {i \in Idx(q) : log[q][i].k = "prep" /\ log[q][i].a = a}
RelIdx(a, q)  == {i \in Idx(q) : log[q][i].k = "rel" /\ log[q][i].a = a}
HasPrep(a, q) == PrepIdx(a, q) # {}
Unreleased(a, q) == Cardinality(PrepIdx(a, q)) > Cardinality(RelIdx(a, q))
DecDots(a) == UNION {{<<p, i>> : i \in {j \in Idx(p) : log[p][j].k = "dec" /\ log[p][j].a = a}}
                     : p \in Parts}

PartKeys(a, q) == {k \in plan[a].R \cup plan[a].W : plan[a].kp[k] = q}
PartR(a, q) == {k \in plan[a].R : plan[a].kp[k] = q}
PartW(a, q) == {k \in plan[a].W : plan[a].kp[k] = q}
WParts(a) == {q \in plan[a].P : PartW(a, q) # {}}

LatestBar(q) == IF \E i \in Idx(q) : log[q][i].k = "bar"
                THEN Max({i \in Idx(q) : log[q][i].k = "bar"}) ELSE 0
PrepReg(a, q) == IF HasPrep(a, q) THEN log[q][Min(PrepIdx(a, q))].reg ELSE 0
RegNewer(a) == \E q \in plan[a].P : LatestBar(q) > PrepReg(a, q)

(*************************** the home: decisions ***************************)

LinDec(h, a) == {d \in DecDots(a) : d[1] \in Lineage(h)}
First(D) == CHOOSE d \in D : \A e \in D : (d[1] < e[1]) \/ (d[1] = e[1] /\ d[2] <= e[2])
Last(D) == CHOOSE d \in D : \A e \in D : (d[1] > e[1]) \/ (d[1] = e[1] /\ d[2] >= e[2])
\* the home answers from its state-machine record only; the record is the
\* latest decision it appended (after a forget, a later decision replaces it)
Known(h, a) == a \in rec[h]
DecAt(h, a) == Last(LinDec(h, a))

\* ground truth: the decision in the log of the coordinator's home lineage
TrueHome(a) == HRoute(plan[a].home)
TrueDec(a) == IF LinDec(plan[a].home, a) = {} THEN NoDot ELSE First(LinDec(plan[a].home, a))
TrueOut(a) == IF TrueDec(a) = NoDot THEN "none" ELSE E(TrueDec(a)).out
TrueCommit(a) == TrueOut(a) = "commit"

\* the home a participant asks
PartHome(a, q) == IF PartHomeView = "max" THEN Max(plan[a].P) ELSE HRoute(plan[a].home)

\* "at most one commit per idempotency key", as home h sees it. Idempotency
\* records are kept for the idempotency window (longer than any run here),
\* and a split hands the window to the child (the lineage).
\* Split2: a child's table holds its own entries' keys, and the parent's
\* keys up to the snapshot index, if they were handed to it (IdemTo)
IdemSees(h, a, p, i) ==
  IF ~ Split2 THEN p \in Lineage(h)
  ELSE \/ p = h
       \/ /\ p = 1 /\ h \in {C1, C2} /\ i <= sp.snapIdx
          /\ h = (IF IdemTo = "anchor" THEN AnchorChild(Req(a)) ELSE C1)
IdemCommitted(h, a) ==
  \E b \in Attempts \ {a} :
     /\ Req(b) = Req(a)
     /\ \E p \in Parts : \E i \in Idx(p) :
          /\ IdemSees(h, a, p, i)
          /\ log[p][i].a = b /\ (log[p][i].k = "mut" \/ (log[p][i].k = "dec" /\ log[p][i].out = "commit"))

IsExpired(a) == Deadline /\ a \in expired

(*************************** versions of a key *****************************)

KeyLin(k) == IF Splits /\ KeyHome[k] = 1 THEN <<1, C>> ELSE <<KeyHome[k]>>

Applies(p, e, b, k) ==
  /\ e.a = b
  /\ \/ e.k = "mut" /\ k \in e.w
     \/ e.k = "rel" /\ e.out = "commit" /\ k \in plan[b].W /\ plan[b].kp[k] = p

\* decided, release not yet appended: a reader at a version holding the
\* decision waits for the release and then sees the write
InDoubtDecided(b, k) ==
  /\ StampMode = "decision"
  /\ k \in plan[b].W
  /\ Unreleased(b, plan[b].kp[k])
  /\ TrueCommit(b)

\* position of b's write of k in k's version order (0: none)
Pos(k, b) ==
  LET L == KeyLin(k)
      p1 == L[1]
      in1 == {i \in Idx(p1) : Applies(p1, log[p1][i], b, k)}
      in2 == IF Len(L) = 2 THEN {i \in Idx(L[2]) : Applies(L[2], log[L[2]][i], b, k)} ELSE {}
  IN IF in1 # {} THEN Max(in1)
     ELSE IF in2 # {} THEN Len(log[p1]) + Max(in2)
     ELSE IF InDoubtDecided(b, k) THEN 100
     ELSE 0

Writers(k) == {b \in Attempts : Pos(k, b) > 0}
Latest(k) == IF Writers(k) = {} THEN "init"
             ELSE CHOOSE b \in Writers(k) : \A c \in Writers(k) : Pos(k, c) <= Pos(k, b)
PosOf(k, b) == IF b = "init" THEN 0 ELSE Pos(k, b)

StaleReads(a) == \E k \in plan[a].R : Latest(k) # rd[a][k]

(*************************** validation (§10.3) ****************************)

StampAt(p, i) ==
  IF log[p][i].k = "rel" /\ StampMode = "decision" THEN log[p][i].dd ELSE <<p, i>>
WritesKey(p, i, k) ==
  LET e == log[p][i] IN
  \/ e.k = "mut" /\ k \in e.w
  \/ e.k = "rel" /\ e.out = "commit" /\ k \in plan[e.a].W /\ plan[e.a].kp[k] = p

TooOld(a, q) == Splits /\ q \in Children /\ snap[a][1] < SplitAt
StaleRegistry(a, q) == LatestBar(q) > snap[a][q]
StampConflict(a, q) ==
  \E i \in Idx(q) : \E k \in PartR(a, q) : WritesKey(q, i, k) /\ ~ InS(StampAt(q, i), snap[a])
IntentHolders(q) == {b \in Attempts : Unreleased(b, q)}
\* write intents block everything; read intents block only writes
Clash(a, b, q) ==
  \/ PartW(b, q) \cap PartKeys(a, q) # {}
  \/ PartKeys(b, q) \cap PartW(a, q) # {}
IntentConflict(a, q) == \E b \in IntentHolders(q) \ {a} : Clash(a, b, q)

CanAppend(p) == up[p] /\ ~ (Splits /\ p = 1 /\ retired)
\* Split2, DrainRule = "all": the draining parent rejects every 2PC record
Open2PC(p) == ~ (Split2 /\ DrainRule = "all" /\ sp.draining /\ ~ retired /\ p = 1)
\* Split2: the draining parent rejects a new prepare with `splitting`
Splitting(q) == Split2 /\ DrainRule # "snapshot" /\ sp.draining /\ q = 1

(*************************** actions ***************************************)

Init ==
  /\ log = [p \in Parts |-> << >>]
  /\ rec = [p \in Parts |-> {}]
  /\ retired = FALSE
  /\ up = [p \in Parts |-> TRUE]
  /\ expired = {}
  /\ pc = [a \in Attempts |-> "idle"]
  /\ snap = [a \in Attempts |-> [p \in Parts |-> 0]]
  /\ rd = [a \in Attempts |-> [k \in Keys |-> "none"]]
  /\ plan = [a \in Attempts |-> NoPlan]
  /\ msgs = [a \in Attempts |-> [p \in Parts |-> 0]]
  /\ rej = [a \in Attempts |-> FALSE]
  /\ park = [a \in Attempts |-> {}]
  /\ stale = {}
  /\ regbad = {}
  /\ sp = [draining |-> FALSE, drainSet |-> {}, snapIdx |-> 0]

AppendTo(p, e) == log' = [log EXCEPT ![p] = Append(log[p], e)]

\* read at S = everything committed now, and plan. A retry starts once its
\* first attempt has started (the client's deadline passed, or a crash).
Execute(a) ==
  /\ pc[a] = "idle"
  /\ IF Prior(a) = "none" THEN TRUE ELSE pc[Prior(a)] # "idle"
  /\ LET rw   == PlanRW(a, IF "y" \in Keys THEN Latest("y") ELSE "init")
         kp   == [k \in Keys |-> KP(k)]
         Pw   == {kp[k] : k \in rw.W}
         Pr   == {kp[k] : k \in rw.R}
         P0   == Pw \cup (IF ReadOnlyParts THEN Pr ELSE {})
         home == IF HomeRule = "first" THEN Min(P0) ELSE AnchorRoute(Req(a))
         P    == P0 \cup {home}
     IN /\ snap' = [snap EXCEPT ![a] = [p \in Parts |-> Len(log[p])]]
        /\ rd' = [rd EXCEPT ![a] = [k \in Keys |-> IF k \in rw.R THEN Latest(k) ELSE "none"]]
        /\ plan' = [plan EXCEPT ![a] = [R |-> rw.R, W |-> rw.W, kp |-> kp, P |-> P, home |-> home]]
        /\ IF Cardinality(P) = 1
             THEN /\ pc' = [pc EXCEPT ![a] = "single"]
                  /\ UNCHANGED msgs
             ELSE /\ pc' = [pc EXCEPT ![a] = "preparing"]
                  /\ msgs' = [msgs EXCEPT ![a] =
                                [q \in Parts |-> IF q \in P THEN (IF Dup THEN 2 ELSE 1) ELSE 0]]
  /\ UNCHANGED <<sp, log, rec, retired, up, expired, rej, park, stale, regbad>>

\* single-partition mutation: validate and append (§10.3)
SingleValidate(a, q) ==
  IF IdemCommitted(q, a)
    THEN /\ pc' = [pc EXCEPT ![a] = "done"] /\ park' = [park EXCEPT ![a] = {}]
         /\ UNCHANGED <<sp, log, stale>>
  ELSE IF TooOld(a, q) \/ StaleRegistry(a, q) \/ StampConflict(a, q)
    THEN /\ pc' = [pc EXCEPT ![a] = "failed"] /\ park' = [park EXCEPT ![a] = {}]
         /\ UNCHANGED <<sp, log, stale>>
  ELSE IF IntentConflict(a, q)
    THEN IF OnIntent = "wait"
           THEN /\ pc' = [pc EXCEPT ![a] = "parked"] /\ park' = [park EXCEPT ![a] = {q}]
                /\ UNCHANGED <<sp, log, stale>>
           ELSE /\ pc' = [pc EXCEPT ![a] = "failed"] /\ park' = [park EXCEPT ![a] = {}]
                /\ UNCHANGED <<sp, log, stale>>
  ELSE /\ AppendTo(q, Entry("mut", a, Dots(snap[a]), "none", NoDot, plan[a].W, LatestBar(q)))
       /\ stale' = IF StaleReads(a) THEN stale \cup {a} ELSE stale
       /\ pc' = [pc EXCEPT ![a] = "done"] /\ park' = [park EXCEPT ![a] = {}]

Propose(a) ==
  /\ pc[a] = "single"
  /\ LET q == CHOOSE p \in plan[a].P : TRUE IN
       /\ up[q]
       /\ IF Splits /\ q = 1 /\ retired
            THEN /\ pc' = [pc EXCEPT ![a] = "failed"]     \* moved
                 /\ UNCHANGED <<sp, log, stale, park>>
            ELSE SingleValidate(a, q)
  /\ UNCHANGED <<sp, rec, retired, up, expired, snap, rd, plan, msgs, rej, regbad>>

UnparkSingle(a) ==
  /\ pc[a] = "parked"
  /\ LET q == CHOOSE p \in park[a] : TRUE IN
       /\ CanAppend(q) /\ ~ IntentConflict(a, q)
       /\ SingleValidate(a, q)
  /\ UNCHANGED <<sp, rec, retired, up, expired, snap, rd, plan, msgs, rej, regbad>>

\* a participant validates a prepare and appends it, recording the registry
\* dot it was validated under (§8 step 1)
PrepValidate(a, q) ==
  IF TooOld(a, q) \/ StaleRegistry(a, q) \/ StampConflict(a, q)
    THEN /\ rej' = [rej EXCEPT ![a] = TRUE] /\ park' = [park EXCEPT ![a] = park[a] \ {q}]
         /\ UNCHANGED log
  ELSE IF IntentConflict(a, q)
    THEN IF OnIntent = "wait"
           THEN /\ park' = [park EXCEPT ![a] = park[a] \cup {q}] /\ UNCHANGED <<sp, log, rej>>
           ELSE /\ rej' = [rej EXCEPT ![a] = TRUE] /\ park' = [park EXCEPT ![a] = park[a] \ {q}]
                /\ UNCHANGED log
  ELSE /\ AppendTo(q, Entry("prep", a, Dots(snap[a]), "none", NoDot, {}, LatestBar(q)))
       /\ park' = [park EXCEPT ![a] = park[a] \ {q}]
       /\ UNCHANGED rej

\* what a participant remembers of an ID: until the deadline with
\* deadlines, otherwise only until its release
Remembers(a, q) == IF Deadline THEN HasPrep(a, q) \/ RelIdx(a, q) # {} ELSE Unreleased(a, q)

\* a prepare message arrives, possibly late: after a crash, a decision, a
\* release, or forgetting
Deliver(a, q) ==
  /\ msgs[a][q] > 0
  /\ q \notin park[a]
  /\ up[q]
  /\ msgs' = [msgs EXCEPT ![a][q] = @ - 1]
  /\ IF (Splits /\ q = 1 /\ retired) \/ IsExpired(a)
       THEN /\ rej' = [rej EXCEPT ![a] = TRUE] /\ UNCHANGED <<sp, log, park>>  \* moved / expired
     ELSE IF Remembers(a, q)
       THEN UNCHANGED <<sp, log, rej, park>>                                  \* known ID
     ELSE IF Splitting(q)
       THEN /\ rej' = [rej EXCEPT ![a] = TRUE] /\ UNCHANGED <<sp, log, park>>  \* splitting
     ELSE PrepValidate(a, q)
  /\ UNCHANGED <<sp, rec, retired, up, expired, pc, snap, rd, plan, stale, regbad>>

UnparkPrep(a, q) ==
  /\ q \in park[a]
  /\ CanAppend(q) /\ ~ IntentConflict(a, q)
  /\ PrepValidate(a, q)
  /\ UNCHANGED <<sp, rec, retired, up, expired, pc, snap, rd, plan, msgs, stale, regbad>>

DecCtx(a) ==
  Dots(snap[a]) \cup
  UNION {{<<q, i>> : i \in PrepIdx(a, q)} :
         q \in (IF CtxPrepDots THEN plan[a].P ELSE plan[a].P \ {Max(plan[a].P)})}

AppendDecision(h, a, out) ==
  /\ AppendTo(h, Entry("dec", a, IF out = "commit" THEN DecCtx(a) ELSE {}, out, NoDot, {}, 0))
  /\ rec' = [rec EXCEPT ![h] = @ \cup {a}]

\* the decision, at the home: the first decision for an ID wins; a commit
\* past the deadline is rejected (presumed abort, nothing appended); a
\* commit aborts if a participant's barrier is newer than its prepare, or
\* if another attempt of the same idempotency key already committed.
Decide(a) ==
  /\ pc[a] = "preparing"
  /\ ~ rej[a]
  /\ \A q \in plan[a].P : HasPrep(a, q)
  /\ LET h == TrueHome(a) IN
       /\ CanAppend(h) /\ Open2PC(h)
       /\ IF Known(h, a) \/ IsExpired(a)
            THEN UNCHANGED <<sp, log, rec, stale, regbad>>
          ELSE IF (RegCheck # "none" /\ RegNewer(a)) \/ IdemCommitted(h, a)
            THEN AppendDecision(h, a, "abort") /\ UNCHANGED <<sp, stale, regbad>>
          ELSE /\ AppendDecision(h, a, "commit")
               /\ stale' = IF StaleReads(a) THEN stale \cup {a} ELSE stale
               /\ regbad' = IF RegNewer(a) THEN regbad \cup {a} ELSE regbad
  /\ pc' = [pc EXCEPT ![a] = "releasing"]
  /\ UNCHANGED <<sp, retired, up, expired, snap, rd, plan, msgs, rej, park>>

CoordAbort(a) ==
  /\ pc[a] = "preparing"
  /\ rej[a]
  /\ LET h == TrueHome(a) IN
       /\ CanAppend(h) /\ Open2PC(h)
       /\ IF Known(h, a) \/ IsExpired(a) THEN UNCHANGED <<sp, log, rec>>
          ELSE AppendDecision(h, a, "abort")
  /\ pc' = [pc EXCEPT ![a] = "releasing"]
  /\ UNCHANGED <<sp, retired, up, expired, snap, rd, plan, msgs, rej, park, stale, regbad>>

\* prepare timeout at q: the home has no decision, so q appends the abort
\* there. Time is abstract: this can fire at any point.
TimeoutAbort(a, q) ==
  /\ Unreleased(a, q)
  /\ LET h == PartHome(a, q) IN
       /\ CanAppend(h) /\ Open2PC(h)
       /\ ~ Known(h, a)
       /\ ~ IsExpired(a)
       /\ AppendDecision(h, a, "abort")
  /\ UNCHANGED <<sp, retired, up, expired, pc, snap, rd, plan, msgs, rej, park, stale, regbad>>

\* intent release at q, from the decision at home h, sent by the coordinator
\* or learned by q after its timeout (the same entry either way); past the
\* deadline with no decision, the home answers "expired" and q aborts
ReleaseFrom(a, q, h) ==
  /\ Unreleased(a, q)
  /\ CanAppend(q) /\ Open2PC(q)
  /\ up[h]
  /\ IF Known(h, a)
       THEN LET d == DecAt(h, a) IN AppendTo(q, Entry("rel", a, {d}, E(d).out, d, {}, 0))
       ELSE /\ IsExpired(a)
            /\ AppendTo(q, Entry("rel", a, {}, "abort", NoDot, {}, 0))
  /\ UNCHANGED <<sp, rec, retired, up, expired, pc, snap, rd, plan, msgs, rej, park, stale, regbad>>

Release(a, q) == ReleaseFrom(a, q, PartHome(a, q))
CoordRelease(a, q) == pc[a] = "releasing" /\ ReleaseFrom(a, q, TrueHome(a))

\* the home drops a decision record once every participant that prepared has
\* released (ForgetRule = "deadline": and the deadline has passed)
ForgetDec(a) ==
  /\ ForgetRule # "never"
  /\ ForgetRule = "deadline" => a \in expired
  /\ \E h \in Parts :
       /\ a \in rec[h]
       /\ \A q \in Parts : ~ Unreleased(a, q)
       /\ rec' = [rec EXCEPT ![h] = @ \ {a}]
  /\ UNCHANGED <<sp, log, retired, up, expired, pc, snap, rd, plan, msgs, rej, park, stale, regbad>>

Expire(a) ==
  /\ Deadline /\ pc[a] # "idle" /\ a \notin expired
  /\ expired' = expired \cup {a}
  /\ UNCHANGED <<sp, log, rec, retired, up, pc, snap, rd, plan, msgs, rej, park, stale, regbad>>

\* a registry barrier reaches q (one per partition here)
Barrier(q) ==
  /\ Barriers /\ CanAppend(q)
  /\ LatestBar(q) = 0
  /\ RegCheck = "drain" => IntentHolders(q) = {}
  /\ AppendTo(q, Entry("bar", "none", {}, "none", NoDot, {}, 0))
  /\ UNCHANGED <<sp, rec, retired, up, expired, pc, snap, rd, plan, msgs, rej, park, stale, regbad>>

\* split of partition 1 into C (§9.6), after draining its prepared parts
DoSplit ==
  /\ Split /\ ~ retired /\ up[1]
  /\ IntentHolders(1) = {}
  /\ \A a \in Attempts : 1 \notin park[a]
  /\ LET i == Len(log[1]) + 1 IN
       log' = [log EXCEPT ![1] = Append(log[1], Entry("split", "none", {}, "none", NoDot, {}, 0)),
                          ![C] = Append(log[C], Entry("start", "none", {<<1, i>>}, "none", NoDot, {}, 0))]
  /\ rec' = [rec EXCEPT ![C] = IF SplitCarry THEN rec[1] ELSE {}, ![1] = {}]
  /\ retired' = TRUE
  /\ UNCHANGED <<sp, up, expired, pc, snap, rd, plan, msgs, rej, park, stale, regbad>>

\* Split2, step 2: the controller asks 1's leader to split; it starts draining
StartDrain ==
  /\ Split2 /\ ~ sp.draining /\ ~ retired
  /\ sp' = [sp EXCEPT !.draining = TRUE, !.drainSet = IntentHolders(1)]
  /\ UNCHANGED <<log, rec, retired, up, expired, pc, snap, rd, plan, msgs, rej, park, stale, regbad>>

\* Split2, steps 3-4: the split entry at i, and each child's first entry
\* (context {<<1, i>>}): C1 gets the decision records; each child installs
\* the idempotency snapshot before accepting proposals (atomic here)
DoSplit2 ==
  /\ Split2 /\ sp.draining /\ ~ retired /\ up[1]
  /\ IF DrainRule = "snapshot" THEN IntentHolders(1) \cap sp.drainSet = {}
                              ELSE IntentHolders(1) = {}
  /\ \A a \in Attempts : 1 \notin park[a]
  /\ LET i == Len(log[1]) + 1
         st == Entry("start", "none", {<<1, i>>}, "none", NoDot, {}, 0)
     IN /\ log' = [log EXCEPT ![1] = Append(log[1], Entry("split", "none", {}, "none", NoDot, {}, 0)),
                              ![C1] = Append(log[C1], st), ![C2] = Append(log[C2], st)]
        /\ \E j \in (IF IdemSnap = "split" THEN {i - 1} ELSE 0..(i - 1)) :
             sp' = [sp EXCEPT !.snapIdx = j]
  /\ rec' = [rec EXCEPT ![C1] = IF SplitCarry THEN rec[1] ELSE {}, ![1] = {}]
  /\ retired' = TRUE
  /\ UNCHANGED <<up, expired, pc, snap, rd, plan, msgs, rej, park, stale, regbad>>

CrashCoord(a) ==
  /\ Crash
  /\ pc[a] \in {"preparing", "releasing"}
  /\ pc' = [pc EXCEPT ![a] = "crashed"]
  /\ UNCHANGED <<sp, log, rec, retired, up, expired, snap, rd, plan, msgs, rej, park, stale, regbad>>

Down(p) == Outages /\ up[p] /\ up' = [up EXCEPT ![p] = FALSE]
           /\ UNCHANGED <<sp, log, rec, retired, expired, pc, snap, rd, plan, msgs, rej, park, stale, regbad>>
Up(p) == Outages /\ ~ up[p] /\ up' = [up EXCEPT ![p] = TRUE]
         /\ UNCHANGED <<sp, log, rec, retired, expired, pc, snap, rd, plan, msgs, rej, park, stale, regbad>>

Next ==
  \/ \E a \in Attempts :
       \/ Execute(a) \/ Propose(a) \/ UnparkSingle(a) \/ Decide(a) \/ CoordAbort(a)
       \/ ForgetDec(a) \/ CrashCoord(a) \/ Expire(a)
       \/ \E q \in Parts :
            \/ Deliver(a, q) \/ UnparkPrep(a, q) \/ TimeoutAbort(a, q) \/ Release(a, q)
            \/ (PartHomeView = "max" /\ CoordRelease(a, q))
  \/ DoSplit \/ StartDrain \/ DoSplit2
  \/ \E p \in Parts : Down(p) \/ Up(p) \/ Barrier(p)

Spec == Init /\ [][Next]_vars

Fairness ==
  /\ \A a \in Attempts :
       /\ WF_vars(Propose(a)) /\ WF_vars(UnparkSingle(a))
       /\ WF_vars(Decide(a)) /\ WF_vars(CoordAbort(a)) /\ WF_vars(ForgetDec(a))
       /\ WF_vars(Expire(a))
       /\ \A q \in Parts :
            /\ WF_vars(Deliver(a, q)) /\ WF_vars(UnparkPrep(a, q))
            /\ WF_vars(TimeoutAbort(a, q)) /\ WF_vars(Release(a, q))
LiveSpec == Spec /\ Fairness /\ WF_vars(DoSplit) /\ WF_vars(DoSplit2)

(*************************** what a reader sees ****************************)

\* the home's answer to "was a decided at a dot in V?"
ResolvesCommitIn(a, q, V) ==
  LET h == HRoute(plan[a].home) IN
  Known(h, a) /\ E(DecAt(h, a)).out = "commit" /\ DecAt(h, a) \in V

\* a materializer that holds a's prepare at q in V, but not its release at
\* pos, learns the outcome: from its own log's release if MatLookahead and
\* the release is committed, else from the home. "Commit at a dot in V"
\* means it waits for the release and applies the part: visible.
LearnsCommitIn(a, q, V) ==
  IF MatLookahead /\ RelIdx(a, q) # {}
  THEN \E i \in RelIdx(a, q) : log[q][i].out = "commit" /\ log[q][i].dd \in V
  ELSE ResolvesCommitIn(a, q, V)

\* is a's part on q visible to a reader at V served by a materializer at pos?
Vis(a, q, pos, V) ==
  LET rels == RelIdx(a, q)
      crels == {i \in rels : log[q][i].out = "commit"}
  IN IF StampMode = "local"
     THEN \E i \in crels : <<q, i>> \in V
     ELSE \/ \E i \in crels : i <= pos /\ log[q][i].dd \in V
          \/ /\ ~ \E i \in rels : i <= pos
             /\ \E j \in PrepIdx(a, q) : <<q, j>> \in V
             /\ LearnsCommitIn(a, q, V)

Positions(V, q) == {MaxIdx(V, q), Len(log[q])}

\* the smallest closed versions that can show a's part on p
Seeds(a, p) ==
  IF StampMode = "local"
  THEN {{<<p, i>>} : i \in RelIdx(a, p)}
  ELSE LET ds == {d \in DecDots(a) : E(d).out = "commit"} \cup
                 {log[p][i].dd : i \in {j \in RelIdx(a, p) : log[p][j].dd # NoDot}}
       IN {{d} : d \in ds} \cup UNION {{{d, <<p, j>>} : j \in PrepIdx(a, p)} : d \in ds}

Started(a) == plan[a].P # {}

(*************************** invariants ************************************)

TypeOK ==
  /\ pc \in [Attempts -> {"idle", "single", "parked", "preparing", "releasing", "done",
                          "failed", "crashed"}]
  /\ stale \subseteq Attempts /\ regbad \subseteq Attempts

\* 1. at most one decision per transaction ID, anywhere (EventuallyDecided
\*    gives "at least one, or expired")
OneDecision == \A a \in Attempts : Cardinality(DecDots(a)) <= 1

\* every release carries the outcome the coordinator's home decided
ReleaseAgrees ==
  \A a \in Attempts, q \in Parts : \A i \in RelIdx(a, q) :
    IF log[q][i].dd = NoDot THEN TrueOut(a) # "commit" ELSE log[q][i].dd = TrueDec(a)

\* 2. atomic visibility. A part's visibility is monotone in V and in the
\*    materializer's position, so a violation exists iff one exists at the
\*    closure of a smallest seed that shows some part, with each
\*    materializer at either extreme (AtomicVisibilityAllV cross-checks)
AtomicVisibility ==
  \A a \in Attempts : Started(a) =>
    \A p \in WParts(a) : \A s \in Seeds(a, p) :
      LET V == Clo(s) IN
      \A q \in WParts(a) : \A x \in Positions(V, p) : \A y \in Positions(V, q) :
        Vis(a, p, x, V) => Vis(a, q, y, V)

\* the same, literally over every causally closed V (small configs only)
AtomicVisibilityAllV ==
  \A V \in {W \in SUBSET AllDots : Closed(W)} :
    \A a \in Attempts : Started(a) =>
      \A p, q \in WParts(a) : \A x \in Positions(V, p) : \A y \in Positions(V, q) :
        Vis(a, p, x, V) => Vis(a, q, y, V)

\* 5. abort leaves no visible fact
AbortInvisible ==
  \A a \in Attempts :
    (Started(a) /\ ((\E d \in DecDots(a) : E(d).out = "abort") \/ (IsExpired(a) /\ ~ TrueCommit(a)))) =>
      \A q \in WParts(a) : \A x \in Positions(AllDots, q) : ~ Vis(a, q, x, AllDots)

\* the decision's context covers every read version and prepare dot
DecisionContextCovers ==
  \A a \in Attempts : \A d \in DecDots(a) : E(d).out = "commit" =>
    /\ Dots(snap[a]) \subseteq E(d).ctx
    /\ \A q \in plan[a].P : \A j \in PrepIdx(a, q) : <<q, j>> \in E(d).ctx

\* no prepare is ever resolved to the wrong outcome: what a materializer
\* holding the prepare would learn (lookahead, else the home) is commit at
\* the decision dot exactly when the transaction committed there
CommitDotOf(a) == IF TrueCommit(a) THEN TrueDec(a) ELSE NoDot
Learned(a, q) ==
  IF MatLookahead /\ RelIdx(a, q) # {}
  THEN LET i == Min(RelIdx(a, q)) IN IF log[q][i].out = "commit" THEN log[q][i].dd ELSE NoDot
  ELSE LET h == HRoute(plan[a].home) IN
       IF Known(h, a) /\ E(DecAt(h, a)).out = "commit" THEN DecAt(h, a) ELSE NoDot
ResolveCorrect ==
  \A a \in Attempts, q \in Parts : HasPrep(a, q) => Learned(a, q) = CommitDotOf(a)

\* a part is applied at most once; a mutation is appended at most once
NoDoubleApply ==
  \A a \in Attempts, q \in Parts :
    /\ q \in WParts(a) => Cardinality({i \in RelIdx(a, q) : log[q][i].out = "commit"}) <= 1
    /\ Cardinality({i \in Idx(q) : log[q][i].k = "mut" /\ log[q][i].a = a}) <= 1

Committed(a) ==
  \/ \E q \in Parts : \E i \in Idx(q) : log[q][i].k = "mut" /\ log[q][i].a = a
  \/ Started(a) /\ TrueCommit(a)

\* at most one commit per idempotency key, across attempts with different
\* homes or participant sets
AtMostOnce ==
  \A a, b \in Attempts : a # b /\ Req(a) = Req(b) => ~ (Committed(a) /\ Committed(b))

\* 6/7. every read of a committed mutation, including reads through a
\*    read-only participant, is still current when it commits: at the
\*    decision for 2PC, at the append for a single-partition mutation
ReadsValidAtCommit == stale = {}

\* no commit decision while a participant's latest barrier is newer than
\* the registry its part was validated under
RegistryAtDecision == regbad = {}

\* stronger, in log order: every release that applies writes after a
\* barrier was validated under that barrier
LatestBarBefore(q, i) == IF \E j \in 1..(i - 1) : log[q][j].k = "bar"
                         THEN Max({j \in 1..(i - 1) : log[q][j].k = "bar"}) ELSE 0
RegistryLogOrder ==
  \A q \in Parts : \A i \in Idx(q) :
    LET e == log[q][i] IN
    (e.k = "rel" /\ e.out = "commit" /\ PartW(e.a, q) # {}) => PrepReg(e.a, q) >= LatestBarBefore(q, i)

\* 6. serializability: the direct serialization graph (ww, wr, rw) over
\*    committed attempts, with each key's version order its apply order
CommittedSet == {a \in Attempts : Committed(a)}
DEdge(u, v) ==
  /\ u # v
  /\ \E k \in Keys :
       \/ Pos(k, u) > 0 /\ Pos(k, v) > 0 /\ Pos(k, u) < Pos(k, v)
       \/ k \in plan[v].R /\ rd[v][k] = u
       \/ k \in plan[u].R /\ Pos(k, v) > PosOf(k, rd[u][k])
RECURSIVE Reach(_, _)
Reach(X, n) == IF n = 0 THEN X
               ELSE Reach(X \cup {v \in CommittedSet : \E u \in X : DEdge(u, v)}, n - 1)
Serializable ==
  \A a \in CommittedSet :
    a \notin Reach({v \in CommittedSet : DEdge(a, v)}, Cardinality(CommittedSet))

\* §9.5 condition 3: writes to one key are causally ordered
StampOf(k, b) ==
  LET p == plan[b].kp[k] IN
  IF \E i \in Idx(p) : log[p][i].k = "mut" /\ log[p][i].a = b
  THEN <<p, CHOOSE i \in Idx(p) : log[p][i].k = "mut" /\ log[p][i].a = b>>
  ELSE IF StampMode = "local"
       THEN <<p, CHOOSE i \in RelIdx(b, p) : log[p][i].out = "commit">>
       ELSE TrueDec(b)
KeyWritesCausal ==
  \A k \in Keys : \A u, v \in Writers(k) :
    Pos(k, u) < Pos(k, v) => StampOf(k, u) \in Clo({StampOf(k, v)})

\* 4. no deadlock: no cycle of parked proposals waiting on each other's
\*    intents. Trivial with "reject", where nothing ever parks.
WaitsFor(a, b) == \E q \in park[a] : b \in IntentHolders(q) \ {a} /\ Clash(a, b, q)
NoDeadlock == \A a, b \in Attempts : ~ (WaitsFor(a, b) /\ WaitsFor(b, a))
NeverWaits == \A a \in Attempts : park[a] = {}

\* Split2: a retired partition never holds an unreleased prepare
NoStrandedPrepare == retired => \A a \in Attempts : ~ Unreleased(a, 1)

\* Split2 witnesses (expected to be violated: the paths are reachable)
\* a retry after the split, homed at the anchor's child
NoChildHome == \A a \in Attempts : plan[a].home \notin {C1, C2}
\* a decision record carried to C1 while a participant is still prepared
NoCarriedLookup == \A a \in Attempts : ~ (a \in rec[C1] /\ \E q \in Parts : Unreleased(a, q))
\* a participant still prepared after the split and the deadline
NoExpiredAfterSplit ==
  ~ (retired /\ \E a \in Attempts : plan[a].home = 1 /\ a \in expired /\ \E q \in Parts : Unreleased(a, q))
\* a prepare rejected by the drain
NoSplittingReject == ~ (sp.draining /\ \E a \in Attempts : rej[a] /\ 1 \in plan[a].P /\ ~ HasPrep(a, 1))

Safety ==
  /\ TypeOK /\ OneDecision /\ ReleaseAgrees /\ AtomicVisibility /\ AbortInvisible
  /\ DecisionContextCovers /\ ResolveCorrect /\ NoDoubleApply /\ AtMostOnce
  /\ ReadsValidAtCommit /\ Serializable /\ KeyWritesCausal /\ NoDeadlock /\ NeverWaits
  /\ RegistryAtDecision

(*************************** liveness **************************************)

AllUp == \A p \in Parts : up[p]

\* 3. intents are released, after any coordinator crash
IntentsReleased == \A a \in Attempts, q \in Parts : HasPrep(a, q) ~> ~ Unreleased(a, q)
\* every transaction that prepared somewhere is decided, or expires undecided
EventuallyDecided ==
  \A a \in Attempts : (\E q \in Parts : HasPrep(a, q)) ~> (TrueOut(a) # "none" \/ IsExpired(a))
\* every decision record is eventually dropped from the home's state machine
EventuallyForgotten ==
  \A a \in Attempts : (TrueOut(a) # "none") ~> (\A h \in Parts : a \notin rec[h])
\* completion is eventual only while the quorums are available
CompletionUnderAvailability == (<>[] AllUp) => (IntentsReleased /\ EventuallyDecided)
\* Split2: a drain that starts ends in the split
DrainTerminates == sp.draining ~> retired
=============================================================================
