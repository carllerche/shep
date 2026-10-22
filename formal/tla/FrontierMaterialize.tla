------------------------- MODULE FrontierMaterialize -------------------------
(***************************************************************************)
(* Shep §3, §9.4, §9.7, §10.4, §12: partition logs, materializers, the    *)
(* published frontier, causally closed reads, retention, and a view that  *)
(* steps between frontiers.                                                *)
(*                                                                         *)
(* Partitions. Each partition has a committed log (Raft is abstracted to  *)
(* "append = commit"). Each partition holds one key, named after it. An   *)
(* entry is                                                                *)
(*   "w"    a single-partition mutation: writes its partition's key,       *)
(*          stamped with its own dot;                                      *)
(*   "prep" a two-phase-commit prepare in Part2: stamps nothing;           *)
(*   "dec"  the commit decision in Home: its context names the prepare,    *)
(*          and it writes Home's key stamped with its own dot (the home's  *)
(*          part, abstracted into the decision);                           *)
(*   "rel"  the intent release in Part2: Part2's part becomes visible     *)
(*          here, at this log position, stamped with the decision dot.    *)
(* A context is the writer's read version: its own partition's committed  *)
(* prefix, the newest stamp of the key it overwrites (validation, §10.3), *)
(* and optionally another partition's committed prefix (a token it        *)
(* carried). Versions are modelled as the sets of dots they denote (as in *)
(* MultiNode.tla, interpretation I4).                                      *)
(*                                                                         *)
(* Materializers. One per partition, plus a second, independent replica   *)
(* of ReplPart. Each applies its log in order into a store of versions    *)
(* <<key, pos, stamp>> (§3's key layout: pos is the log position of the   *)
(* entry that wrote the version, the stamp the dot that makes it          *)
(* visible), and keeps m/applied and m/cf (the union of contexts of       *)
(* entries 1..applied). It flushes in batches and can crash, losing what  *)
(* it hadn't flushed.                                                      *)
(*                                                                         *)
(* Frontier. Materializers report applied indexes; the meta leader        *)
(* publishes the largest causally closed set of reported entries. A       *)
(* report and the publish are one step; a leader crash loses only         *)
(* reports, so it is not modelled separately.                              *)
(*                                                                         *)
(* Readers. Pick a start version (an at_least token for one partition, or *)
(* the published frontier), close it by the cf fixpoint, are admitted by  *)
(* each partition's materializer (which pins the version against the      *)
(* retention floor, or answers too_old), wait until the materializer has  *)
(* applied the version's dots there and resolved in-doubt 2PCs (§9.7),    *)
(* and read the newest version whose stamp is in V.                       *)
(*                                                                         *)
(* Retention. Each materializer's floor (a log position) advances up to   *)
(* its flushed index, but not past a pinned reader's version. Compaction  *)
(* keeps, for each key, the newest version at or below the floor (live or *)
(* ghost alike: the value is abstracted to the stamp) and every version   *)
(* above it.                                                               *)
(*                                                                         *)
(* View. A maintainer over the keys of footprint FP steps from its        *)
(* version to the published frontier, adding the Z-set deltas (with old   *)
(* values) of the dots in F2 \ F1.                                         *)
(*                                                                         *)
(* Knobs (correct design first):                                           *)
(*  CtxClosed    TRUE: contexts are causally closed (closure of what was   *)
(*               read); FALSE: a context is the raw indexes read (F12).    *)
(*  ReleaseCtx   TRUE: a release's context is its decision dot (closed);  *)
(*               FALSE: a release has no context (§8, §10 say nothing).   *)
(*  FrontierMode "close": F' = F \cup Core(reported); "max": per-partition*)
(*               max of reported applied, no closure; "nokeep":           *)
(*               Core(latest reports), not merged with what was published.*)
(*  RestartMode  "batch": m/applied is in the data's write batch;          *)
(*               "perEntry": m/applied is made durable per entry, data    *)
(*               only at flush.                                            *)
(*  FloorMode    "pins": the floor stops at pinned readers; "ignore".     *)
(*  ReaderMode   "fix": close by fixpoint; "once": one cf lookup (§9.4);  *)
(*               "none": read each partition at a replica's applied index,*)
(*               no closure.                                               *)
(*  InDoubt      TRUE: a read resolves in-doubt 2PCs (§9.7).              *)
(***************************************************************************)
EXTENDS Naturals, Integers, Sequences, FiniteSets, TLC

CONSTANTS
  Parts, Home, Part2, HasTxn, ReplPart,
  MaxW, MaxCrash, MaxReads, FP,
  CtxClosed, ReleaseCtx, FrontierMode, RestartMode, FloorMode, ReaderMode, InDoubt,
  EnableFrontier, EnableView, EnableReader, EnableRetention, ReadFrontier

ASSUME HasTxn => (Home \in Parts /\ Part2 \in Parts /\ Home /= Part2)
ASSUME FP \subseteq Parts

MaxLen == MaxW + 2
Dots == Parts \X (1..MaxLen)
NoDot == <<"none", 0>>
NoM == <<"none", 0>>
Mats == {<<p, 1>> : p \in Parts} \cup (IF ReplPart \in Parts THEN {<<ReplPart, 2>>} ELSE {})
MatsOf(p) == {m \in Mats : m[1] = p}
Big == 1000

VARIABLES
  log,      \* log[p]: committed entries [kind, ctx, dec]
  writes,   \* writes[k]: stamps written to key k, in commit order (ghost)
  tx,       \* the one 2PC: [st, prep, dec, rel]
  nw,       \* single-partition writes so far
  app,      \* app[m]: m/applied (in memory, including unflushed batches)
  fl,       \* fl[m]: last flushed index
  db,       \* db[m]: stored versions <<key, pos, stamp>>
  cf,       \* cf[m]: m/cf
  floor,    \* floor[m]: retention floor (a log position)
  crashes, twice,
  F,        \* published frontier
  rpc, rV, rpin, rres, rgot, nreads,   \* the reader
  vver, vZ                             \* the view

wvars == <<log, writes, tx, nw>>
mvars == <<app, fl, db, cf, floor, crashes, twice>>
fvars == <<F>>
rvars == <<rpc, rV, rpin, rres, rgot, nreads>>
vvars == <<vver, vZ>>
vars == <<wvars, mvars, fvars, rvars, vvars>>

(*************************** versions and closure **************************)

Ctx(d) == log[d[1]][d[2]].ctx
Committed == UNION {{<<p, i>> : i \in 1..Len(log[p])} : p \in Parts}
Pre(p, n) == {<<p, i>> : i \in 1..n}
Closed(S) == \A d \in S : Ctx(d) \subseteq S

RECURSIVE CloseR(_)
CloseR(S) == LET S2 == S \cup UNION {Ctx(d) : d \in S}
             IN IF S2 = S THEN S ELSE CloseR(S2)

MkCtx(S) == IF CtxClosed THEN CloseR(S) ELSE S

\* the largest causally closed subset of S
RECURSIVE Core(_)
Core(S) == LET S2 == {d \in S : Ctx(d) \subseteq S}
           IN IF S2 = S THEN S ELSE Core(S2)

\* cf(p, n): the union of the contexts of entries 1..n
CF(p, n) == UNION {Ctx(<<p, i>>) : i \in 1..n}

Max(a, b) == IF a >= b THEN a ELSE b
Min(a, b) == IF a <= b THEN a ELSE b
SetMax(I) == IF I = {} THEN 0 ELSE CHOOSE i \in I : \A j \in I : j <= i

\* highest index of p in V, and the length of V's contiguous prefix of p
MaxIdx(V, p) == SetMax({d[2] : d \in {e \in V : e[1] = p}})
PrefLen(V, p) == CHOOSE n \in 0..MaxLen : Pre(p, n) \subseteq V /\ <<p, n + 1>> \notin V

AppSet(r) == UNION {Pre(p, r[p]) : p \in Parts}

LastStamp(k) == IF writes[k] = <<>> THEN {} ELSE {writes[k][Len(writes[k])]}

(*************************** stored versions *******************************)

\* versions <<key, pos, stamp>> written by entry i of partition p
VersionsOf(p, i) ==
  LET e == log[p][i] IN
  CASE e.kind \in {"w", "dec"} -> {<<p, i, <<p, i>>>>}
    [] e.kind = "rel"          -> {<<p, i, e.dec>>}
    [] OTHER                   -> {}

Vers(p, n) == UNION {VersionsOf(p, i) : i \in 1..n}

\* §3: the newest version (by position) whose stamp is in V
ReadAt(m, V) ==
  LET vs == {v \in db[m] : v[3] \in V}
  IN IF vs = {} THEN NoDot
     ELSE (CHOOSE v \in vs : \A w \in vs : w[2] <= v[2])[3]

\* §9.2, the definition: the newest write to k whose stamp is in V
Fold(V, k) ==
  LET s == writes[k]
      I == {i \in 1..Len(s) : s[i] \in V}
  IN IF I = {} THEN NoDot ELSE s[SetMax(I)]

\* §9.7: a prepare of p in V whose decision is in V, and whose release m
\* hasn't applied, is in doubt; the read waits for the release
Resolved(m, V) ==
  ~( HasTxn /\ tx.prep[1] = m[1] /\ tx.prep \in V /\ tx.dec \in V
     /\ ~(tx.rel /= NoDot /\ app[m] >= tx.rel[2]) )

\* m can serve a read at V for its partition
CanServe(m, V) == MaxIdx(V, m[1]) <= app[m] /\ Resolved(m, V)

(*************************** Z-sets for the view ***************************)

ZZero == [x \in Dots |-> 0]
ZAdd(a, b) == [x \in Dots |-> a[x] + b[x]]

\* the delta of dot d on key k: -old +new
Delta(d, k) ==
  LET s == writes[k]
      I == {i \in 1..Len(s) : s[i] = d}
  IN IF I = {} THEN ZZero
     ELSE LET i == CHOOSE i \in I : TRUE IN
          [x \in Dots |-> (IF x = d THEN 1 ELSE 0)
                          - (IF i > 1 /\ x = s[i - 1] THEN 1 ELSE 0)]

RECURSIVE SumDelta(_, _)
SumDelta(S, k) == IF S = {} THEN ZZero
                  ELSE LET d == CHOOSE d \in S : TRUE
                       IN ZAdd(Delta(d, k), SumDelta(S \ {d}, k))

ZOf(V, k) == [x \in Dots |-> IF Fold(V, k) = x THEN 1 ELSE 0]

\* well-formed (Lean WF): a set, with at most one value per field
WFZ(z) == (\A x \in Dots : z[x] \in {0, 1})
          /\ (\A x, y \in Dots : z[x] = 1 /\ z[y] = 1 => x = y)

(*************************** init ******************************************)

ReaderReset ==
  /\ rpc' = "idle"
  /\ rV' = {}
  /\ rpin' = [p \in Parts |-> NoM]
  /\ rres' = [p \in Parts |-> NoDot]
  /\ rgot' = {}

Init ==
  /\ log = [p \in Parts |-> <<>>]
  /\ writes = [k \in Parts |-> <<>>]
  /\ tx = [st |-> "none", prep |-> NoDot, dec |-> NoDot, rel |-> NoDot]
  /\ nw = 0
  /\ app = [m \in Mats |-> 0]
  /\ fl = [m \in Mats |-> 0]
  /\ db = [m \in Mats |-> {}]
  /\ cf = [m \in Mats |-> {}]
  /\ floor = [m \in Mats |-> 0]
  /\ crashes = 0
  /\ twice = FALSE
  /\ F = {}
  /\ rpc = "idle"
  /\ rV = {}
  /\ rpin = [p \in Parts |-> NoM]
  /\ rres = [p \in Parts |-> NoDot]
  /\ rgot = {}
  /\ nreads = 0
  /\ vver = {}
  /\ vZ = [k \in FP |-> ZZero]

(*************************** writers (the WAL) *****************************)

\* intents: Part2's key is locked from the prepare until the release (§8)
Locked(p) == HasTxn /\ p = Part2 /\ tx.st \in {"prep", "dec"}

\* a single-partition mutation on p; dep = another partition whose
\* committed prefix it read (a token it carried), or "none"
Write(p, dep) ==
  /\ nw < MaxW
  /\ ~Locked(p)
  /\ dep = "none" \/ (dep \in Parts \ {p} /\ Len(log[dep]) > 0)
  /\ LET i == Len(log[p]) + 1
         other == IF dep = "none" THEN {} ELSE Pre(dep, Len(log[dep]))
         c == MkCtx(Pre(p, Len(log[p])) \cup LastStamp(p) \cup other)
     IN /\ log' = [log EXCEPT ![p] = Append(@, [kind |-> "w", ctx |-> c, dec |-> NoDot])]
        /\ writes' = [writes EXCEPT ![p] = Append(@, <<p, i>>)]
  /\ nw' = nw + 1
  /\ UNCHANGED <<tx, mvars, fvars, rvars, vvars>>

Prepare ==
  /\ HasTxn /\ tx.st = "none"
  /\ LET i == Len(log[Part2]) + 1
         c == MkCtx(Pre(Part2, Len(log[Part2])) \cup LastStamp(Part2))
     IN /\ log' = [log EXCEPT ![Part2] = Append(@, [kind |-> "prep", ctx |-> c, dec |-> NoDot])]
        /\ tx' = [tx EXCEPT !.st = "prep", !.prep = <<Part2, i>>]
  /\ UNCHANGED <<writes, nw, mvars, fvars, rvars, vvars>>

\* §8 step 2: the decision's context merges the read versions and names
\* every prepare
Decide ==
  /\ tx.st = "prep"
  /\ LET j == Len(log[Home]) + 1
         d == <<Home, j>>
         c == MkCtx(Pre(Home, Len(log[Home])) \cup LastStamp(Home)
                    \cup Ctx(tx.prep) \cup {tx.prep})
     IN /\ log' = [log EXCEPT ![Home] = Append(@, [kind |-> "dec", ctx |-> c, dec |-> NoDot])]
        /\ writes' = [writes EXCEPT ![Home] = Append(@, d), ![Part2] = Append(@, d)]
        /\ tx' = [tx EXCEPT !.st = "dec", !.dec = d]
  /\ UNCHANGED <<nw, mvars, fvars, rvars, vvars>>

Release ==
  /\ tx.st = "dec"
  /\ LET r == Len(log[Part2]) + 1
         c == IF ReleaseCtx THEN MkCtx({tx.dec}) ELSE {}
     IN /\ log' = [log EXCEPT ![Part2] = Append(@, [kind |-> "rel", ctx |-> c, dec |-> tx.dec])]
        /\ tx' = [tx EXCEPT !.st = "rel", !.rel = <<Part2, r>>]
  /\ UNCHANGED <<writes, nw, mvars, fvars, rvars, vvars>>

(*************************** materializers *********************************)

\* §10.4: apply the next entry, stamped, with m/applied and m/cf, in one batch
Apply(m) ==
  LET p == m[1] IN
  /\ app[m] < Len(log[p])
  /\ LET i == app[m] + 1
         vs == VersionsOf(p, i)
     IN /\ twice' = (twice \/ vs \cap db[m] /= {})
        /\ db' = [db EXCEPT ![m] = @ \cup vs]
        /\ cf' = [cf EXCEPT ![m] = @ \cup Ctx(<<p, i>>)]
        /\ app' = [app EXCEPT ![m] = i]
  /\ UNCHANGED <<wvars, fl, floor, crashes, fvars, rvars, vvars>>

Flush(m) ==
  /\ fl[m] < app[m]
  /\ fl' = [fl EXCEPT ![m] = app[m]]
  /\ UNCHANGED <<wvars, app, db, cf, floor, crashes, twice, fvars, rvars, vvars>>

\* lose every unflushed batch. Compaction only ever touches versions at or
\* below the floor <= fl, so the flushed store is db restricted to <= fl.
Crash(m) ==
  /\ crashes < MaxCrash
  /\ crashes' = crashes + 1
  /\ db' = [db EXCEPT ![m] = {v \in @ : v[2] <= fl[m]}]
  /\ cf' = [cf EXCEPT ![m] = CF(m[1], fl[m])]     \* m/cf, flushed with its batch
  /\ app' = [app EXCEPT ![m] = IF RestartMode = "batch" THEN fl[m] ELSE @]
  /\ UNCHANGED <<wvars, fl, floor, twice, fvars, rvars, vvars>>

\* the oldest version a pinned read still uses at m
MinPin(m) == IF rpc = "reading" /\ rpin[m[1]] = m THEN PrefLen(rV, m[1]) ELSE Big

\* advance the floor, then compact: below the floor keep only each key's
\* newest version, live or ghost (§3, §10.4)
AdvanceFloor(m) ==
  /\ EnableRetention
  /\ LET nf == IF FloorMode = "ignore" THEN fl[m] ELSE Min(fl[m], MinPin(m))
     IN /\ nf > floor[m]
        /\ floor' = [floor EXCEPT ![m] = nf]
        /\ db' = [db EXCEPT ![m] = {v \in @ : ~\E w \in @ : w[1] = v[1] /\ v[2] < w[2] /\ w[2] <= nf}]
  /\ UNCHANGED <<wvars, app, fl, cf, crashes, twice, fvars, rvars, vvars>>

(*************************** the frontier **********************************)

\* §9.7: each materializer reports its applied index; the meta leader
\* publishes the largest causally closed set of reported entries. A report
\* and the publish are folded into one step: the leader uses, per
\* partition, the report of one replica (rm). A leader crash loses only
\* reports, which this step re-reads; F itself is in the meta partition.
Publish(rm) ==
  /\ EnableFrontier
  /\ \A p \in Parts : rm[p] \in MatsOf(p)
  /\ LET newest == [p \in Parts |-> SetMax({app[m] : m \in MatsOf(p)})]
         chosen == [p \in Parts |-> app[rm[p]]]
         nf == CASE FrontierMode = "close"  -> F \cup Core(AppSet(newest))
                 [] FrontierMode = "max"    -> AppSet(newest)
                 [] FrontierMode = "nokeep" -> Core(AppSet(chosen))
     IN /\ nf /= F
        /\ F' = nf
  /\ UNCHANGED <<wvars, mvars, rvars, vvars>>

(*************************** the reader ************************************)

Pick ==
  /\ EnableReader
  /\ rpc = "idle"
  /\ nreads < MaxReads
  /\ \/ /\ ReaderMode \in {"fix", "once"}
        /\ \E p \in Parts : \E n \in 1..Len(log[p]) : rV' = Pre(p, n)
        /\ rpc' = "closing"
     \/ /\ ReaderMode = "none"
        /\ \E ms \in [Parts -> Mats] :
             /\ \A p \in Parts : ms[p][1] = p
             /\ rV' = UNION {Pre(p, app[ms[p]]) : p \in Parts}
        /\ rpc' = "reading"
     \/ /\ ReadFrontier /\ F /= {}
        /\ rV' = F
        /\ rpc' = "reading"
  /\ UNCHANGED <<wvars, mvars, fvars, rpin, rres, rgot, nreads, vvars>>

\* §9.7: add cf(p, V[p]) for every partition in V, once a materializer of
\* p has applied V[p]; repeat until nothing changes
CloseStep ==
  /\ rpc = "closing"
  /\ \A p \in Parts : MaxIdx(rV, p) = 0 \/ \E m \in MatsOf(p) : app[m] >= MaxIdx(rV, p)
  /\ LET V2 == rV \cup UNION {CF(p, MaxIdx(rV, p)) : p \in Parts}
     IN /\ rV' = V2
        /\ rpc' = IF V2 = rV \/ ReaderMode = "once" THEN "reading" ELSE "closing"
  /\ UNCHANGED <<wvars, mvars, fvars, rpin, rres, rgot, nreads, vvars>>

\* admission at a materializer of p pins V, or answers too_old
Admit(p, m) ==
  /\ rpc = "reading"
  /\ rpin[p] = NoM
  /\ m \in MatsOf(p)
  /\ IF PrefLen(rV, p) >= floor[m]
       THEN /\ rpin' = [rpin EXCEPT ![p] = m]
            /\ UNCHANGED <<rpc, rV, rres, rgot, nreads>>
       ELSE /\ ReaderReset
            /\ nreads' = nreads + 1
  /\ UNCHANGED <<wvars, mvars, fvars, vvars>>

\* at_least V: wait until m has applied V's dots in p (and, with InDoubt,
\* resolved in-doubt 2PCs), then read
ReadPart(p) ==
  /\ rpc = "reading"
  /\ rpin[p] /= NoM
  /\ p \notin rgot
  /\ MaxIdx(rV, p) <= app[rpin[p]]
  /\ InDoubt => Resolved(rpin[p], rV)
  /\ rres' = [rres EXCEPT ![p] = ReadAt(rpin[p], rV)]
  /\ rgot' = rgot \cup {p}
  /\ UNCHANGED <<wvars, mvars, fvars, rpc, rV, rpin, nreads, vvars>>

ReadDone ==
  /\ rpc = "reading"
  /\ rgot = Parts
  /\ ReaderReset
  /\ nreads' = nreads + 1
  /\ UNCHANGED <<wvars, mvars, fvars, vvars>>

(*************************** the view **************************************)

\* §9.7/§12: step from vver to F, adding the deltas of the dots in F \ vver
ViewStep ==
  /\ EnableView
  /\ F /= vver
  /\ vZ' = [k \in FP |-> ZAdd(vZ[k], SumDelta(F \ vver, k))]
  /\ vver' = F
  /\ UNCHANGED <<wvars, mvars, fvars, rvars>>

(*************************** spec ******************************************)

Next ==
  \/ \E p \in Parts : \E dep \in Parts \cup {"none"} : Write(p, dep)
  \/ Prepare \/ Decide \/ Release
  \/ \E m \in Mats : Apply(m) \/ Flush(m) \/ Crash(m) \/ AdvanceFloor(m)
  \/ \E rm \in [Parts -> Mats] : Publish(rm)
  \/ Pick \/ CloseStep \/ ReadDone
  \/ \E p \in Parts : ReadPart(p) \/ \E m \in Mats : Admit(p, m)
  \/ ViewStep

FairNoRelease ==
  /\ \A m \in Mats : WF_vars(Apply(m))
  /\ WF_vars(\E rm \in [Parts -> Mats] : Publish(rm)) /\ WF_vars(Decide)
  /\ WF_vars(CloseStep) /\ WF_vars(ReadDone) /\ WF_vars(ViewStep)
  /\ \A p \in Parts : WF_vars(\E m \in Mats : Admit(p, m)) /\ WF_vars(ReadPart(p))

Fair == FairNoRelease /\ WF_vars(Release)

Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ Fair
\* no participant ever appends the release (no timeout recovery, §8)
FairSpecNoRelease == Spec /\ FairNoRelease

(*************************** properties ************************************)

\* 1. every published frontier is causally closed, committed, and monotone
FrontierClosed == Closed(F) /\ F \subseteq Committed
FrontierMonotone == [][F \subseteq F']_F

\* 2. a closed version is causally closed and <= committed
ReadClosed == rpc = "reading" => (Closed(rV) /\ rV \subseteq Committed)

\* 3. reads at V return the definition's state at V
ReadExact == \A p \in rgot : rres[p] = Fold(rV, p)

\* the versions every materializer can be asked about: closures of single
\* partitions' prefixes, and the frontier
Family == UNION {{CloseR(Pre(p, n)) : n \in 0..Len(log[p])} : p \in Parts} \cup {F}

\* 3. and every replica gives the same answer
ReplicasAgree ==
  \A V \in Family : \A m1, m2 \in Mats :
     (m1[1] = m2[1] /\ CanServe(m1, V) /\ CanServe(m2, V)
      /\ PrefLen(V, m1[1]) >= Max(floor[m1], floor[m2]))
       => ReadAt(m1, V) = ReadAt(m2, V)

\* 4. a materializer's store is exactly entries 1..applied, minus what
\* compaction may drop, and m/cf matches; no entry applied twice
Shadowed(v, m) == \E w \in Vers(m[1], app[m]) : w[1] = v[1] /\ v[2] < w[2] /\ w[2] <= floor[m]
MatConsistent ==
  \A m \in Mats :
     /\ db[m] \subseteq Vers(m[1], app[m])
     /\ \A v \in Vers(m[1], app[m]) : ~Shadowed(v, m) => v \in db[m]
     /\ cf[m] = CF(m[1], app[m])
     /\ ~twice

\* 6. the floor never passes a pinned version, and reads at any version
\* at or above the floor stay exact
FloorSafe == \A p \in Parts : rpin[p] /= NoM => floor[rpin[p]] <= PrefLen(rV, p)
AboveFloorExact ==
  \A m \in Mats : \A V \in Family :
     (PrefLen(V, m[1]) >= floor[m] /\ CanServe(m, V)) => ReadAt(m, V) = Fold(V, m[1])

\* 7. the view's deltas chain to the full result at its version, which is
\* well-formed (Lean wf_stateAt_closed)
ViewExact == \A k \in FP : vZ[k] = ZOf(vver, k) /\ WFZ(vZ[k])

\* §3: writes to one key are causally ordered
KeyChain == \A k \in Parts : \A i \in 1..(Len(writes[k]) - 1) :
              writes[k][i] \in CloseR(Ctx(writes[k][i + 1]))

Safety == FrontierClosed /\ ReadClosed /\ ReadExact /\ ReplicasAgree /\ MatConsistent
          /\ FloorSafe /\ AboveFloorExact /\ ViewExact /\ KeyChain

\* 2. closing terminates; 5. at_least terminates; the frontier and the
\* view eventually cover every committed dot
ClosureTerminates == (rpc = "closing") ~> (rpc /= "closing")
AtLeastTerminates == (rpc = "reading") ~> (rpc = "idle")
FrontierLive == \A d \in Dots : (d \in Committed) ~> (d \in F)
ViewLive == \A d \in Dots : (d \in Committed) ~> (d \in vver)
=============================================================================
