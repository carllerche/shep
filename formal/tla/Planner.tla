------------------------------ MODULE Planner ------------------------------
(***************************************************************************)
(* Shep design §7.6 (consistency classes), §4.1.2 (#[frozen] and          *)
(* #[permanent]), §8 and §10.3 (leader-resolved field operations), §6.3   *)
(* (the read horizon split by revocation sensitivity), on TWO partitions.  *)
(*                                                                         *)
(* Each partition has one leader and a log. Entries are appended (and      *)
(* enter the validation table and the leader's last-value map at once)     *)
(* and committed later, one at a time, so appends are pipelined. No        *)
(* failover. A version is a vector <<i1, i2>> of committed prefixes; a     *)
(* front end reads at any causally closed committed version (a lagging     *)
(* materializer), so S can be arbitrarily stale.                           *)
(*                                                                         *)
(* Schema (keys; P1 holds Commit c, P2 holds everything else):             *)
(*   Commit c   #[permanent] (unless the registry change weakens it)       *)
(*     cLife    absent | live | deleted                                    *)
(*     cOwner   #[frozen] One<User> edge (none | u1 | u2), set at create   *)
(*   BuildResult r (rLife), and q (qLife): absent | live                   *)
(*   Item i     mutable lifecycle (soft delete)                            *)
(*     iLife    live | deleted                                             *)
(*     owner    #[frozen] edge, always u1 (no writer, so not a key)        *)
(*     iStock   mutable Int, #[check(self >= 0)], op-eligible; NoVal when  *)
(*              the optional field is removed                              *)
(*     iWatch   mutable edge i -> u1 (watchers)                            *)
(*                                                                         *)
(* Requests (each one client call, re-executed on contended / too_old /    *)
(* stale_registry, at most MaxExec times):                                 *)
(*   createC  : require c absent; create c with owner u1          [P1]     *)
(*   accept   : require exists c && c.owner == u1; create r       [P2]     *)
(*              Stable (permanent lifecycle, frozen edge), so SKIPPED:     *)
(*              no read-only participant in P1. A false at S re-executes   *)
(*              the whole mutation once at S' = the leaders' commit index. *)
(*   acceptI  : require exists i; create q                        [P2]     *)
(*              i's lifecycle is mutable, so validated                     *)
(*   purge    : require c live && no r; delete c   [P1 + P2 read-only]     *)
(*              (deployable only where Commit isn't #[permanent])          *)
(*   reassign : set c.owner := u2                                 [P1]     *)
(*              (deployable only where cOwner isn't #[frozen])             *)
(*   delI     : soft-delete i                                     [P2]     *)
(*   unwatch  : unlink i.watchers u1                              [P2]     *)
(*   dec1/2   : i.stock.decrement(), leader-resolved, bound >= 0  [P2]     *)
(*   bump     : require i.stock > 0; i.stock.increment()          [P2]     *)
(*              the guard reads the op's target, so it compiles to a       *)
(*              validated read + plain write (OpReadRule)                  *)
(*   clear    : remove the optional field i.stock (a plain write)  [P2]    *)
(*                                                                         *)
(* A two-phase commit with a read-only participant is modeled as one       *)
(* atomic step that validates at both leaders and appends at both (the     *)
(* read-only participant gets an entry with no writes, so its position in  *)
(* that log constrains the serial order).                                  *)
(*                                                                         *)
(* Registry (one discipline change, Change):                               *)
(*   "none"       Commit is #[permanent], cOwner #[frozen], forever.       *)
(*   "weaken"     drop #[permanent] from Commit. reg 0 -> 1: the re-       *)
(*                planned sets activate and a barrier (rv 1) goes to every *)
(*                partition where an affected operation writes or is home  *)
(*                (accept: P2). reg 1 -> 2, only after every one of those  *)
(*                has committed it (TwoStep): the weaker rule is in force, *)
(*                and its barrier (rv 2) goes to P1.                       *)
(*   "strengthen" add #[permanent] to Commit and #[frozen] to cOwner.      *)
(*                reg 0 -> 1, barrier rv 1 to P1 (the enforcement barrier) *)
(*                and P2. A plan relies on the new disciplines for P1's    *)
(*                facts only if S contains P1's barrier (RelyRule).        *)
(* Disciplines are enforced by the FRONT END under its registry version    *)
(* pv (any version <= reg: caches are stale), and a leader rejects a       *)
(* proposal whose registry dot is older than its latest barrier            *)
(* (BarrierCheck).                                                         *)
(*                                                                         *)
(* Leader-resolved ops (§8): a proposal carries op records {key, op,       *)
(* base_value, base_stamp, bound}; every plain write to an op-eligible key *)
(* carries {key, new_value}, a removal {key, absent}. The leader keeps the *)
(* last value of each op-eligible key written within the window, appended  *)
(* entries included (the log above the floor). An op resolves against the *)
(* newest in-window write to its key that is not in S, else base_value;    *)
(* too_old iff S's prefix is below the window floor; the bound is checked  *)
(* on the resolved value. The op's node can be deleted, so the proposal    *)
(* reads the node's lifecycle key (validated). The entry's context is      *)
(* S ∪ {prev} ∪ ctx(prev), where prev is the key's last appended writer;   *)
(* when prev is still pending (no index yet) the leader instead sets a     *)
(* self-prefix flag that expands to {p: i-1} (chosen nondeterministically, *)
(* since the model always has the index).                                  *)
(*                                                                         *)
(* Reads (invariant ReadHorizon): read decisions on c (allow if            *)
(* node.owner == viewer), i (the same), and i under `allow if viewer in    *)
(* node.watchers`. A decision that isn't RevocationSensitive is served at  *)
(* ANY closed committed version (a materializer arbitrarily far behind);  *)
(* a sensitive one only at the fresh committed vector (the horizon, with   *)
(* B abstracted to 0). Read plans use the current registry: the registry   *)
(* is in the actor part of the horizon (§6.3).                             *)
(*                                                                         *)
(* Constants select the rule implemented; see the cfgs.                   *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, Sequences, TLC

CONSTANTS
  Reqs,          \* request names (strings, see ProgOf)
  StockInit,     \* initial i.stock
  CLive0,        \* TRUE: c exists (owner u1) initially; FALSE: absent
  MaxExec,       \* executions per request
  MaxFloor,      \* bound on advances of each window floor
  Change,        \* "none" | "weaken" | "strengthen"
  TwoStep,       \* TRUE: the weakening is in force only after every affected
                 \*   partition committed the re-plan barrier (design)
  BarrierCheck,  \* TRUE: a leader rejects a registry dot older than its
                 \*   latest barrier (design)
  RelyRule,      \* TRUE: rely on a discipline in Q only if S has Q's
                 \*   enforcement barrier (design)
  LifecycleRule, \* TRUE: only a #[permanent] lifecycle is a stable input
                 \*   (design); FALSE: `exists` is skipped as "monotone"
  VisibleRule,   \* TRUE: RevocationSensitive is judged over visible()
                 \*   (design); FALSE: over the policy's rules only
  Resolve,       \* "window": newest in-window write not in S, else base
                 \*   (design); "base": always base_value (ignores the
                 \*   pipeline); "noremoval": removals not recorded in the map
  PrevMode,      \* "prev": ctx gets prev and ctx(prev) or the self-prefix
                 \*   flag (design); "none": ctx = S
  OpReadRule,    \* TRUE: an op on a field another consumer reads compiles to
                 \*   a validated write (design)
  OpLifeRead,    \* TRUE: an op's proposal reads (validates) its node's
                 \*   lifecycle key, so a concurrent delete is contended (design)
  RetryFresh,    \* TRUE: a stale false re-executes at the leaders' commit
                 \*   indexes (design); FALSE: at any materializer version
  RetryWhole     \* TRUE: the whole mutation re-executes at S' (S' becomes S
                 \*   and joins the context) (design); FALSE: only the guard
                 \*   is re-evaluated at S'; reads and context stay at S

ASSUME Change \in {"none", "weaken", "strengthen"}
ASSUME Resolve \in {"window", "base", "noremoval"}
ASSUME PrevMode \in {"prev", "none"}

ProgOf(t) == IF t \in {"dec1", "dec2"} THEN "dec" ELSE t

P1 == 1
P2 == 2
Parts == {P1, P2}
Zero == <<0, 0>>
NoVal == -100        \* a removed optional field
Bad == -200          \* an op applied to a removed field (BoundNeverCrossed)
Keys == {"cLife", "cOwner", "rLife", "qLife", "iLife", "iStock", "iWatch"}
Part(k) == IF k \in {"cLife", "cOwner"} THEN P1 ELSE P2

Init0 == [k \in Keys |->
           CASE k = "cLife"  -> IF CLive0 THEN "live" ELSE "absent"
             [] k = "cOwner" -> IF CLive0 THEN "u1" ELSE "none"
             [] k = "rLife"  -> "absent"
             [] k = "qLife"  -> "absent"
             [] k = "iLife"  -> "live"
             [] k = "iStock" -> StockInit
             [] k = "iWatch" -> TRUE]

Max(a, b) == IF a > b THEN a ELSE b
MaxSet(X) == CHOOSE x \in X : \A y \in X : y <= x
Join(a, b) == <<Max(a[1], b[1]), Max(a[2], b[2])>>
Leq(a, b) == a[1] <= b[1] /\ a[2] <= b[2]
Unit(p, j) == IF p = P1 THEN <<j, 0>> ELSE <<0, j>>
EmptyW == [k \in {} |-> 0]
F(k, v) == [x \in {k} |-> v]

MaxReg == CASE Change = "weaken" -> 2 [] Change = "strengthen" -> 1 [] OTHER -> 0

(************************* registry and disciplines ***********************)

\* what front ends enforce under registry version r
EnforcedPerm(r)   == CASE Change = "none" -> TRUE
                       [] Change = "weaken" -> r < 2
                       [] OTHER -> r >= 1
EnforcedFrozen(r) == IF Change = "strengthen" THEN r >= 1 ELSE TRUE
\* what plans under registry version r rely on (the weakening's re-planned
\* sets, r = 1, already plan for the weaker rule)
PlanPerm(r)   == CASE Change = "none" -> TRUE
                   [] Change = "weaken" -> r = 0
                   [] OTHER -> r >= 1
PlanFrozen(r) == EnforcedFrozen(r)

\* partitions where an affected operation writes or is the home
BarrierParts1 == CASE Change = "weaken" -> {P2}
                   [] Change = "strengthen" -> {P1, P2}
                   [] OTHER -> {}

VARIABLES
  log,     \* per partition: sequence of entries
  commit,  \* <<commit index of P1, of P2>>
  floor,   \* per partition: the window floor (table covers idx > floor)
  reg,     \* the registry (meta partition) version
  st,      \* per request: "idle" | "prop" | "done" | "fail"
  n,       \* per request: executions so far
  prop,    \* per request: the proposal in flight
  why,     \* per request: why it failed
  failAt   \* per request: the leaders' commit vector when it failed

vars == <<log, commit, floor, reg, st, n, prop, why, failAt>>

Fresh == commit

BarRvs(p, j) == {log[p][i].rv : i \in {x \in 1..(j - 1) : log[p][x].kind = "bar"}}
BarRv(p, j) == IF BarRvs(p, j) = {} THEN 0 ELSE MaxSet(BarRvs(p, j))   \* in force before j
LatestBar(p) == BarRv(p, Len(log[p]) + 1)
HasBar(p, r) == \E i \in 1..Len(log[p]) : log[p][i].kind = "bar" /\ log[p][i].rv = r
BarIdx(p, r) == CHOOSE i \in 1..Len(log[p]) : log[p][i].kind = "bar" /\ log[p][i].rv = r
Acked(p, r) == \E i \in 1..commit[p] : log[p][i].kind = "bar" /\ log[p][i].rv = r

\* a plan may rely on a discipline for P1's facts only if V contains P1's
\* enforcement barrier (for "none"/"weaken" it is enforced from the start)
RelyOK(V) == ~ RelyRule \/ Change # "strengthen" \/ (HasBar(P1, 1) /\ BarIdx(P1, 1) <= V[P1])

\* a delta W (key -> <<old, new>>) breaks a discipline in force at r
BreaksAt(W, r) ==
  \/ /\ "cOwner" \in DOMAIN W /\ EnforcedFrozen(r)
     /\ ~ ("cLife" \in DOMAIN W /\ W["cLife"][1] = "absent")   \* not the creating entry
  \/ /\ "cLife" \in DOMAIN W /\ W["cLife"][1] = "live" /\ W["cLife"][2] = "deleted"
     /\ EnforcedPerm(r)

(*************************** versions ************************************)

ValLog(p, k, upto) ==
  LET js == {j \in 1..upto : k \in DOMAIN log[p][j].W}
  IN IF js = {} THEN Init0[k] ELSE log[p][MaxSet(js)].W[k][2]
Val(V, k) == ValLog(Part(k), k, V[Part(k)])
StateAt(V) == [k \in Keys |-> Val(V, k)]

ClosedIn(V) == \A p \in Parts : \A j \in 1..V[p] : Leq(log[p][j].ctx, V)
\* what front ends and materializers can read at: closed and committed
ClosedVersions == {V \in {<<a, b>> : a \in 0..commit[1], b \in 0..commit[2]} : ClosedIn(V)}
\* every closed version over the appended logs (for the context rule)
AllClosed == {V \in {<<a, b>> : a \in 0..Len(log[1]), b \in 0..Len(log[2])} : ClosedIn(V)}

(*************************** programs ************************************)

\* what each guard reads (implicit lifecycle reads included, §7.6 item 3)
GuardKeys(g) ==
  CASE g = "createC"  -> {"cLife"}
    [] g = "accept"   -> {"cLife", "cOwner"}
    [] g = "acceptI"  -> {"iLife"}
    [] g = "purge"    -> {"cLife", "rLife"}
    [] g = "reassign" -> {"cLife"}
    [] g = "delI"     -> {"iLife"}
    [] g = "unwatch"  -> {"iLife", "iWatch"}
    [] g = "dec"      -> {"iLife"}            \* the op's node lifecycle
    [] g = "bump"     -> {"iLife", "iStock"}
    [] g = "clear"    -> {"iLife"}
    [] OTHER          -> {}                   \* barrier entries

GuardOn(g, s) ==
  CASE g = "createC"  -> s["cLife"] = "absent"
    [] g = "accept"   -> s["cLife"] = "live" /\ s["cOwner"] = "u1"
    [] g = "acceptI"  -> s["iLife"] = "live"
    [] g = "purge"    -> s["cLife"] = "live" /\ s["rLife"] = "absent"
    [] g = "reassign" -> s["cLife"] = "live"
    [] g = "delI"     -> s["iLife"] = "live"
    [] g = "unwatch"  -> s["iLife"] = "live" /\ s["iWatch"]
    [] g = "dec"      -> s["iLife"] = "live"
    [] g = "bump"     -> s["iLife"] = "live" /\ s["iStock"] # NoVal /\ s["iStock"] > 0
    [] g = "clear"    -> s["iLife"] = "live"

OpD(g) == IF g = "dec" THEN -1 ELSE 1

\* the serial effect: key -> new value (guard assumed true)
Effect(g, s) ==
  CASE g = "createC"  -> [x \in {"cLife", "cOwner"} |-> IF x = "cLife" THEN "live" ELSE "u1"]
    [] g = "accept"   -> F("rLife", "live")
    [] g = "acceptI"  -> F("qLife", "live")
    [] g = "purge"    -> F("cLife", "deleted")
    [] g = "reassign" -> F("cOwner", "u2")
    [] g = "delI"     -> F("iLife", "deleted")
    [] g = "unwatch"  -> F("iWatch", FALSE)
    [] g = "dec"      -> F("iStock", s["iStock"] - 1)
    [] g = "bump"     -> F("iStock", s["iStock"] + 1)
    [] g = "clear"    -> F("iStock", NoVal)

\* the outcome a mutation has at a serial position: guard and bound
SerialOK(g, s) ==
  /\ GuardOn(g, s)
  /\ g \in {"dec", "bump"} => (s["iStock"] # NoVal /\ s["iStock"] + OpD(g) >= 0)

\* keys resolved at the leader
OpsOf(g) == CASE g = "dec"  -> {"iStock"}
              [] g = "bump" -> IF OpReadRule THEN {} ELSE {"iStock"}
              [] OTHER      -> {}
\* other consumers in the same mutation that read a key
ConsumerReads(g) == GuardKeys(g)

\* deploy-time check: an operation that breaks a discipline isn't in the set
InOpSet(g, r) == CASE g = "purge"    -> ~ EnforcedPerm(r)
                   [] g = "reassign" -> ~ EnforcedFrozen(r)
                   [] OTHER -> TRUE

\* the planner's classification of a guard: skipped (Stable) or validated
SkipGuard(g, r, V) ==
  CASE g = "accept"  -> PlanPerm(r) /\ PlanFrozen(r) /\ RelyOK(V)
    [] g = "acceptI" -> ~ LifecycleRule
    [] OTHER -> FALSE

(*************************** actions *************************************)

Init ==
  /\ log = <<<<>>, <<>>>>
  /\ commit = Zero
  /\ floor = Zero
  /\ reg = 0
  /\ st = [t \in Reqs |-> "idle"]
  /\ n = [t \in Reqs |-> 0]
  /\ prop = [t \in Reqs |-> [none |-> TRUE]]
  /\ why = [t \in Reqs |-> ""]
  /\ failAt = [t \in Reqs |-> Zero]

Fail(t, w) ==
  /\ st' = [st EXCEPT ![t] = "fail"]
  /\ why' = [why EXCEPT ![t] = w]
  /\ failAt' = [failAt EXCEPT ![t] = Fresh]
  /\ UNCHANGED prop

\* stage the mutation at read version S under registry r
Stage(t, r, S, skip) ==
  LET g == ProgOf(t)
      s == StateAt(S)
      eff == Effect(g, s)
      ops == OpsOf(g)
      W == [k \in DOMAIN eff \ ops |-> <<s[k], eff[k]>>]
      R == (((IF skip THEN {} ELSE GuardKeys(g)) \cup DOMAIN W) \ ops)
             \ (IF ~ OpLifeRead /\ ops # {} THEN {"iLife"} ELSE {})
  IN IF BreaksAt(W, r)                          \* front-end enforcement
     THEN Fail(t, "discipline")
     ELSE /\ st' = [st EXCEPT ![t] = "prop"]
          /\ prop' = [prop EXCEPT ![t] =
               [g |-> g, pv |-> r, S |-> S, skip |-> skip, R |-> R, W |-> W,
                ops |-> ops, base |-> [k \in ops |-> s[k]],
                parts |-> {Part(k) : k \in R \cup DOMAIN W \cup ops}]]
          /\ UNCHANGED <<why, failAt>>

Retry(V) == IF RetryFresh THEN {Fresh} ELSE {X \in ClosedVersions : Leq(V, X)}

Exec(t) ==
  /\ st[t] = "idle" /\ n[t] < MaxExec
  /\ n' = [n EXCEPT ![t] = @ + 1]
  /\ \E V \in ClosedVersions, r \in 0..reg :
       LET g == ProgOf(t)
           skip == SkipGuard(g, r, V)
       IN /\ InOpSet(g, r)
          /\ IF GuardOn(g, StateAt(V)) THEN Stage(t, r, V, skip)
             ELSE IF skip
             THEN \* a stale false of a Stable guard: re-execute once at S'
                  \E V2 \in Retry(V) :
                    IF GuardOn(g, StateAt(V2))
                    THEN IF RetryWhole THEN Stage(t, r, V2, SkipGuard(g, r, V2))
                         ELSE Stage(t, r, V, skip)
                    ELSE Fail(t, "missing")
             ELSE Fail(t, "require")
  /\ UNCHANGED <<log, commit, floor, reg>>

\* the leader's last-value map: newest in-window write to k not in S
InMap(v) == Resolve # "noremoval" \/ v # NoVal
Resolved(k, pr) ==
  LET p == Part(k)
      js == {j \in (pr.S[p] + 1)..Len(log[p]) :
               k \in DOMAIN log[p][j].W /\ InMap(log[p][j].W[k][2])}
  IN IF Resolve = "base" \/ js = {} THEN pr.base[k] ELSE log[p][MaxSet(js)].W[k][2]

LastWriter(p, k) == {j \in 1..Len(log[p]) : k \in DOMAIN log[p][j].W}
PrevCtx(p, k) == LET js == LastWriter(p, k) IN
                 IF js = {} THEN Zero
                 ELSE Join(Unit(p, MaxSet(js)), log[p][MaxSet(js)].ctx)

Validate(t) ==
  /\ st[t] = "prop"
  /\ LET pr == prop[t]
         tooOld == \E p \in pr.parts : pr.S[p] < floor[p]
         stale == BarrierCheck /\ \E p \in pr.parts : pr.pv < LatestBar(p)
         conflict == \E p \in pr.parts : \E j \in (pr.S[p] + 1)..Len(log[p]) :
                        \E k \in DOMAIN log[p][j].W : k \in pr.R
         cur == [k \in pr.ops |-> Resolved(k, pr)]
         nv == [k \in pr.ops |-> IF cur[k] = NoVal THEN NoVal ELSE cur[k] + OpD(pr.g)]
         bound == \E k \in pr.ops : cur[k] = NoVal \/ nv[k] < 0
         Wf == pr.W @@ [k \in pr.ops |-> <<cur[k], nv[k]>>]
         hasPrev == pr.ops # {} /\ LastWriter(P2, "iStock") # {}
     IN IF tooOld \/ stale \/ conflict
        THEN /\ st' = [st EXCEPT ![t] = "idle"]
             /\ UNCHANGED <<log, why, failAt, prop>>
        ELSE IF bound
        THEN /\ Fail(t, "invalid") /\ UNCHANGED log
        ELSE \E pend \in (IF hasPrev /\ PrevMode = "prev" THEN BOOLEAN ELSE {FALSE}) :
             LET ctx == IF pr.ops = {} \/ PrevMode = "none" THEN pr.S
                        ELSE IF pend THEN Join(pr.S, Unit(P2, Len(log[P2])))   \* {p: i-1}
                        ELSE Join(pr.S, PrevCtx(P2, "iStock"))
                 Ent(p) == [kind |-> IF \E k \in DOMAIN Wf : Part(k) = p THEN "tx" ELSE "ro",
                            tx |-> <<t, n[t]>>, g |-> pr.g,
                            W |-> [k \in {x \in DOMAIN Wf : Part(x) = p} |-> Wf[k]],
                            out |-> [k \in DOMAIN Wf |-> Wf[k][2]],
                            S |-> pr.S, ctx |-> ctx, pv |-> pr.pv, skip |-> pr.skip,
                            ops |-> pr.ops, selfpre |-> pend, rv |-> 0]
             IN /\ log' = [p \in Parts |-> IF p \in pr.parts THEN Append(log[p], Ent(p))
                                           ELSE log[p]]
                /\ st' = [st EXCEPT ![t] = "done"]
                /\ UNCHANGED <<why, failAt, prop>>
  /\ UNCHANGED <<commit, floor, reg, n>>

Commit(p) ==
  /\ commit[p] < Len(log[p])
  /\ commit' = [commit EXCEPT ![p] = @ + 1]
  /\ UNCHANGED <<log, floor, reg, st, n, prop, why, failAt>>

Advance(p) ==
  /\ floor[p] < commit[p] /\ floor[p] < MaxFloor
  /\ floor' = [floor EXCEPT ![p] = @ + 1]
  /\ UNCHANGED <<log, commit, reg, st, n, prop, why, failAt>>

Barrier(r) == [kind |-> "bar", tx |-> <<"-", 0>>, g |-> "-", W |-> EmptyW, out |-> EmptyW,
               S |-> Zero, ctx |-> Zero, pv |-> 0, skip |-> FALSE, ops |-> {},
               selfpre |-> FALSE, rv |-> r]

\* step 1: the re-planned sets activate
Activate ==
  /\ Change # "none" /\ reg = 0
  /\ reg' = 1
  /\ UNCHANGED <<log, commit, floor, st, n, prop, why, failAt>>

\* step 1: the barrier reaches each affected partition
SendBarrier(p) ==
  /\ reg >= 1 /\ p \in BarrierParts1 /\ ~ HasBar(p, 1)
  /\ log' = [log EXCEPT ![p] = Append(@, Barrier(1))]
  /\ UNCHANGED <<commit, floor, reg, st, n, prop, why, failAt>>

\* step 2 of a weakening: the weaker rule is in force (registry change plus
\* its barrier in P1, the partition of the facts it lets change)
Enforce ==
  /\ Change = "weaken" /\ reg = 1
  /\ TwoStep => \A p \in BarrierParts1 : Acked(p, 1)
  /\ reg' = 2
  /\ log' = [log EXCEPT ![P1] = Append(@, Barrier(2))]
  /\ UNCHANGED <<commit, floor, st, n, prop, why, failAt>>

Next ==
  \/ \E t \in Reqs : Exec(t) \/ Validate(t)
  \/ \E p \in Parts : Commit(p) \/ Advance(p) \/ SendBarrier(p)
  \/ Activate
  \/ Enforce

Spec == Init /\ [][Next]_vars

(*************************** invariants **********************************)

TypeOK ==
  /\ commit \in (0..10) \X (0..10)
  /\ reg \in 0..MaxReg
  /\ st \in [Reqs -> {"idle", "prop", "done", "fail"}]

Entries(p) == {log[p][j] : j \in 1..Len(log[p])}
TxIds == {e.tx : e \in {x \in Entries(1) \cup Entries(2) : x.kind \in {"tx", "ro"}}}
TxCommitted(x) == \A p \in Parts : \A j \in 1..Len(log[p]) : log[p][j].tx = x => j <= commit[p]
CTx == {x \in TxIds : TxCommitted(x)}
TxE(x) == CHOOSE e \in Entries(1) \cup Entries(2) : e.tx = x
Before(a, b) == \E p \in Parts : \E ja, jb \in 1..Len(log[p]) :
                  log[p][ja].tx = a /\ log[p][jb].tx = b /\ ja < jb
Apply(s, out) == [k \in Keys |-> IF k \in DOMAIN out THEN out[k] ELSE s[k]]

\* 1. Outcome-serializability with skips: some order of the committed
\* mutations, consistent with each partition's log, in which each one's
\* require/bound outcome (it committed, so true) and its writes equal what
\* it would get at that position.
RECURSIVE SerFrom(_, _)
SerFrom(todo, s) ==
  \/ todo = {}
  \/ \E x \in todo :
       /\ \A u \in todo \ {x} : ~ Before(u, x)
       /\ SerialOK(TxE(x).g, s)
       /\ TxE(x).out = Effect(TxE(x).g, s)
       /\ SerFrom(todo \ {x}, Apply(s, TxE(x).out))
OutcomeSerializable == SerFrom(CTx, Init0)

\* 1. A stale false is reported only if false at the leaders' commit
\* indexes (the reported error is fresh).
FreshFalse ==
  \A t \in Reqs : (st[t] = "fail" /\ why[t] = "missing") =>
                    ~ GuardOn(ProgOf(t), StateAt(failAt[t]))

\* 1. S' joins the context: a closed version holding r holds c's creation,
\* the fact that made accept's guard true.
CauseVisible ==
  \A V \in AllClosed :
    (\E j \in 1..V[P2] : log[P2][j].kind = "tx" /\ log[P2][j].g = "accept")
      => Val(V, "cLife") # "absent"

\* every delta's old values are the values at its log position (the
\* valid-transition hypothesis of wf_stateAt_closed; no lost updates)
DeltaOldExact ==
  \A p \in Parts : \A j \in 1..Len(log[p]) : \A k \in DOMAIN log[p][j].W :
    log[p][j].W[k][1] = ValLog(p, k, j - 1)

\* 2. The ctx rule: every closed version holding a resolved op's entry
\* holds the key's previous writer and that writer's context.
CtxRule ==
  \A j \in 1..Len(log[P2]) : \A k \in log[P2][j].ops :
    LET js == {i \in 1..(j - 1) : k \in DOMAIN log[P2][i].W} IN
    /\ Leq(log[P2][j].S, log[P2][j].ctx)
    /\ js # {} => \A V \in AllClosed :
                    V[P2] >= j => (V[P2] >= MaxSet(js) /\ Leq(log[P2][MaxSet(js)].ctx, V))

\* 2. Writes to one key are causally ordered.
KeyOrder ==
  \A p \in Parts : \A j1, j2 \in 1..Len(log[p]) :
    (j1 < j2 /\ DOMAIN log[p][j1].W \cap DOMAIN log[p][j2].W # {})
      => log[p][j2].ctx[p] >= j1

\* 2. A pipelined bounded op never crosses its bound: replaying the ops
\* themselves (not the recorded values) over the appended log.
RECURSIVE TV(_)
TV(j) == IF j = 0 THEN StockInit
         ELSE LET e == log[P2][j]
                  pv == TV(j - 1)
              IN IF "iStock" \notin DOMAIN e.W THEN pv
                 ELSE IF e.ops = {} THEN e.W["iStock"][2]
                 ELSE IF pv = NoVal \/ pv = Bad THEN Bad
                 ELSE pv + OpD(e.g)
BoundNeverCrossed ==
  \A j \in 1..Len(log[P2]) : TV(j) # Bad /\ (TV(j) # NoVal => TV(j) >= 0)

\* 2. A field that another consumer reads is never leader-resolved.
CompiledAsValidated ==
  \A p \in Parts : \A e \in Entries(p) : e.ops \cap ConsumerReads(e.g) = {}

\* 3. No committed delta breaks a discipline in force at its position: the
\* newer of the registry it was computed under and the partition's barrier.
DisciplinesHold ==
  \A j \in 1..commit[P1] :
    LET e == log[P1][j] IN
    e.kind = "tx" => ~ BreaksAt(e.W, Max(e.pv, BarRv(P1, j)))

\* 3. A plan relies on discipline D in P1 only if S contains P1's
\* enforcement barrier; a plan relying on the old D committed before the
\* re-plan barrier.
RelyNeedsBarrier ==
  \A j \in 1..Len(log[P2]) :
    LET e == log[P2][j] IN
    (e.kind = "tx" /\ e.g = "accept" /\ e.skip) =>
      /\ Change = "strengthen" => (HasBar(P1, 1) /\ BarIdx(P1, 1) <= e.S[P1])
      /\ Change = "weaken" => ~ (HasBar(P2, 1) /\ BarIdx(P2, 1) < j)

\* 3. A weakening is in force only after every affected partition
\* committed the re-plan barrier.
WeakenAfterAck ==
  (Change = "weaken" /\ reg = 2) => \A p \in BarrierParts1 : Acked(p, 1)

\* 4. A decision that isn't RevocationSensitive, served from any lagging
\* version, never grants what a fresh read denies.
Objs == {"c", "i", "iw"}
AllowAt(o, V) ==
  CASE o = "c"  -> Val(V, "cLife") = "live" /\ Val(V, "cOwner") = "u1"
    [] o = "i"  -> Val(V, "iLife") = "live"                 \* owner frozen = u1
    [] o = "iw" -> Val(V, "iLife") = "live" /\ Val(V, "iWatch")
\* not RevocationSensitive: lifecycle stable (permanent, relied on legally)
\* and the Allow's inputs unchanging
NotRS(o, V) ==
  CASE o = "c"  -> PlanPerm(reg) /\ PlanFrozen(reg) /\ RelyOK(V)
    [] o = "i"  -> ~ VisibleRule                  \* owner frozen; lifecycle mutable
    [] o = "iw" -> FALSE                          \* watchers: mutable edge
ReadHorizon ==
  \A o \in Objs, V \in ClosedVersions :
    ((NotRS(o, V) \/ V = Fresh) /\ AllowAt(o, V)) => AllowAt(o, Fresh)

(*************************** reachability (sanity) ***********************)
(* Each is expected to be VIOLATED in the matching Planner_reach_*.cfg,     *)
(* showing the passing configs aren't vacuous.                            *)

CEntries(p) == {log[p][j] : j \in 1..commit[p]}
\* a skipped cross-partition require commits, its S after c's creation
ReachSkip == ~ \E e \in CEntries(P2) : e.g = "accept" /\ e.skip /\ e.S[P1] >= 1
\* a skipped require commits (c initially live)
ReachSkip0 == ~ \E e \in CEntries(P2) : e.g = "accept" /\ e.skip
\* a validated (re-planned) require commits through a read-only participant
ReachValidated == ~ \E e \in CEntries(P1) : e.g = "accept" /\ e.kind = "ro"
\* a stale false is reported after the retry at the leaders' commit index
ReachMissing == ~ \E t \in Reqs : why[t] = "missing"
\* purge commits (after the weakening)
ReachPurge == ~ \E e \in CEntries(P1) : e.g = "purge"
\* reassign commits (before the strengthening)
ReachReassign == ~ \E e \in CEntries(P1) : e.g = "reassign"
\* both pipelined decrements commit, the second with a pending prev
ReachSelfPre == ~ \E e \in CEntries(P2) : e.ops # {} /\ e.selfpre
\* a pipelined decrement is rejected by its bound at the leader
ReachInvalid == ~ \E t \in Reqs : why[t] = "invalid"
\* a decrement resolves against an uncommitted in-window write
ReachWindow == ~ \E j \in 1..Len(log[P2]) :
                   LET e == log[P2][j] IN
                   e.ops # {} /\ \E i \in 1..(j - 1) : i > e.S[P2] /\ "iStock" \in DOMAIN log[P2][i].W

\* the self-prefix flag expands across partitions: the flagged entry's own
\* context misses a P1 dot that its pending prev's context names
ReachXSelfPre ==
  ~ \E j \in 1..Len(log[P2]) :
      LET e == log[P2][j]
          js == {i \in 1..(j - 1) : "iStock" \in DOMAIN log[P2][i].W}
      IN e.ops # {} /\ e.selfpre /\ js # {} /\ log[P2][MaxSet(js)].ctx[P1] > e.ctx[P1]

=============================================================================
