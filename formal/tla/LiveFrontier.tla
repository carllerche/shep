---------------------------- MODULE LiveFrontier ----------------------------
(***************************************************************************)
(* Shep §9.7 (frontiers), §13.1–§13.3 (live queries stepping between       *)
(* published frontiers), §11.4 (hot window), §6.3 (actors and viewers),    *)
(* §6.8 (viewer-only and actor-reading policies).                          *)
(* Extends LiveRevocation.tla to two partitions, published frontiers, mid- *)
(* stream joins, resume with `since`, hot-window misses, and front-end     *)
(* crashes.                                                                *)
(*                                                                         *)
(* Partitions. A holds channel C1: its messages and its members. B holds   *)
(* channel C2 (members; its message q exists from the start), the actor    *)
(* and viewer nodes (@deleted_at), and the policy registry                 *)
(* (standing in for the meta partition). A version is a pair <<a, b>> of   *)
(* log prefixes. Each entry names the prefix of the other partition it     *)
(* causally depends on (its context); contexts are monotone in a log, so a *)
(* prefix pair is causally closed iff its last entries' contexts fit.      *)
(*                                                                         *)
(* The query is in_channel(C1) with includes(quotes). m1 is a plain post;  *)
(* m2 quotes q. Message.read delegates to C1, q's read delegates to C2,    *)
(* Channel.read = viewer in members. P2 adds `deny if actor is Agent`. A   *)
(* fresh evaluation as actor a at version V returns `denied` if a is       *)
(* deleted, else the messages of C1 (if a's viewer can read C1) plus q (if *)
(* it can also read C2 and m2 is present). Agent g1 runs as (actor g1,     *)
(* viewer u1), fixed by its act-for key (§6.3); a request whose viewer is  *)
(* deleted is also `denied`. A fresh read of a non-closed V                *)
(* closes it first (§9.7), so the reference is evaluated at Closure(V).    *)
(*                                                                         *)
(* Materializers apply each log at its own pace; the meta leader publishes *)
(* the largest causally closed version of what is applied (§9.7). The      *)
(* topic steps from V_topic to S. Phase 1: Δcore = messages in (V_t, S].   *)
(* Phase 2 keeps per subscriber a cached anchor flag (can read C1) and a   *)
(* cached quote flag (can read q), re-evaluated at S only when routed:     *)
(*   - a C1 membership edge whose member is the subscriber's viewer;       *)
(*   - a C2 membership edge for its viewer, if q is in the reference set;  *)
(*   - q entering the reference set (the data-change batched lookup);      *)
(*   - @deleted_at of its own actor node or its viewer node;               *)
(*   - a policy deploy.                                                    *)
(* Anchor access changing gives a resync; otherwise a delta, computed from *)
(* the topic's own old and new views. Deleting the subscriber's actor or   *)
(* its viewer closes the stream (`denied`).                                *)
(*                                                                         *)
(* P2 reads `actor`, so the reference set's policies are actor-reading     *)
(* (§6.8) and the topic is keyed by actor class (§13.2): phase 2's lookups *)
(* evaluate the actor terms per actor class. LookupByViewerOnly drops      *)
(* that: the batched lookup answers per viewer only (u1's answer for g1).  *)
(*                                                                         *)
(* Clients hold (copy, version of the last update applied). The front end  *)
(* can crash (topics are soft state); its streams drop, and clients resume *)
(* with `since` = their version. A replacement topic may start at any      *)
(* published frontier (another front end's topic, or one that follows a   *)
(* lagging meta replica). `since` older than the hot-window floor gets a   *)
(* resync.                                                                 *)
(*                                                                         *)
(* Knobs (design value first):                                             *)
(*  StepTo     "frontier": steps to published frontiers; "maxima": to the  *)
(*             per-partition applied maxima.                               *)
(*  JoinMode   "loop": joins and resumes are answered at V_topic;          *)
(*             "snapshot": at the latest published frontier, independently *)
(*             of the topic loop (cf. F4).                                 *)
(*  DenyDeletedViewer TRUE: a request whose viewer is deleted is `denied`, *)
(*             and its stream closes (§6.3, §13.2). FALSE: the superseded  *)
(*             mapping model (no viewer; FL4).                             *)
(*  LookupByViewerOnly FALSE: phase 2 evaluates actor terms per actor      *)
(*             class (§13.2); TRUE: it looks up by viewer only.            *)
(*  JoinWait   TRUE (proposed): a resume whose `since` is ahead of V_topic *)
(*             waits for the topic to reach it; FALSE: answered at once.   *)
(*  TopicStart "latest": a new topic starts at the latest frontier; "any": *)
(*             at any published frontier (a lagging front end).            *)
(*  Window     TRUE: the hot-window floor moves (retention), so resumes    *)
(*             can miss it.                                                *)
(*                                                                         *)
(* Superseded: the mapping model. g1's viewer was its acts_for target,     *)
(* which a "remap" entry in B re-points; only the superseded configs put   *)
(* "remap" in OpsB. ReRegister TRUE took the viewer node from each step's  *)
(* snapshot; FALSE, the one registered at subscribe. Without "remap", g1's *)
(* viewer is the constant u1 and ReRegister is inert.                      *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS MaxEntries, MaxA, MaxB, MaxMsgs, MaxCrashes, OpsA, OpsB, Deps, Subs,
          Window, StepTo, JoinMode, ReRegister, JoinWait, TopicStart,
          DenyDeletedViewer, LookupByViewerOnly

Users  == {"u1", "u2"}
Agents == {"g1"}
Actors == Users \cup Agents
NoV == "nobody"
Zero == <<0, 0>>

A0 == [msgs |-> {}, memA |-> {"u1", "u2"}]
B0 == [memB |-> {"u1", "u2"}, del |-> {}, owner |-> "u1", pol |-> "P1"]

VARIABLES
  logA, logB,   \* entries [k, x, dep]; dep = prefix of the other log it depends on
  histA, histB, \* histA[i + 1] = A's state after i entries
  ap,           \* ap[1], ap[2]: applied index of A's and B's materializer
  pubs,         \* the published frontiers (a chain)
  W,            \* hot-window floor (retention), a published frontier
  tex, Vt,      \* the front end's topic: exists, V_topic
  crashes,
  st,           \* stream state: "idle", "open", "dropped", "closed"
  cview, cver,  \* client copy and the version of the last update it applied
  audA, audB,   \* phase 2's cached flags: can read C1 (anchor), can read q
  reg,          \* viewer node registered for the subscriber (superseded)
  missed,       \* the stream's last (re)join was a hot-window-miss resync
  monoOK,       \* (1) versions never decrease along a stream
  pubOK,        \* (1) every update's version is a published, closed frontier
  delOK,        \* (2) no push to an actor deleted at the push's version
  safeOK,       \* (3) no push reveals what is unreadable at its version
  firstOK,      \* (4) a (re)joining stream's first update is at V_topic
  exactOK,      \* (5) after every update, client copy = fresh evaluation
  resyncOK      \* (6) after a hot-window-miss resync, every later update exact

vars == <<logA, logB, histA, histB, ap, pubs, W, tex, Vt, crashes, st, cview,
          cver, audA, audB, reg, missed, monoOK, pubOK, delOK, safeOK, firstOK,
          exactOK, resyncOK>>
okVars == <<monoOK, pubOK, delOK, safeOK, firstOK, exactOK, resyncOK>>
logVars == <<logA, logB, histA, histB>>

(*************************** versions **************************************)

Leq(v, w) == v[1] <= w[1] /\ v[2] <= w[2]
Max(S) == CHOOSE x \in S : \A y \in S : y <= x
Min(S) == CHOOSE x \in S : \A y \in S : x <= y
Range(s) == {s[i] : i \in DOMAIN s}

Closed(v) == /\ \A i \in 1..v[1] : logA[i].dep <= v[2]
             /\ \A j \in 1..v[2] : logB[j].dep <= v[1]
\* the least closed version containing v (a fresh read closes its version)
Closure(v) == LET C == {w \in (v[1]..Len(logA)) \X (v[2]..Len(logB)) : Closed(w)}
              IN <<Min({w[1] : w \in C}), Min({w[2] : w \in C})>>
\* the largest closed version within v (what the meta leader publishes)
Largest(v) == LET C == {w \in (0..v[1]) \X (0..v[2]) : Closed(w)}
              IN <<Max({w[1] : w \in C}), Max({w[2] : w \in C})>>

\* the latest published frontier, and the next one after v
F == CHOOSE f \in pubs : \A g \in pubs : Leq(g, f)
After(v) == {g \in pubs : Leq(v, g) /\ g /= v}
NextPub(v) == CHOOSE f \in After(v) : \A g \in After(v) : Leq(f, g)

St(v) == LET a == histA[v[1] + 1]
             b == histB[v[2] + 1]
         IN [msgs |-> a.msgs, memA |-> a.memA, memB |-> b.memB,
             del |-> b.del, owner |-> b.owner, pol |-> b.pol]

(*************************** policy ****************************************)

Viewer(a, s) == IF a \in Users THEN a
                ELSE IF s.owner \in s.del THEN NoV ELSE s.owner
CanA(a, s) == /\ Viewer(a, s) /= NoV /\ Viewer(a, s) \in s.memA
              /\ (s.pol = "P2" => a \notin Agents)
CanB(a, s) == /\ Viewer(a, s) /= NoV /\ Viewer(a, s) \in s.memB
              /\ (s.pol = "P2" => a \notin Agents)
Denied(a, s) == \/ a \in s.del
                \/ DenyDeletedViewer /\ a \in Agents /\ s.owner \in s.del
\* a view built from core at s and the two access flags
ViewOf(s, fa, fb) == IF fa THEN s.msgs \cup (IF fb /\ "m2" \in s.msgs THEN {"q"} ELSE {})
                     ELSE {}
FreshAt(a, s) == ViewOf(s, CanA(a, s), CanB(a, s))
\* Phase 2's answer: per actor class (the full policy), or by viewer only,
\* which drops the actor term (P2's deny for agents).
PA(a, s) == IF LookupByViewerOnly THEN Viewer(a, s) /= NoV /\ Viewer(a, s) \in s.memA
            ELSE CanA(a, s)
PB(a, s) == IF LookupByViewerOnly THEN Viewer(a, s) /= NoV /\ Viewer(a, s) \in s.memB
            ELSE CanB(a, s)
\* the view the topic computes for a at s
TopicView(a, s) == ViewOf(s, PA(a, s), PB(a, s))
\* the reference: a fresh evaluation as a at (the closure of) v
Ref(a, v) == FreshAt(a, St(Closure(v)))
RefDenied(a, v) == Denied(a, St(Closure(v)))

(*************************** init ******************************************)

Init ==
  /\ logA = << >> /\ logB = << >>
  /\ histA = << A0 >> /\ histB = << B0 >>
  /\ ap = Zero
  /\ pubs = {Zero}
  /\ W = Zero
  /\ tex = FALSE /\ Vt = Zero
  /\ crashes = 0
  /\ st = [a \in Subs |-> "idle"]
  /\ cview = [a \in Subs |-> {}]
  /\ cver = [a \in Subs |-> Zero]
  /\ audA = [a \in Subs |-> FALSE]
  /\ audB = [a \in Subs |-> FALSE]
  /\ reg = [a \in Subs |-> NoV]
  /\ missed = [a \in Subs |-> FALSE]
  /\ monoOK = TRUE /\ pubOK = TRUE /\ delOK = TRUE /\ safeOK = TRUE
  /\ firstOK = TRUE /\ exactOK = TRUE /\ resyncOK = TRUE

(*************************** writes ****************************************)

ApplyA(s, e) ==
  CASE e.k = "post" -> [s EXCEPT !.msgs = @ \cup {e.x}]
    [] e.k = "addA" -> [s EXCEPT !.memA = @ \cup {e.x}]
    [] e.k = "rmA"  -> [s EXCEPT !.memA = @ \ {e.x}]
ApplyB(s, e) ==
  CASE e.k = "addB"    -> [s EXCEPT !.memB = @ \cup {e.x}]
    [] e.k = "rmB"     -> [s EXCEPT !.memB = @ \ {e.x}]
    [] e.k = "del"     -> [s EXCEPT !.del = @ \cup {e.x}]
    [] e.k = "restore" -> [s EXCEPT !.del = @ \ {e.x}]
    [] e.k = "remap"   -> [s EXCEPT !.owner = e.x]
    [] e.k = "deploy"  -> [s EXCEPT !.pol = e.x]

CurA == histA[Len(logA) + 1]
CurB == histB[Len(logB) + 1]
NextMsg(s) == IF "m1" \in s.msgs THEN "m2" ELSE "m1"

PossA(s) ==
     (IF Cardinality(s.msgs) < MaxMsgs THEN {[k |-> "post", x |-> NextMsg(s)]} ELSE {})
  \cup {[k |-> "addA", x |-> u] : u \in Users \ s.memA}
  \cup {[k |-> "rmA", x |-> u] : u \in s.memA}
PossB(s) ==
     {[k |-> "addB", x |-> u] : u \in Users \ s.memB}
  \cup {[k |-> "rmB", x |-> u] : u \in s.memB}
  \cup {[k |-> "del", x |-> a] : a \in Actors \ s.del}
  \cup {[k |-> "restore", x |-> a] : a \in s.del}
  \cup {[k |-> "remap", x |-> u] : u \in Users \ {s.owner}}
  \cup {[k |-> "deploy", x |-> p] : p \in {"P1", "P2"} \ {s.pol}}

PrevDep(l) == IF l = << >> THEN 0 ELSE l[Len(l)].dep
DepChoices(l, other) == IF Deps THEN {PrevDep(l), other} ELSE {PrevDep(l)}

Room == Len(logA) + Len(logB) < MaxEntries

WriteA ==
  /\ Room /\ Len(logA) < MaxA
  /\ \E e \in PossA(CurA), d \in DepChoices(logA, Len(logB)) :
       /\ e.k \in OpsA
       /\ logA' = Append(logA, [k |-> e.k, x |-> e.x, dep |-> d])
       /\ histA' = Append(histA, ApplyA(CurA, e))
  /\ UNCHANGED <<logB, histB, ap, pubs, W, tex, Vt, crashes, st, cview, cver,
                 audA, audB, reg, missed>>
  /\ UNCHANGED okVars

WriteB ==
  /\ Room /\ Len(logB) < MaxB
  /\ \E e \in PossB(CurB), d \in DepChoices(logB, Len(logA)) :
       /\ e.k \in OpsB
       /\ logB' = Append(logB, [k |-> e.k, x |-> e.x, dep |-> d])
       /\ histB' = Append(histB, ApplyB(CurB, e))
  /\ UNCHANGED <<logA, histA, ap, pubs, W, tex, Vt, crashes, st, cview, cver,
                 audA, audB, reg, missed>>
  /\ UNCHANGED okVars

(*************************** materializers, frontier, retention ************)

\* Applied indexes are tracked only when the topic reads them (StepTo =
\* "maxima"). Otherwise the meta leader publishes the committed cut, which is
\* causally closed: every materializer has caught up. Any closed cut is the
\* committed cut of some interleaving of the writes.
ApplyEntry(p) ==
  /\ StepTo = "maxima"
  /\ ap[p] < (IF p = 1 THEN Len(logA) ELSE Len(logB))
  /\ ap' = [ap EXCEPT ![p] = @ + 1]
  /\ UNCHANGED <<logVars, pubs, W, tex, Vt, crashes, st, cview, cver, audA, audB,
                 reg, missed>>
  /\ UNCHANGED okVars

Publish ==
  LET G == IF StepTo = "maxima" THEN Largest(ap) ELSE <<Len(logA), Len(logB)>> IN
  /\ G /= F
  /\ pubs' = pubs \cup {G}
  /\ UNCHANGED <<logVars, ap, W, tex, Vt, crashes, st, cview, cver, audA, audB,
                 reg, missed>>
  /\ UNCHANGED okVars

\* the retention floor moves up one frontier (§10.4)
Retain ==
  /\ Window
  /\ After(W) /= {}
  /\ W' = NextPub(W)
  /\ UNCHANGED <<logVars, ap, pubs, tex, Vt, crashes, st, cview, cver, audA, audB,
                 reg, missed>>
  /\ UNCHANGED okVars

(*************************** the front end *********************************)

StartTopic ==
  /\ ~tex
  /\ \E v \in (IF TopicStart = "latest" THEN {F} ELSE pubs) : Vt' = v
  /\ tex' = TRUE
  /\ UNCHANGED <<logVars, ap, pubs, W, crashes, st, cview, cver, audA, audB,
                 reg, missed>>
  /\ UNCHANGED okVars

Crash ==
  /\ tex /\ crashes < MaxCrashes
  /\ tex' = FALSE
  /\ crashes' = crashes + 1
  /\ st' = [a \in Subs |-> IF st[a] = "open" THEN "dropped" ELSE st[a]]
  /\ UNCHANGED <<logVars, ap, pubs, W, Vt, cview, cver, audA, audB, reg, missed>>
  /\ UNCHANGED okVars

\* The mapped node registered at (re)join. With ReRegister, the step's
\* snapshot supplies it, so it is not kept (fewer states).
RegAt(a, s) == IF a \in Agents /\ ~ReRegister THEN s.owner ELSE NoV

\* The version at which a join or resume is answered.
JoinAt == IF JoinMode = "loop" THEN Vt ELSE F

\* A new subscription without `since`: its first update is a resync.
Subscribe(a) ==
  /\ st[a] \in {"idle", "closed"}
  /\ tex
  /\ LET V == JoinAt
         s == St(V)
     IN /\ ~Denied(a, s)
        /\ st' = [st EXCEPT ![a] = "open"]
        /\ audA' = [audA EXCEPT ![a] = PA(a, s)]
        /\ audB' = [audB EXCEPT ![a] = PB(a, s)]
        /\ reg' = [reg EXCEPT ![a] = RegAt(a, s)]
        /\ cview' = [cview EXCEPT ![a] = TopicView(a, s)]
        /\ cver' = [cver EXCEPT ![a] = V]
        /\ missed' = [missed EXCEPT ![a] = FALSE]
        /\ firstOK' = (firstOK /\ V = Vt)
        /\ pubOK' = (pubOK /\ Closed(V) /\ V \in pubs)
        /\ exactOK' = (exactOK /\ ~RefDenied(a, V) /\ TopicView(a, s) = Ref(a, V))
  /\ UNCHANGED <<logVars, ap, pubs, W, tex, Vt, crashes, monoOK, delOK, safeOK,
                 resyncOK>>

\* A client whose stream dropped resumes with since = its version (§13.1).
Resume(a) ==
  /\ st[a] = "dropped"
  /\ tex
  /\ JoinWait => Leq(cver[a], Vt)
  /\ LET V == JoinAt
         s == St(V)
     IN IF Denied(a, s)
        THEN /\ st' = [st EXCEPT ![a] = "closed"]
             /\ cview' = [cview EXCEPT ![a] = {}]
             /\ cver' = [cver EXCEPT ![a] = Zero]
             /\ UNCHANGED <<audA, audB, reg, missed, okVars>>
        ELSE
        LET inWin == Leq(W, cver[a])
            sv == TopicView(a, St(cver[a]))   \* the view at since, from the window
            nv == TopicView(a, s)
            after == IF inWin THEN (cview[a] \ (sv \ nv)) \cup (nv \ sv) ELSE nv
        IN /\ st' = [st EXCEPT ![a] = "open"]
           /\ audA' = [audA EXCEPT ![a] = PA(a, s)]
           /\ audB' = [audB EXCEPT ![a] = PB(a, s)]
           /\ reg' = [reg EXCEPT ![a] = RegAt(a, s)]
           /\ cview' = [cview EXCEPT ![a] = after]
           /\ cver' = [cver EXCEPT ![a] = V]
           /\ missed' = [missed EXCEPT ![a] = Window /\ ~inWin]
           /\ monoOK' = (monoOK /\ Leq(cver[a], V))
           /\ pubOK' = (pubOK /\ Closed(V) /\ V \in pubs)
           /\ firstOK' = (firstOK /\ V = Vt)
           /\ exactOK' = (exactOK /\ after = Ref(a, V))
           /\ resyncOK' = (resyncOK /\ (~inWin => after = Ref(a, V)))
           /\ UNCHANGED <<delOK, safeOK>>
  /\ UNCHANGED <<logVars, ap, pubs, W, tex, Vt, crashes>>

(*************************** a topic step to S *****************************)

Step(S) ==
  LET old == St(Vt)
      new == St(S)
      EA == {logA[i] : i \in (Vt[1] + 1)..S[1]}
      EB == {logB[i] : i \in (Vt[2] + 1)..S[2]}
      open == {a \in Subs : st[a] = "open"}
      mapped(a) == IF a \notin Agents THEN {}
                   ELSE IF ReRegister THEN {new.owner} ELSE {reg[a]}
      rDel(a) == \E e \in EB : e.k \in {"del", "restore"} /\ e.x \in {a} \cup mapped(a)
      rRemap(a) == a \in Agents /\ \E e \in EB : e.k = "remap"
      rDeploy == \E e \in EB : e.k = "deploy"
      rAll(a) == rDel(a) \/ rRemap(a) \/ rDeploy
      rMemA(a) == \E e \in EA : e.k \in {"addA", "rmA"} /\ e.x = Viewer(a, new)
      qOld == "m2" \in old.msgs
      qNew == "m2" \in new.msgs
      rMemB(a) == qOld /\ \E e \in EB : e.k \in {"addB", "rmB"} /\ e.x = Viewer(a, new)
      nA(a) == IF rAll(a) \/ rMemA(a) THEN PA(a, new) ELSE audA[a]
      nB(a) == IF rAll(a) \/ rMemB(a) \/ (qNew /\ ~qOld) THEN PB(a, new) ELSE audB[a]
      closing == {a \in open : rDel(a) /\ Denied(a, new)}
      stay == open \ closing
      oldV(a) == ViewOf(old, audA[a], audB[a])
      newV(a) == ViewOf(new, nA(a), nB(a))
      isResync(a) == nA(a) /= audA[a]
      rcpt == {a \in stay : isResync(a) \/ newV(a) /= oldV(a)}
      after(a) == IF isResync(a) THEN newV(a)
                  ELSE (cview[a] \ (oldV(a) \ newV(a))) \cup (newV(a) \ oldV(a))
  IN
  /\ Vt' = S
  /\ st' = [a \in Subs |-> IF a \in closing THEN "closed" ELSE st[a]]
  /\ audA' = [a \in Subs |-> IF a \in stay THEN nA(a) ELSE audA[a]]
  /\ audB' = [a \in Subs |-> IF a \in stay THEN nB(a) ELSE audB[a]]
  /\ cview' = [a \in Subs |-> IF a \in closing THEN {}
                              ELSE IF a \in rcpt THEN after(a) ELSE cview[a]]
  /\ cver' = [a \in Subs |-> IF a \in closing THEN Zero
                             ELSE IF a \in rcpt THEN S ELSE cver[a]]
  /\ monoOK' = (monoOK /\ \A a \in rcpt : Leq(cver[a], S))
  /\ pubOK' = (pubOK /\ (rcpt /= {} => Closed(S) /\ S \in pubs))
  /\ delOK' = (delOK /\ \A a \in rcpt : ~RefDenied(a, S))
  /\ safeOK' = (safeOK /\ \A a \in rcpt : after(a) \subseteq Ref(a, S))
  /\ exactOK' = (exactOK /\ \A a \in rcpt : after(a) = Ref(a, S))
  /\ resyncOK' = (resyncOK /\ \A a \in rcpt : missed[a] => after(a) = Ref(a, S))
  /\ UNCHANGED <<logVars, ap, pubs, W, tex, crashes, reg, missed, firstOK>>

StepAny ==
  /\ tex
  /\ IF StepTo = "frontier"
     THEN After(Vt) /= {} /\ Step(NextPub(Vt))
     ELSE LET S == <<ap[1], ap[2]>> IN Leq(Vt, S) /\ S /= Vt /\ Step(S)

Next ==
  \/ WriteA \/ WriteB
  \/ ApplyEntry(1) \/ ApplyEntry(2)
  \/ Publish \/ Retain
  \/ StartTopic \/ Crash
  \/ StepAny
  \/ \E a \in Subs : Subscribe(a) \/ Resume(a)

Spec == Init /\ [][Next]_vars

(*************************** properties ************************************)

\* Published frontiers are causally closed and totally ordered (monotone:
\* Publish only adds a cut above F; Largest is monotone in ap).
FrontierOK == /\ \A f \in pubs : Closed(f)
              /\ \A f, g \in pubs : Leq(f, g) \/ Leq(g, f)
              /\ W \in pubs

\* 1. Versions only increase; each one is a published, causally closed
\*    frontier. (No gaps: deltas chain, which (5) checks on the client's copy.)
Monotone == monoOK
Published == pubOK /\ FrontierOK
\* 2. Deleting the subscriber's actor (or, with DenyDeletedViewer, its
\*    viewer) closes the stream before any later push.
DeleteCloses == /\ delOK
                /\ \A a \in Subs : st[a] = "open" => ~RefDenied(a, Vt)
\* 3. A revocation or mapping change at R hides data in every push at a
\*    version >= R.
PushSafe == safeOK
\* 4. A (re)joining stream's first update is at the topic's version.
FirstAtTopic == firstOK
\* 5. The differential property: after every update, and at every version
\*    the topic reaches, an open stream's copy = a fresh evaluation.
PushExact == exactOK
ViewExact == \A a \in Subs : st[a] = "open" => cview[a] = Ref(a, Vt)
\* 6. A resync after a hot-window miss restores exactness.
ResyncExact == resyncOK

\* Reachability witnesses, for vacuity checks only (each is expected to be
\* violated in the design configs: the paths they deny are reachable).
WitnessNoMissResync == \A a \in Subs : ~missed[a]
WitnessNeverClosed == \A a \in Subs : st[a] /= "closed"
WitnessNeverQuote == \A a \in Subs : "q" \notin cview[a]
WitnessNoAgentLoss == \A a \in Subs \cap Agents : st[a] = "open" /\ ~audA[a] => cver[a] = Zero

TypeOK == /\ Vt \in (0..MaxA) \X (0..MaxB)
          /\ st \in [Subs -> {"idle", "open", "dropped", "closed"}]
=============================================================================
