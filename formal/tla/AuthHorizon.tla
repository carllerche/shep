---------------------------- MODULE AuthHorizon ----------------------------
(***************************************************************************)
(* Shep §6.3 (the authorization horizon), §9 (the authorization-staleness  *)
(* and leftover-access rows), §13.2 (topics pause, then close), §16.4      *)
(* (link checks and the presigned redirect).                               *)
(*                                                                         *)
(* Time is discrete real time `now`. Every clock reading on any node is    *)
(* within Eps of real time; nothing else is assumed (readings need not be  *)
(* monotone or consistent between nodes), which over-approximates bounded  *)
(* drift.                                                                  *)
(*                                                                         *)
(* Partitions: sys (keys), meta (policies), act (the actor's node),        *)
(* viewer (the viewer's node), chan (the channel membership edge that the  *)
(* read policy checks). Each has a leader whose heartbeat carries (commit  *)
(* index, leader-lease time = the leader's clock when it sent it).         *)
(*                                                                         *)
(* One request runs as the pair (actor g1, viewer u1) fixed by its act-for *)
(* key K (§6.3) and reads a channel. Every revocation kind denies it:      *)
(*   sys: revoke K (by u1, `shep key revoke`, or by the app's owner);      *)
(*   meta: deploy a policy denying agents; act: delete the agent;          *)
(*   viewer: delete u1 (a deleted viewer is `denied`, §6.3);               *)
(*   viewer_apponly: delete u1, where the read policy also has an app-only *)
(*     allow (`allow if app == slack`); it is the viewer partition, so     *)
(*     HorizonParts names it "viewer";                                     *)
(*   chan: remove u1 from the channel;                                     *)
(*   token: the external auth service stops issuing tokens to the request  *)
(*     (§6.3: Shep keeps no revocation list). Shep never sees it: a token  *)
(*     issued at iat = 0 is honored until its `exp` (TokLife), judged by a *)
(*     clock with an ExpSkew allowance; a client refreshes its token       *)
(*     (Refresh, any time before the revocation, exp = now + TokLife), a   *)
(*     link carries its minting token's exp and a mutation its read's.     *)
(*     The bound is then TokLife + ExpSkew + Eps, not B.                   *)
(*   split: delete u1, after u1's partition p split and its group moved to *)
(*     a child c. p's leader stays alive and keeps stamping heartbeats     *)
(*     whose index includes the split entry; the deletion commits at c.    *)
(*   restart: delete u1 on an RF = 1 WAL node (no external leader) that    *)
(*     restarted at fo and, being the only node, skips the lease wait.     *)
(*   sys: (superseded) revoke the Shep-issued act-for key K.               *)
(* (The superseded configs call the viewer partition "map": the agent's    *)
(* acts_for target under the mapping model.)                               *)
(* A behaviour commits at most one revocation, in its focus partition      *)
(* `phi` (chosen at Init). Every other partition's index stays 0 and its   *)
(* evidence is taken to be fresh. That is a sound reduction: every         *)
(* property below is a conjunction over partitions, each conjunct depends  *)
(* only on that partition's commits and evidence, and a second revocation  *)
(* or staler evidence elsewhere only turns grants into denials or          *)
(* `unavailable` and only moves deadlines earlier.                         *)
(*                                                                         *)
(* Front ends hold no modelled state. The network delays, drops, reorders  *)
(* and duplicates heartbeats arbitrarily and may cut a front end off a     *)
(* leader, so at any check a front end may hold any heartbeat sent before  *)
(* it, or none (then it answers `unavailable`, which changes no state).    *)
(* Its learner may lag arbitrarily, so S[p] is any index from the          *)
(* evidence's index up to the commit index. Two front ends, or any number, *)
(* are therefore covered: a link minted on f1 and checked on f2, or a 2PC  *)
(* coordinated by f1 while a topic runs on f2, are behaviours here.        *)
(* For each heartbeat send time only the latest possible stamp is kept     *)
(* (r + Eps, or the lease bound): a later stamp is accepted more often and *)
(* gives a later deadline, so it dominates.                                *)
(*                                                                         *)
(* Leader failover (at most Failovers, in phi): the old leader is cut off  *)
(* at fo and its lease ends at LE = fo + LeaseLen on its clock. It keeps   *)
(* heartbeating its old commit index; with HbLease it stamps only clock    *)
(* readings < LE. The new leader commits only once its clock reads >= LE.  *)
(*                                                                         *)
(* Operations: a read (which may mint a link); a mutation (read at S, then *)
(* a decide/send step, which is the proposal of a single-partition         *)
(* mutation or the decision of a 2PC after its prepares, then the append   *)
(* at the leader, which may be delayed in flight); a live topic stepping   *)
(* at the published frontier; a link check that redirects to a presigned   *)
(* GET; a download through that GET at the object store.                   *)
(*                                                                         *)
(* Knobs (design value / fix first):                                       *)
(*  AgeDrift      TRUE: evidence is fresh iff clock - stamp <= B - 2 Eps.  *)
(*  HbLease       TRUE: a deposed leader stamps no heartbeat past its lease*)
(*  HorizonParts  {"sys","meta","act","viewer"}: partitions with evidence. *)
(*  MatCheck      "version" (fix): the materializer refuses an actor-      *)
(*                sensitive read at a version more than B behind;          *)
(*                "applied" (design text): refuses while its applied index *)
(*                is more than B behind; "none".                           *)
(*  TopicGate     "horizon": a step needs S to contain the horizon; "none" *)
(*  DecideCheck   "fresh": the send/decision step gathers fresh evidence   *)
(*                and re-checks at S joined with it; "reuse": read-time.   *)
(*  InFlightDelay TRUE: a sent proposal/decision is appended later.        *)
(*  Deadline      TRUE (fix): it carries oldest stamp + B - 2Eps (or + B   *)
(*                without AgeDrift), and the leader appends it only while  *)
(*                its clock is at or before that.                          *)
(*  PresignMargin 2*Eps (fix): presigned expiry = clock + PL - margin.     *)
(*  ExpCap        TRUE (fix): a proposal or decision is appended only     *)
(*                while the leader's clock is at or before the token's     *)
(*                exp + ExpSkew; FALSE (design text): only the horizon     *)
(*                deadline applies.                                        *)
(*  SplitFix      TRUE (fix): evidence from a partition whose committed    *)
(*                log holds a split entry doesn't count for ranges past    *)
(*                it; the tail switches to the child and fails closed      *)
(*                until it has evidence there. FALSE: p's fresh evidence   *)
(*                counts.                                                  *)
(*  FollowerStamp "relay" (fix): a fetch from a follower carries the       *)
(*                leader's (commit index, leader-lease stamp) unchanged,   *)
(*                which is a delayed leader heartbeat; "own": the follower *)
(*                stamps its own clock on the commit index it last heard.  *)
(*  RestartZombie FALSE: after an RF = 1 restart the old incarnation is    *)
(*                gone; TRUE: it lives on (a hung process) and keeps       *)
(*                stamping its old index, with no lease to stop it.        *)
(*  ViewerDeleteDenies TRUE (§6.3): a request whose viewer is deleted is   *)
(*                `denied` before any policy runs (and an app-only allow,  *)
(*                written `... and not exists global viewer`, never holds  *)
(*                for a request with a viewer). FALSE (the superseded      *)
(*                semantics): the request has no viewer and the app-only   *)
(*                allow admits it, so in viewer_apponly the deletion never *)
(*                denies.                                                  *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS Eps, B, PL, LeaseLen, MaxT, FocusSet, Failovers, Ops,
          HorizonParts, AgeDrift, HbLease, MatCheck, TopicGate,
          DecideCheck, InFlightDelay, Deadline, PresignMargin,
          ViewerDeleteDenies, TokLife, ExpSkew, ExpCap, SplitFix, FollowerStamp,
          RestartZombie

NONE == -1
NoEv == -100          \* "no evidence for phi" as a send time
Bacc == IF AgeDrift THEN B - 2 * Eps ELSE B   \* largest accepted computed age
Min(a, b) == IF a <= b THEN a ELSE b

VARIABLES
  now,    \* real time
  phi,    \* the focus partition
  revT,   \* real commit time of phi's revocation, or NONE
  fo,     \* real time phi's old leader was cut off, or NONE
  mat,    \* chan materializer's applied index (phi = "chan" only)
  fr,     \* phi's component of the published frontier
  mut,    \* the mutation: [ph, c (deadline stamp), r (evidence send time),
          \*   ct (commit time)]
  top,    \* the topic subscription: none, open, paused, closed
  link,   \* exp of the token that minted a download link, or NONE
  rdr,    \* check time of the latest redirect, or NONE
  hzOK, rvOK, fcOK, mlOK, lkOK,  \* history flags, one per invariant
  tokExp, \* exp of the token the client currently holds (phi = "token")
  splitT  \* real time p's split entry committed, or NONE (phi = "split")

vars == <<now, phi, revT, fo, mat, fr, mut, top, link, rdr, hzOK, rvOK, fcOK, mlOK, lkOK,
          tokExp, splitT>>
flags == <<hzOK, rvOK, fcOK, mlOK, lkOK>>

\* phi's commit index at instant t, the start of tick t: commits in earlier
\* ticks. A heartbeat sent at t carries CiAt(t). A token revocation is the
\* auth service's, in no Shep partition.
CiAt(t) == IF phi /= "token" /\ revT /= NONE /\ revT < t THEN 1 ELSE 0
\* phi's commit index now (including a commit earlier in this tick)
Ci == IF phi /= "token" /\ revT /= NONE THEN 1 ELSE 0
\* a token with expiry e passes a check at real time t: some clock reading
\* (within Eps of t) is at or before e + ExpSkew
TokValid(t, e) == phi /= "token" \/ t - Eps <= e + ExpSkew
\* how long after a revocation outputs can still appear
RevBound == IF phi = "token" THEN TokLife + ExpSkew + Eps ELSE B
\* the deposed leader's commit index: what it committed before it was cut off
OldIdx == IF revT /= NONE /\ revT <= fo THEN 1 ELSE 0
LE == fo + LeaseLen
\* the partition phi's revocation is in
Part == IF phi \in {"viewer_apponly", "split", "restart"} THEN "viewer" ELSE phi
\* the revocation doesn't deny: the superseded semantics, where deleting
\* the viewer left an app-only allow to decide
NoDeny == phi = "viewer_apponly" /\ ~ViewerDeleteDenies

(************************* evidence ******************************************)

\* Heartbeats of phi's leaders sent at real times <= t: send time r, stamp c,
\* commit index idx. The leader in charge reports the true commit index
\* (heartbeats from the new leader before it is elected carry true information
\* too, which only helps). A deposed leader reports its index at fo.
\* An RF = 1 restart has no lease: the old incarnation is gone, or (zombie)
\* stamps on unbounded. A follower stamping its own clock (FollowerStamp =
\* "own") reports, at s, the index it last heard at r0 <= s.
HB(t) ==
  {[r |-> r, c |-> r + Eps, idx |-> CiAt(r)] : r \in 0..t}
  \cup (IF fo = NONE \/ (phi = "restart" /\ ~RestartZombie) THEN {}
        ELSE IF phi = "restart" THEN
          {[r |-> r, c |-> r + Eps, idx |-> OldIdx] : r \in fo..t}
        ELSE
        {[r |-> r, c |-> IF HbLease THEN Min(r + Eps, LE - 1) ELSE r + Eps,
          idx |-> OldIdx] :
            r \in {x \in fo..t : HbLease => x - Eps < LE}})
  \cup (IF FollowerStamp = "own"
        THEN UNION {{[r |-> r0, c |-> s + Eps, idx |-> CiAt(r0)] : r0 \in 0..s} : s \in 0..t}
        ELSE {})

\* accepted by a node whose clock reads at least t - Eps
AccOf(H, t) == {h \in H : t - Eps - h.c <= Bacc}
Acc(t) == AccOf(HB(t), t)

\* split: p's heartbeats count the split entry (idx 1 once it committed);
\* the child c exists after the split, and its index counts the deletion
HBp(t) == {[r |-> r, c |-> r + Eps, idx |-> IF splitT /= NONE /\ splitT < r THEN 1 ELSE 0]
           : r \in 0..t}
HBc(t) == IF splitT = NONE THEN {}
          ELSE {[r |-> r, c |-> r + Eps, idx |-> CiAt(r)] : r \in (splitT + 1)..t}
\* the evidence that counts for the principal's range, as (r, c, idx of
\* the deletion): with the fix, p's only before its split entry, then the
\* child's; without, p's evidence counts and its state lacks the deletion
EffEv(t) == AccOf(HBc(t), t) \cup
            (IF SplitFix THEN {h \in AccOf(HBp(t), t) : h.idx = 0}
             ELSE {[h EXCEPT !.idx = 0] : h \in AccOf(HBp(t), t)})
GrantEv(t) == {h \in Acc(t) : h.idx = 0}

\* fc: the evidence was sent within B in real time (ghost, for Inv_FailClosed)
Opt(h, t) == [fc |-> h.r >= t - B, c |-> h.c, r |-> h.r]
NoOpt(t) == [fc |-> FALSE, c |-> t + Eps, r |-> NoEv]

\* The ways an actor-sensitive check at t can grant (S[phi] = 0). A denial
\* (S[phi] = 1) or `unavailable` changes no state, so it isn't an action.
GrantOptsE(t, e) ==
  IF ~TokValid(t, e) THEN {}
  \* the deletion doesn't deny: a grant at any S the evidence allows
  ELSE IF NoDeny THEN {Opt(h, t) : h \in Acc(t)}
  \* a token revocation leaves every partition's state granting
  ELSE IF phi = "token" THEN {Opt(h, t) : h \in GrantEv(t)}
  ELSE IF phi = "split" THEN {Opt(h, t) : h \in {h \in EffEv(t) : h.idx = 0}}
  ELSE IF phi = "chan" THEN
     \* served by the materializer at its applied index (a point read, §9.4)
     IF mat /= 0 THEN {}
     ELSE IF MatCheck = "none" THEN {NoOpt(t)}
     ELSE {Opt(h, t) : h \in GrantEv(t)}
  ELSE IF Part \in HorizonParts THEN {Opt(h, t) : h \in GrantEv(t)}
  \* a partition outside the horizon: whatever the learner has, maybe 0
  ELSE {NoOpt(t)}
GrantOpts(t) == GrantOptsE(t, tokExp)

(************************* produced outputs **********************************)

\* An actor-sensitive output at real time t, granted at S with S[phi] = 0.
Produce(t, fc) ==
  /\ hzOK' = (hzOK /\ CiAt(t - B) = 0)            \* S[phi] >= ci(t - B)
  /\ rvOK' = (rvOK /\ ~(revT /= NONE /\ revT + RevBound < t))
  /\ fcOK' = (fcOK /\ fc)

\* the chan materializer serves an actor-sensitive read at t
MatServe(t) == mlOK' = (mlOK /\ (phi = "chan" => mat >= CiAt(t - B)))

Init ==
  /\ now = 0
  /\ phi \in FocusSet
  /\ revT = NONE /\ fo = NONE /\ mat = 0 /\ fr = 0
  /\ mut = [ph |-> "none", c |-> 0, r |-> 0, ct |-> 0, e |-> 0]
  /\ top = "none" /\ link = NONE /\ rdr = NONE
  /\ tokExp = TokLife
  /\ splitT = NONE
  /\ hzOK = TRUE /\ rvOK = TRUE /\ fcOK = TRUE /\ mlOK = TRUE /\ lkOK = TRUE

(************************* environment ***************************************)

Tick == now < MaxT /\ now' = now + 1
        /\ UNCHANGED <<phi, revT, fo, mat, fr, mut, top, link, rdr, flags>>

\* phi's revocation commits; after a failover, only once the new leader's
\* clock reads >= LE
\* (split: the deletion commits at the child, after the split; restart: an
\* RF = 1 node skips the lease wait)
Revoke == revT = NONE /\ ((fo /= NONE /\ phi /= "restart") => now + Eps >= LE)
          /\ (phi = "split" => splitT /= NONE /\ splitT < now)
          /\ revT' = now
          /\ UNCHANGED <<now, phi, fo, mat, fr, mut, top, link, rdr, flags>>

Failover == Failovers > 0 /\ phi \notin {"split", "token"} /\ fo = NONE /\ fo' = now
            /\ UNCHANGED <<now, phi, revT, mat, fr, mut, top, link, rdr, flags>>

MatApply == phi = "chan" /\ mat < Ci /\ mat' = Ci
            /\ UNCHANGED <<now, phi, revT, fo, fr, mut, top, link, rdr, flags>>

\* the meta leader publishes a frontier that includes phi's entry; for chan
\* it can't pass the materializer
FrontierAdvance == fr < (IF phi = "chan" THEN mat ELSE Ci) /\ fr' = fr + 1
            /\ UNCHANGED <<now, phi, revT, fo, mat, mut, top, link, rdr, flags>>

(************************* reads and links ***********************************)

Read == "read" \in Ops /\ \E o \in GrantOpts(now) :
          /\ Produce(now, o.fc) /\ MatServe(now)
          /\ link' = IF "link" \in Ops THEN tokExp ELSE NONE
          /\ UNCHANGED <<now, phi, revT, fo, mat, fr, mut, top, rdr, lkOK>>

\* §16.4: verify, check the minting key, the actor and can_op at S, redirect
LinkCheck == link /= NONE /\ \E o \in GrantOptsE(now, link) :
          /\ Produce(now, o.fc) /\ MatServe(now)
          /\ lkOK' = (lkOK /\ ~(phi = "sys" /\ revT /= NONE /\ revT + B < now))
          /\ rdr' = now
          /\ UNCHANGED <<now, phi, revT, fo, mat, fr, mut, top, link>>

\* the object store accepts the presigned GET while its clock reading is at
\* or before the expiry the front end stamped (its clock + PL - margin)
Download == rdr /= NONE /\ now - Eps <= rdr + Eps + PL - PresignMargin
          /\ rvOK' = (rvOK /\ ((revT /= NONE /\ revT + RevBound < now) => now - rdr <= PL))
          /\ UNCHANGED <<now, phi, revT, fo, mat, fr, mut, top, link, rdr,
                         hzOK, fcOK, mlOK, lkOK>>

(************************* mutations *****************************************)

MutRead == "mut" \in Ops /\ mut.ph = "none" /\ \E o \in GrantOpts(now) :
          /\ mut' = [ph |-> "read", c |-> o.c, r |-> o.r, ct |-> 0, e |-> tokExp]
          /\ MatServe(now)
          /\ UNCHANGED <<now, phi, revT, fo, mat, fr, top, link, rdr,
                         hzOK, rvOK, fcOK, lkOK>>

\* the leader appends (commits) the proposal or decision now. A chan read is
\* a write-policy read, validated at commit, so its effective version is
\* the current one.
Append(c, r) ==
  /\ Deadline => now - Eps <= c + Bacc
  /\ ExpCap => TokValid(now, mut.e)
  /\ phi = "chan" => Ci = 0
  /\ Produce(now, phi = "chan" \/ r >= now - B)
  /\ mut' = [ph |-> "committed", c |-> c, r |-> r, ct |-> now, e |-> mut.e]
  /\ mlOK' = mlOK /\ lkOK' = lkOK

\* send the proposal (single partition) or the decision (2PC)
Decide == mut.ph = "read" /\
  /\ IF DecideCheck = "fresh"
     THEN \E o \in GrantOptsE(now, mut.e) :
            IF InFlightDelay
            THEN mut' = [ph |-> "sent", c |-> o.c, r |-> o.r, ct |-> 0, e |-> mut.e]
                 /\ UNCHANGED flags
            ELSE Append(o.c, o.r)
     ELSE IF InFlightDelay
          THEN mut' = [mut EXCEPT !.ph = "sent"] /\ UNCHANGED flags
          ELSE Append(mut.c, mut.r)
  /\ UNCHANGED <<now, phi, revT, fo, mat, fr, top, link, rdr>>

Deliver == mut.ph = "sent" /\ Append(mut.c, mut.r)
           /\ UNCHANGED <<now, phi, revT, fo, mat, fr, top, link, rdr>>

MutAbort == mut.ph \in {"read", "sent"} /\ mut' = [mut EXCEPT !.ph = "aborted"]
           /\ UNCHANGED <<now, phi, revT, fo, mat, fr, top, link, rdr, flags>>

(************************* live topic ****************************************)

Subscribe == "topic" \in Ops /\ top = "none" /\ top' = "open"
           /\ UNCHANGED <<now, phi, revT, fo, mat, fr, mut, link, rdr, flags>>

\* fc values of the gate checks that let a step at S = the frontier proceed
TopicGateOpts(t) ==
  IF ~TokValid(t, tokExp) THEN {}
  ELSE IF phi = "token" THEN {h.r >= t - B : h \in {h \in Acc(t) : h.idx <= fr}}
  ELSE IF phi = "split" THEN {h.r >= t - B : h \in {h \in EffEv(t) : h.idx <= fr}}
  ELSE IF phi = "chan" THEN
     CASE MatCheck = "none"    -> {FALSE}
       [] MatCheck = "applied" -> {h.r >= t - B : h \in {h \in Acc(t) : h.idx <= mat}}
       [] MatCheck = "version" -> {h.r >= t - B : h \in {h \in Acc(t) : h.idx <= fr}}
  ELSE IF Part \in HorizonParts /\ TopicGate = "horizon"
       THEN {h.r >= t - B : h \in {h \in Acc(t) : h.idx <= fr}}
  ELSE {FALSE}

\* a step at S = frontier: push data if permitted at S, else close (denied)
TopicStep == top \in {"open", "paused"} /\ \E fc \in TopicGateOpts(now) :
          /\ IF fr = 0 \/ NoDeny
             THEN top' = "open" /\ Produce(now, fc) /\ MatServe(now) /\ lkOK' = lkOK
             ELSE top' = "closed" /\ UNCHANGED flags
          /\ UNCHANGED <<now, phi, revT, fo, mat, fr, mut, link, rdr>>

\* no S containing the horizon: pause; if it doesn't catch up, close
TopicPause == top = "open" /\ top' = "paused"
           /\ UNCHANGED <<now, phi, revT, fo, mat, fr, mut, link, rdr, flags>>
TopicClose == top = "paused" /\ top' = "closed"
           /\ UNCHANGED <<now, phi, revT, fo, mat, fr, mut, link, rdr, flags>>

\* the client gets a fresh token (for a stream, its answer to `reauth`);
\* the auth service issues one only before the revocation
Refresh == phi = "token" /\ revT = NONE /\ tokExp < now + TokLife
           /\ tokExp' = now + TokLife
           /\ UNCHANGED <<now, phi, revT, fo, mat, fr, mut, top, link, rdr, flags>>

\* p's split entry commits (phi = "split")
Split == phi = "split" /\ splitT = NONE /\ splitT' = now
         /\ UNCHANGED <<now, phi, revT, fo, mat, fr, mut, top, link, rdr, flags>>

Next ==
  \/ /\ \/ Tick \/ Revoke \/ Failover \/ MatApply \/ FrontierAdvance
        \/ Read \/ LinkCheck \/ Download
        \/ MutRead \/ Decide \/ Deliver \/ MutAbort
        \/ Subscribe \/ TopicStep \/ TopicPause \/ TopicClose
     /\ UNCHANGED <<tokExp, splitT>>
  \/ Refresh /\ UNCHANGED splitT
  \/ Split /\ UNCHANGED tokExp

Spec == Init /\ [][Next]_vars

(************************* invariants ****************************************)

\* 1. Every actor-sensitive output (response, commit of a proposal or 2PC
\*    decision, topic push, link redirect) at real time t is at an S whose
\*    phi component is at least phi's commit index at t - B.
Inv_Horizon == hzOK
\* 2. A revocation committed at tr affects every output after tr + B; the
\*    only residual is a download through a redirect within PL of its check.
\*    For a token revocation (outside Shep) the bound is TokLife + ExpSkew
\*    + Eps instead of B.
Inv_Revoke == rvOK
\* 3. Data is produced only with evidence for each relevant partition (the
\*    horizon partitions, and chan through its materializer) sent within B
\*    of the output in real time; otherwise `unavailable`, pause or close.
Inv_FailClosed == fcOK
\* 4. The chan materializer serves no actor-sensitive read while its applied
\*    index is behind chan's commit index as of B ago.
Inv_MaterializerLag == mlOK
\* 5. A link minted by a revoked key stops passing its check within B.
Inv_LinkRevoked == lkOK

\* Sanity, each expected to be violated under AuthHorizon.cfg to show the
\* passing config isn't vacuous. State predicates over enabling conditions,
\* since a granted read, step or download that changes nothing is a
\* stuttering step that [][A]_vars wouldn't see.
\*  - a residual download is possible after tr + B
Sanity_NoResidual == ~(rdr /= NONE /\ now - Eps <= rdr + Eps + PL - PresignMargin
                       /\ revT /= NONE /\ revT + RevBound < now)
\*  - a mutation commits exactly at tr + B
Sanity_NoBoundaryCommit == ~(mut.ph = "committed" /\ revT /= NONE /\ mut.ct = revT + RevBound)
\*  - a topic can push exactly at tr + B
Sanity_NoBoundaryPush == ~(top \in {"open", "paused"} /\ fr = 0 /\ revT /= NONE
                           /\ now = revT + RevBound /\ TopicGateOpts(now) /= {})
\*  - after a failover, a check can grant after the new leader's commit
Sanity_NoGrantAfterFailover == ~(fo /= NONE /\ revT /= NONE /\ revT > fo
                                 /\ now > revT /\ GrantOpts(now) /= {})

TypeOK ==
  /\ now \in 0..MaxT /\ revT \in {NONE} \cup 0..MaxT /\ fo \in {NONE} \cup 0..MaxT
  /\ mat \in 0..1 /\ fr \in 0..1 /\ mut.ph \in {"none", "read", "sent", "committed", "aborted"}
  /\ top \in {"none", "open", "paused", "closed"}
=============================================================================
