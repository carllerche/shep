------------------------------ MODULE Cutover ------------------------------
(***************************************************************************)
(* Shep design §4.2 (schema cutover: Prepared -> DualWrite -> Active, or   *)
(* Abandoned), §4.3-§4.4 (adapters, lossy write adapters, backfill),       *)
(* §4.6 (lifecycle), §10.3 (registry barriers, backfill as validated       *)
(* mutations), §9.6 (splits, barrier re-delivery, the split handoff),      *)
(* §11.7 / §13.2 (freshness checks and live topics count registry dots)    *)
(* and §14.2-§14.3 (test bases are pinned with no cutover open).           *)
(*                                                                         *)
(* One schema, one field f, one step old head -> new head, a TIGHTENING    *)
(* with a lossy write adapter (`String limit 200 -> 100`, truncate):       *)
(*   values 0..2;  old head accepts 0..2;  new head accepts 0..1           *)
(*   put(v) = min(v, 1)  (truncate, lossy on 2);  get = identity           *)
(*   the new head also adds a local #[unique] on f within group G1.        *)
(* Rows r1, r2 are in group G1 (the unique scope), r3 in group G3.         *)
(*                                                                         *)
(* Partitions: P1 holds G1, P2 holds G3. P1 may split once, at a group     *)
(* boundary, into C1 (G1) and an empty sibling the model leaves out. A     *)
(* "lineage" log is P1's log followed by C1's: the split entry ("s") at    *)
(* index i is also C1's first entry, with context {P1: i} and the split    *)
(* handoff's latest registry dot (§9.6). Versions are prefixes of the two  *)
(* lineages plus a prefix m of the meta partition's registry log; a        *)
(* version is causally closed iff every entry it holds has its registry    *)
(* context inside it, and every activation entry it holds has its guard    *)
(* snapshot inside it.                                                     *)
(*                                                                         *)
(* Writers are front ends whose registry caches have any staleness: a      *)
(* write picks a registry prefix m (its cache) at least as new as the      *)
(* partition's latest barrier (§10.3; an older cache gets stale_registry   *)
(* and refreshes, which is just another attempt). The write is staged      *)
(* under the cutover state at m. Old-version writers may write any value;  *)
(* new-version writers exist only once m is Active (not_ready before).     *)
(* A rejected attempt changes nothing, so only commits are transitions.    *)
(*                                                                         *)
(* The meta leader's driver: register (Prepared, with barriers), the       *)
(* Prepared unique check, the DualWrite registry change and its barriers,  *)
(* DualWrite entered once every barrier is delivered, backfill per         *)
(* partition with a cursor, the activation guard and activation entry,     *)
(* abandon, and registering the next version afterwards.                   *)
(*                                                                         *)
(* Readers of the old version are modelled by their LENS: the value an    *)
(* old reader sees for a stored value x at version V. A freshness check    *)
(* (§11.7) answers "unchanged" from V1 to V2 iff no visited row has a new  *)
(* stamp and no counted registry dot is in V2 \ V1; a live topic resyncs   *)
(* exactly at counted registry dots and otherwise applies row deltas.      *)
(* Both are sound iff every single-entry step between closed versions      *)
(* that changes an old reader's view is a write to that row or a counted   *)
(* registry dot (Inv_Lens); any longer step is a chain of such steps.      *)
(*                                                                         *)
(* The constants select design variants; see the cfgs.                     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  MaxWrites,            \* bound on user writes (contention bound)
  LensSwitch,           \* "barrier": old readers read through get(put(x)) from
                        \*   DualWrite on (design); "activation": from Active on
  LensKey,              \* "registry": the lens at V is chosen by the registry
                        \*   at V (design, §10.3 "under the registry at S");
                        \*   "barrier": by whether V holds the partition's
                        \*   DualWrite barrier entry
  CountDots,            \* registry dots freshness checks / topics count for
                        \*   old-version readers: subset of {"dual","act","abandon"}
  UniqueEnforce,        \* where writes enforce the new head's unique, over
                        \*   new-head images put(x): "index" from the Prepared
                        \*   barrier on; "dual" from DualWrite on; "none" only
                        \*   once Active
  ActCheckUnique,       \* the activation guard checks the unique constraint
  ActCtx,               \* the activation entry's context is the guard snapshot
  BackfillValidate,     \* backfill rewrites are validated (OCC) mutations
  BackfillSnap,         \* "afterBarrier": backfill reads at snapshots holding
                        \*   its partition's DualWrite barrier; "any": any snapshot
  Redeliver,            \* a barrier sent to a split parent goes to its children
  CursorRemap,          \* at a split, the child's cursor: "remap", "reset", "lost"
  AbandonAfterBackfill, \* abandon allowed after a backfill write
  WritePolicy,          \* DualWrite old writes: "rejectLossy" (the patch must be
                        \*   valid for both heads) or "putLossy" (put's result must)
  BaseCheck,            \* a test base is pinned only where no cutover is open
  WithSplit,            \* P1 may split
  WithNext,             \* register a newer version after Active/Abandoned
  WithBase              \* a test base may be taken

ASSUME LensSwitch \in {"barrier", "activation"}
ASSUME LensKey \in {"registry", "barrier"}
ASSUME CountDots \subseteq {"dual", "act", "abandon"}
ASSUME UniqueEnforce \in {"index", "dual", "none"}
ASSUME BackfillSnap \in {"afterBarrier", "any"}
ASSUME CursorRemap \in {"remap", "reset", "lost"}
ASSUME WritePolicy \in {"rejectLossy", "putLossy"}

Rows == {"r1", "r2", "r3"}
Lines == {"L1", "L2"}
Parts == {"P1", "P2", "C1"}
LineOf(r) == IF r = "r3" THEN "L2" ELSE "L1"
LineOfPart(p) == IF p = "P2" THEN "L2" ELSE "L1"
RowSeq(L) == IF L = "L1" THEN <<"r1", "r2">> ELSE <<"r3">>
RowsOf(L) == IF L = "L1" THEN {"r1", "r2"} ELSE {"r3"}
NRows(L) == Len(RowSeq(L))
InG1(r) == r \in {"r1", "r2"}
Other(r) == IF r = "r1" THEN "r2" ELSE "r1"

Vals == 0..2
NewVals == 0..1
OldValid(v) == v \in Vals
NewValid(v) == v \in NewVals
Put(v) == IF v > 1 THEN 1 ELSE v      \* truncate: the lossy write adapter
Get(v) == v                            \* the read adapter (identity)
GP(v) == Get(Put(v))                   \* the new step's lens on a stored value
Img(v) == Put(v)                       \* a stored value's new-head image

Init0 == [r \in Rows |-> IF r = "r2" THEN 0 ELSE 2]

Max(S) == CHOOSE x \in S : \A y \in S : y <= x
Min(S) == CHOOSE x \in S : \A y \in S : x <= y

VARIABLES
  meta,     \* the meta partition's registry log: [t, g]
  log,      \* lineage -> sequence of entries
  phase,    \* the driver's state
  pending,  \* partitions the current registry change is still to reach
  split,    \* P1 has its split entry (C1 started)
  mapNew,   \* the partition map change is recorded (leader targets C1)
  cursor,   \* backfill cursor per lineage (rows done)
  writes,   \* user writes so far
  base      \* a test base snapshot: [taken, m, st]

vars == <<meta, log, phase, pending, split, mapNew, cursor, writes, base>>

NoG == [L \in Lines |-> 0]
MetaE(t) == [t |-> t, g |-> NoG]

(* Entries. m is the registry context: for a write, its snapshot's        *)
(* registry prefix; for a barrier "b" or the split entry "s", the         *)
(* registry dot it names. ph is a ghost: the driver phase at append.      *)
(* iv is a ghost: the value the writer intended (before put). uq is a     *)
(* ghost: the new head's unique holds over images right after the write.  *)
Wr(r, v, iv, m, by, uq) ==
  [t |-> "w", row |-> r, val |-> v, iv |-> iv, m |-> m, by |-> by,
   ph |-> phase, uq |-> uq]
Bar(t, rr) ==
  [t |-> t, row |-> "none", val |-> 0, iv |-> 0, m |-> rr, by |-> "none",
   ph |-> phase, uq |-> TRUE]

---------------------------------------------------------------------------
(* Registry state at a registry prefix m *)

MetaHasIn(t, m) == \E j \in 1..m : meta[j].t = t
MetaHas(t) == MetaHasIn(t, Len(meta))
MetaIdx(t) == IF MetaHas(t) THEN CHOOSE j \in 1..Len(meta) : meta[j].t = t ELSE 0

StateAt(m) ==
  CASE MetaHasIn("act", m)     -> "act"
    [] MetaHasIn("abandon", m) -> "abandon"
    [] MetaHasIn("dual", m)    -> "dual"
    [] MetaHasIn("prep", m)    -> "prep"
    [] OTHER                   -> "none"

OpenAt(m) == StateAt(m) \in {"prep", "dual"} \/ MetaHasIn("next", m)

---------------------------------------------------------------------------
(* Lineage logs, versions, values *)

LastW(L, n, r) ==
  LET ws == {i \in 1..n : log[L][i].t = "w" /\ log[L][i].row = r}
  IN IF ws = {} THEN 0 ELSE Max(ws)
Val(L, n, r) == LET i == LastW(L, n, r) IN IF i = 0 THEN Init0[r] ELSE log[L][i].val
Intent(L, n, r) == LET i == LastW(L, n, r) IN IF i = 0 THEN Init0[r] ELSE log[L][i].iv
Cur(r) == Val(LineOf(r), Len(log[LineOf(r)]), r)
CurIntent(r) == Intent(LineOf(r), Len(log[LineOf(r)]), r)

SplitPos == IF split THEN CHOOSE i \in 1..Len(log["L1"]) : log["L1"][i].t = "s" ELSE 0
\* first position held by the live partition of L (C1 starts at the split entry)
LivePos(L) == IF L = "L1" /\ split THEN SplitPos ELSE 0

\* prefix n of L is part of a causally closed version whose registry prefix is m
ValidPrefix(L, n, m) ==
  /\ \A i \in 1..n : log[L][i].m <= m
  /\ \A j \in 1..m : meta[j].g[L] <= n

IsBar(e) == e.t \in {"b", "s"}
IsDualBar(e) == IsBar(e) /\ StateAt(e.m) = "dual"
LatestBarrier(L) ==
  LET bs == {log[L][i].m : i \in {k \in 1..Len(log[L]) : IsBar(log[L][k])}}
  IN IF bs = {} THEN 0 ELSE Max(bs)
HasDualBar(L, lo, hi) == \E j \in lo..hi : j >= 1 /\ IsDualBar(log[L][j])
FirstDualPos(L) ==
  LET ps == {j \in 1..Len(log[L]) : IsDualBar(log[L][j])}
  IN IF ps = {} THEN Len(log[L]) ELSE Min(ps)
\* the live partition of L holds the DualWrite barrier within prefix hi
LiveBar(L, hi) ==
  HasDualBar(L, IF L = "L1" /\ split /\ SplitPos <= hi THEN SplitPos ELSE 1, hi)

LiveParts == IF mapNew THEN {"C1", "P2"} ELSE {"P1", "P2"}

BfWritten == \E L \in Lines : \E i \in 1..Len(log[L]) :
               log[L][i].t = "w" /\ log[L][i].by = "bf"

\* the old version's lens: what an old reader sees for stored x at (m, L, n)
UseNewLens(m, L, n) ==
  CASE StateAt(m) = "act"  -> TRUE
    [] StateAt(m) = "dual" -> LensSwitch = "barrier" /\
                              (LensKey = "registry" \/ HasDualBar(L, 1, n))
    [] OTHER               -> FALSE
OldLens(m, L, n, x) == IF UseNewLens(m, L, n) THEN GP(x) ELSE x

UqAfter(r, sv) == InG1(r) => Img(sv) # Img(Cur(Other(r)))

---------------------------------------------------------------------------
Init ==
  /\ meta = <<>>
  /\ log = [L \in Lines |-> <<>>]
  /\ phase = "none"
  /\ pending = {}
  /\ split = FALSE
  /\ mapNew = FALSE
  /\ cursor = [L \in Lines |-> 0]
  /\ writes = 0
  /\ base = [taken |-> FALSE, m |-> 0, st |-> "none"]

(* The driver (meta partition's leader) *)

Register ==
  /\ phase = "none"
  /\ meta' = Append(meta, MetaE("prep"))
  /\ phase' = "prep"
  /\ pending' = LiveParts
  /\ UNCHANGED <<log, split, mapNew, cursor, writes, base>>

\* Deliver the latest registry dot to partition p. A barrier sent to P1
\* after its split entry is re-delivered to the child (§9.6), or dropped.
Deliver(p) ==
  /\ p \in pending
  /\ IF p = "P1" /\ split
       THEN /\ pending' = IF Redeliver THEN (pending \ {"P1"}) \cup {"C1"}
                                       ELSE pending \ {"P1"}
            /\ UNCHANGED log
       ELSE /\ log' = [log EXCEPT ![LineOfPart(p)] = Append(@, Bar("b", Len(meta)))]
            /\ pending' = pending \ {p}
  /\ UNCHANGED <<meta, phase, split, mapNew, cursor, writes, base>>

\* §4.2 item 1: the new unique, checked over the built index (new-head
\* images) once every partition has the Prepared (index) barrier.
PrepCheck ==
  /\ phase = "prep" /\ pending = {}
  /\ IF Img(Cur("r1")) # Img(Cur("r2"))
       THEN /\ phase' = "prepOK"
            /\ UNCHANGED <<meta, pending>>
       ELSE /\ meta' = Append(meta, MetaE("abandon"))
            /\ phase' = "abandon"
            /\ pending' = LiveParts
  /\ UNCHANGED <<log, split, mapNew, cursor, writes, base>>

EnterDual ==
  /\ phase = "prepOK"
  /\ meta' = Append(meta, MetaE("dual"))
  /\ phase' = "dualSent"
  /\ pending' = LiveParts
  /\ UNCHANGED <<log, split, mapNew, cursor, writes, base>>

DualEntered ==
  /\ phase = "dualSent" /\ pending = {}
  /\ phase' = "dual"
  /\ UNCHANGED <<meta, log, pending, split, mapNew, cursor, writes, base>>

\* One backfill step in lineage L: read the cursor's row at a snapshot n;
\* skip it if already valid for the new head, else rewrite it to put(x) as
\* an ordinary mutation (validated, unique-checked, rejected by a child for
\* a snapshot older than its first entry).
BackfillStep(L) ==
  /\ phase = "dual"
  /\ cursor[L] < NRows(L)
  /\ LET r  == RowSeq(L)[cursor[L] + 1]
         lo == IF BackfillSnap = "afterBarrier" THEN FirstDualPos(L) ELSE 0
     IN \E n \in lo..Len(log[L]) :
          LET x == Val(L, n, r)
              sv == Put(x)
              rs == IF InG1(r) /\ UniqueEnforce # "none" THEN {r, Other(r)} ELSE {r}
          IN IF NewValid(x)
               THEN /\ cursor' = [cursor EXCEPT ![L] = @ + 1]
                    /\ UNCHANGED log
               ELSE /\ n >= LivePos(L)
                    /\ BackfillValidate =>
                         \A i \in (n+1)..Len(log[L]) :
                            ~(log[L][i].t = "w" /\ log[L][i].row \in rs)
                    /\ (InG1(r) /\ UniqueEnforce # "none") =>
                         Img(Val(L, n, Other(r))) # sv
                    /\ log' = [log EXCEPT ![L] =
                                 Append(@, Wr(r, sv, CurIntent(r), Len(meta), "bf", UqAfter(r, sv)))]
                    /\ cursor' = [cursor EXCEPT ![L] = @ + 1]
  /\ UNCHANGED <<meta, phase, pending, split, mapNew, writes, base>>

\* §4.2 item 3: at a snapshot G holding every barrier and backfill dot,
\* every value valid for the new head and the set-level constraint holding.
Activate ==
  /\ phase = "dual"
  /\ \A L \in Lines : cursor[L] = NRows(L)
  /\ \A r \in Rows : NewValid(Cur(r))
  /\ ActCheckUnique => Cur("r1") # Cur("r2")
  /\ meta' = Append(meta, [t |-> "act",
                           g |-> IF ActCtx THEN [L \in Lines |-> Len(log[L])] ELSE NoG])
  /\ phase' = "act"
  /\ pending' = LiveParts
  /\ UNCHANGED <<log, split, mapNew, cursor, writes, base>>

Abandon ==
  /\ phase \in {"prep", "prepOK", "dualSent", "dual"}
  /\ AbandonAfterBackfill \/ ~BfWritten
  /\ meta' = Append(meta, MetaE("abandon"))
  /\ phase' = "abandon"
  /\ pending' = LiveParts
  /\ UNCHANGED <<log, split, mapNew, cursor, writes, base>>

\* a newer version registers only once this one is Active or Abandoned
RegisterNext ==
  /\ WithNext
  /\ phase \in {"act", "abandon"} /\ pending = {}
  /\ meta' = Append(meta, MetaE("next"))
  /\ phase' = "next"
  /\ UNCHANGED <<log, pending, split, mapNew, cursor, writes, base>>

(* The placement controller *)

\* drain, split entry, child's first entry (its handoff carries the latest
\* registry dot P1 holds), cursor remap
SplitEntry ==
  /\ WithSplit /\ ~split
  /\ log' = [log EXCEPT !["L1"] = Append(@, Bar("s", LatestBarrier("L1")))]
  /\ split' = TRUE
  /\ cursor' = CASE CursorRemap = "remap" -> cursor
                 [] CursorRemap = "reset" -> [cursor EXCEPT !["L1"] = 0]
                 [] CursorRemap = "lost"  -> [cursor EXCEPT !["L1"] = NRows("L1")]
  /\ UNCHANGED <<meta, phase, pending, mapNew, writes, base>>

MapChange ==
  /\ split /\ ~mapNew
  /\ mapNew' = TRUE
  /\ UNCHANGED <<meta, log, phase, pending, split, cursor, writes, base>>

(* Writers: front ends with registry caches of any staleness *)

UniqueChecked(st) ==
  CASE st = "act"  -> TRUE
    [] st = "dual" -> UniqueEnforce \in {"dual", "index"}
    [] st = "prep" -> UniqueEnforce = "index"
    [] OTHER       -> FALSE

\* a snapshot with registry prefix m can hold prefix n of L
SnapOK(L, n, m) == \A j \in 1..n : log[L][j].m <= m

UserWrite(ver, r, v, m) ==
  /\ writes < MaxWrites
  /\ LET L  == LineOf(r)
         st == StateAt(m)
         sv == CASE st = "dual" -> Put(v)
                 [] st = "act"  -> IF ver = "old" THEN Put(v) ELSE v
                 [] OTHER       -> v
         uc == UniqueChecked(st) /\ InG1(r)
         need == Max({LivePos(L), IF uc THEN LastW(L, Len(log[L]), Other(r)) ELSE 0})
     IN /\ m >= LatestBarrier(L)                          \* registry barrier check
        /\ ver = "new" => st = "act"                      \* not_ready before Active
        /\ v \in (IF ver = "new" THEN NewVals ELSE Vals)
        /\ st = "dual" => (WritePolicy = "putLossy" \/ Put(v) = v)
        /\ uc => Img(sv) # Img(Cur(Other(r)))
        /\ SnapOK(L, need, m)                             \* too_old / contended otherwise
        /\ log' = [log EXCEPT ![L] = Append(@, Wr(r, sv, v, m, "u", UqAfter(r, sv)))]
  /\ writes' = writes + 1
  /\ UNCHANGED <<meta, phase, pending, split, mapNew, cursor, base>>

(* A test base: production's registry as of a version S (registry prefix m) *)
TakeBase ==
  /\ WithBase /\ ~base.taken
  /\ \E m \in 0..Len(meta) :
       /\ BaseCheck => ~OpenAt(m)
       /\ base' = [taken |-> TRUE, m |-> m, st |-> StateAt(m)]
  /\ UNCHANGED <<meta, log, phase, pending, split, mapNew, cursor, writes>>

Next ==
  \/ Register \/ PrepCheck \/ EnterDual \/ DualEntered \/ Activate
  \/ Abandon \/ RegisterNext
  \/ \E p \in Parts : Deliver(p)
  \/ \E L \in Lines : BackfillStep(L)
  \/ SplitEntry \/ MapChange \/ TakeBase
  \/ \E ver \in {"old", "new"}, r \in Rows, v \in Vals, m \in 0..Len(meta) :
       UserWrite(ver, r, v, m)

Fairness ==
  /\ WF_vars(Register) /\ WF_vars(PrepCheck) /\ WF_vars(EnterDual)
  /\ WF_vars(DualEntered) /\ WF_vars(Activate)
  /\ \A p \in Parts : WF_vars(Deliver(p))
  /\ \A L \in Lines : WF_vars(BackfillStep(L))

Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ Fairness

---------------------------------------------------------------------------
(* Invariants *)

TypeOK ==
  /\ phase \in {"none", "prep", "prepOK", "dualSent", "dual", "act", "abandon", "next"}
  /\ pending \subseteq Parts
  /\ writes \in 0..MaxWrites
  /\ \A L \in Lines : cursor[L] \in 0..NRows(L)

\* 1. The stored values of every causally closed V fit the cutover state at
\* V: old head (Prepared, Abandoned); in DualWrite, every row whose last
\* write follows the partition's DualWrite barrier is valid for both heads
\* (rows awaiting backfill are valid for the old head); Active: every value
\* valid for the new head and its unique constraint holding.
ShapeOK(L, n, st) ==
  CASE st = "act" ->
         /\ \A r \in RowsOf(L) : NewValid(Val(L, n, r))
         /\ L = "L1" => Val(L, n, "r1") # Val(L, n, "r2")
    [] st = "dual" ->
         \A r \in RowsOf(L) :
            LET i == LastW(L, n, r) IN
            (i > 0 /\ HasDualBar(L, 1, i - 1)) => NewValid(log[L][i].val)
    [] OTHER -> \A r \in RowsOf(L) : OldValid(Val(L, n, r))

Inv_Shape ==
  \A L \in Lines : \A m \in 0..Len(meta) : \A n \in 0..Len(log[L]) :
     ValidPrefix(L, n, m) => ShapeOK(L, n, StateAt(m))

\* 2. Every entry committed while the cutover is in DualWrite (after its
\* partition's barrier, or after the driver entered DualWrite) satisfies
\* both heads, the new unique included.
Inv_DualWrite ==
  \A L \in Lines : \A i \in 1..Len(log[L]) :
     LET e == log[L][i] IN
     (e.t = "w" /\ (e.ph = "dual" \/ (e.ph = "dualSent" /\ HasDualBar(L, 1, i - 1))))
       => (NewValid(e.val) /\ OldValid(e.val) /\ e.uq)

\* 3. An old reader's view at V changes only through a stamped write to the
\* row or a registry dot that freshness checks and topics count.
LensMetaOK ==
  \A k \in 1..Len(meta) :
     meta[k].t \notin CountDots =>
       \A L \in Lines : \A n \in 0..Len(log[L]) :
          (ValidPrefix(L, n, k - 1) /\ meta[k].g[L] <= n) =>
             \A r \in RowsOf(L) :
                OldLens(k - 1, L, n, Val(L, n, r)) = OldLens(k, L, n, Val(L, n, r))
LensDataOK ==
  \A L \in Lines : \A j \in 1..Len(log[L]) :
     IsBar(log[L][j]) =>
       \A m \in 0..Len(meta) :
          ValidPrefix(L, j, m) =>
             \A r \in RowsOf(L) :
                OldLens(m, L, j - 1, Val(L, j, r)) = OldLens(m, L, j, Val(L, j, r))
Inv_Lens == LensMetaOK /\ LensDataOK

\* 4. Active is entered only at a snapshot G holding every DualWrite barrier
\* (in the live partitions, children included) and every backfill dot, with
\* every value valid for the new head and the unique constraint holding.
Inv_ActivateGuard ==
  \A a \in 1..Len(meta) :
     meta[a].t = "act" =>
       LET G == meta[a].g IN
       /\ \A L \in Lines : LiveBar(L, G[L])
       /\ \A L \in Lines : \A i \in 1..Len(log[L]) :
            (log[L][i].t = "w" /\ log[L][i].by = "bf") => i <= G[L]
       /\ \A r \in Rows : NewValid(Val(LineOf(r), G[LineOf(r)], r))
       /\ Val("L1", G["L1"], "r1") # Val("L1", G["L1"], "r2")

\* 5. Abandoned is never entered after the first backfill write.
Inv_NoReturn == MetaHas("abandon") => ~BfWritten

\* 6. At most one open cutover per schema.
Inv_One ==
  (IF StateAt(Len(meta)) \in {"prep", "dual"} THEN 1 ELSE 0)
  + (IF MetaHas("next") THEN 1 ELSE 0) <= 1

\* Split x cutover: once DualWrite is entered, the live partition of every
\* lineage (a child created after the barrier was sent included) holds the
\* barrier; and every row behind a backfill cursor is valid for the new head.
Inv_BarrierChild == phase = "dual" => \A L \in Lines : LiveBar(L, Len(log[L]))
Inv_Cursor ==
  phase \in {"dual", "act"} =>
     \A L \in Lines : \A k \in 1..cursor[L] : NewValid(Cur(RowSeq(L)[k]))

\* Base x open cutover: a test base never holds an open cutover ...
Inv_Base == base.taken => ~OpenAt(base.m)
\* ... because the test universe would run it to Active synchronously
\* (§4.2 "Dev and test universes"), and production may still abandon it.
Inv_BaseAgree ==
  (base.taken /\ base.st \in {"prep", "dual"}) => ~MetaHas("abandon")

\* Supporting invariants
\* a backfill rewrite replaces exactly the value it read (no lost update)
Inv_NoLostUpdate ==
  \A L \in Lines : \A i \in 1..Len(log[L]) :
     (log[L][i].t = "w" /\ log[L][i].by = "bf")
        => log[L][i].val = Put(Val(L, i - 1, log[L][i].row))
\* abandoning loses nothing: every value is what its last writer wrote
Inv_AbandonLossless == MetaHas("abandon") => \A r \in Rows : Cur(r) = CurIntent(r)
\* once in DualWrite, the new unique holds over images, so backfill can finish
Inv_SetFeasible ==
  phase \in {"dualSent", "dual"} => Img(Cur("r1")) # Img(Cur("r2"))
\* "a row read before and after its backfill looks the same" (§4.2)
Inv_BackfillQuiet ==
  \A L \in Lines : \A i \in 1..Len(log[L]) :
     (log[L][i].t = "w" /\ log[L][i].by = "bf") =>
        \A m \in 0..Len(meta) :
           ValidPrefix(L, i, m) =>
              OldLens(m, L, i - 1, Val(L, i - 1, log[L][i].row))
                = OldLens(m, L, i, log[L][i].val)

(* Liveness *)
\* 7. Under fairness and bounded contention, backfill completes.
Live_Backfill == BfWritten ~> MetaHas("act")
Live_Done == <>(MetaHas("act") \/ MetaHas("abandon"))

=============================================================================
