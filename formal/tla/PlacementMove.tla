--------------------------- MODULE PlacementMove ---------------------------
(***************************************************************************)
(* Shep §9.6 (placement moves), §9.5 (contexts), §10.3 (writing), §3      *)
(* (`placement`, `placement_owner`).                                       *)
(*                                                                         *)
(* Two PG nodes S and T. One placement group G, held by S at the start,    *)
(* with two kinds of fact:                                                 *)
(*   - the root node c with one field, <<"c", v>> for v in {0, 1}          *)
(*     (created by the initial log entry S1 with c = 0);                   *)
(*   - a node m placed with c, <<"m", 1>> while it exists. Creating m      *)
(*     writes its directory entry first (on the directory primary), then   *)
(*     runs m's own transaction on G's PG node.                            *)
(*                                                                         *)
(* Clients (each is a Shep instance with its own cached PG node map) run   *)
(* a fixed list of operations. A write transaction (§10.3) on node x:      *)
(*   WBegin  : snapshot (its version = x's committed dots) and the         *)
(*             snapshot's view of x's ownership row                        *)
(*   WLock   : SELECT ... FOR SHARE on the ownership row (ForShare). It    *)
(*             waits behind an uncommitted UPDATE of the row; if the row   *)
(*             changed since the snapshot, Postgres raises a serialization *)
(*             failure; if x does not hold G active, the write fails with  *)
(*             `moved` and the instance refreshes its cache.               *)
(*   WCommit : row conflicts (first committer wins), then the data change, *)
(*             the dot and the log row (dot and log row are atomic with    *)
(*             the commit here; DotFrontier.tla covers holes).             *)
(*                                                                         *)
(* The move worker (one, elected): seal on S (begin = snapshot, UPDATE     *)
(* waits for FOR SHARE holders, commit = logged seal entry), copy S's rows *)
(* to T (no facts logged), activate on T (logged), switch the PG node map  *)
(* (`dir`), clean up S. It can crash at every step and then resumes from   *)
(* the database state, or (before activation) rolls back: unseal S         *)
(* (logged) and drop T's copy.                                             *)
(*                                                                         *)
(* A log reader applies committed entries in causal order (an entry once   *)
(* its whole context is applied). A reader takes routed single-node reads  *)
(* and fan-out reads (one snapshot per node, in either order). A sweeper   *)
(* removes orphan directory entries.                                       *)
(*                                                                         *)
(* Facts are sets (rows); log deltas are Z-sets {<<fact, +1/-1>>}.         *)
(***************************************************************************)
EXTENDS Naturals, Integers, FiniteSets, Sequences, TLC

CONSTANTS
  S, T,            \* the two PG nodes (model values)
  Ops,             \* client -> sequence of ops in {"set0", "set1", "create"}
  MaxDot,          \* per-node counter bound
  ForShare,        \* TRUE: writes read the ownership row FOR SHARE (design)
  ActCtx,          \* activation context: "seal" (design: seal dot + T's
                   \*   snapshot), "copy" (fix: the copy's snapshot version
                   \*   of S, which contains the seal), "none" (no seal dot)
  SealCtxLate,     \* TRUE: alternative fix: the seal captures its version
                   \*   after its UPDATE of the ownership row returns
  CopyLogsFacts,   \* TRUE: the copy logs +1 facts on T (negative)
  CleanupLogged,   \* TRUE: S's cleanup commits an empty logged entry whose
                   \*   context names the activation dot
  ReadRule,        \* "design": reads don't look at ownership; fan-out reads
                   \*   roll back to the closed core (F6)
                   \* "owner": reads honour ownership rows (moved / pick the
                   \*   holder); fan-out still rolls back to the core
                   \* "fixed": as owner, but a fan-out read whose merged
                   \*   version is not closed re-takes the lagging snapshot
  SweepMode,       \* "none" | "naive" (check absent, then delete) |
                   \*   "deadline" (create deadline + generation-checked delete)
  SweepNode,       \* where the sweeper checks for the node: "cache" (its
                   \*   cached PG node, no ownership check), "held" (active or
                   \*   sealed holder), "active" (active holder only)
  MaxCrashes,      \* bound on move-worker crashes
  AllowRollback,   \* the worker may roll a move back before activation
  MaxAttempts,     \* bound on move attempts
  MaxGen,          \* bound on directory-entry writes (create retries)
  WithLogReader,   \* include the causal log reader
  WithReader       \* include the reader (routed + fan-out reads)

Nodes == {S, T}
Other(x) == IF x = S THEN T ELSE S
Clients == DOMAIN Ops
Dots == Nodes \X (1..MaxDot)

CF(v) == <<"c", v>>
MF == <<"m", 1>>
AllFacts == {CF(0), CF(1), MF}
OpKeys(op) == IF op = "create" THEN {"m"} ELSE {"c"}

NoEntry == [kind |-> "none", ctx |-> {}, delta |-> {}, keys |-> {}]
Own(st, v) == [st |-> st, v |-> v]

VARIABLES
  own,       \* own[x]: x's placement_owner row for G: st in {none, active, sealed}, v = row version
  rows,      \* rows[x]: G's facts stored on x
  rgen,      \* rgen[x]: bumped by non-logged bulk changes to x's rows (copy, cleanup)
  log, ctr,  \* log[d]: committed entry for dot d (atomic with commit); ctr[x]: counter
  shareH,    \* shareH[x]: clients holding FOR SHARE on x's ownership row
  updL,      \* updL[x]: an uncommitted UPDATE (the seal) holds x's ownership row
  dir,       \* the PG node map entry for G (authoritative)
  cpc, opi, cache, cnode, csnap, cown, crg, cgen,   \* clients
  acked,     \* dots of writes acknowledged to clients
  writeOK,   \* history: every write committed while its node held G active
  wpc, wsnap, wcopy, crashes, attempts,             \* move worker
  entry, gctr, exp,                                 \* m's directory entry
  spc, sg0, scache,                                 \* sweeper
  rpc, rs, rfirst, rcache, readOK,                  \* reader
  applied                                           \* causal log reader

vars == <<own, rows, rgen, log, ctr, shareH, updL, dir, cpc, opi, cache, cnode,
          csnap, cown, crg, cgen, acked, writeOK, wpc, wsnap, wcopy, crashes,
          attempts, entry, gctr, exp, spc, sg0, scache, rpc, rs, rfirst, rcache,
          readOK, applied>>

-----------------------------------------------------------------------------
(* helpers *)

Committed(x) == {d \in Dots : d[1] = x /\ log[d].kind /= "none"}
AllCommitted == Committed(S) \cup Committed(T)

WIn(d, f) == IF <<f, 1>> \in log[d].delta THEN 1
             ELSE IF <<f, -1>> \in log[d].delta THEN -1 ELSE 0
RECURSIVE SumOver(_, _)
SumOver(D, f) == IF D = {} THEN 0
                 ELSE LET d == CHOOSE e \in D : TRUE IN WIn(d, f) + SumOver(D \ {d}, f)
Ind(f, R) == IF f \in R THEN 1 ELSE 0

RECURSIVE Clo(_)
Clo(D) == LET D2 == D \cup UNION {log[d].ctx : d \in D}
          IN IF D2 = D THEN D ELSE Clo(D2)
Closed(D) == \A d \in D : log[d].ctx \subseteq D
RECURSIVE Core(_)
Core(D) == LET D2 == {d \in D : log[d].ctx \subseteq D}
           IN IF D2 = D THEN D ELSE Core(D2)

\* the node whose copy of G is authoritative
AuthNode == IF own[T].st = "active" THEN T
            ELSE IF own[S].st \in {"active", "sealed"} THEN S
            ELSE IF own[T].st = "sealed" THEN T ELSE S
AuthRows == rows[AuthNode]

KindDots(x, k) == {d \in Committed(x) : log[d].kind = k}
LastOf(D) == CHOOSE d \in D : \A e \in D : e[2] <= d[2]

ApplyDelta(R, delta) ==
  (R \ {f \in AllFacts : <<f, -1>> \in delta}) \cup {f \in AllFacts : <<f, 1>> \in delta}

CurC(R) == {v \in {0, 1} : CF(v) \in R}

Op(i) == Ops[i][opi[i]]
PcFor(i, k) == IF k > Len(Ops[i]) THEN "done"
               ELSE IF Ops[i][k] = "create" THEN "c1" ELSE "begin"

Snap(x) == [ver |-> Committed(x), st |-> own[x].st, rows |-> rows[x]]

-----------------------------------------------------------------------------
Init ==
  /\ own = [x \in Nodes |-> IF x = S THEN Own("active", 0) ELSE Own("none", 0)]
  /\ rows = [x \in Nodes |-> IF x = S THEN {CF(0)} ELSE {}]
  /\ rgen = [x \in Nodes |-> 0]
  /\ log = [d \in Dots |-> IF d = <<S, 1>>
                             THEN [kind |-> "write", ctx |-> {}, delta |-> {<<CF(0), 1>>}, keys |-> {"c"}]
                             ELSE NoEntry]
  /\ ctr = [x \in Nodes |-> IF x = S THEN 1 ELSE 0]
  /\ shareH = [x \in Nodes |-> {}]
  /\ updL = [x \in Nodes |-> FALSE]
  /\ dir = S
  /\ cpc = [i \in Clients |-> PcFor(i, 1)]
  /\ opi = [i \in Clients |-> 1]
  /\ cache = [i \in Clients |-> S]
  /\ cnode = [i \in Clients |-> S]
  /\ csnap = [i \in Clients |-> {}]
  /\ cown = [i \in Clients |-> Own("none", 0)]
  /\ crg = [i \in Clients |-> 0]
  /\ cgen = [i \in Clients |-> 0]
  /\ acked = {}
  /\ writeOK = TRUE
  /\ wpc = "idle"
  /\ wsnap = {}
  /\ wcopy = {}
  /\ crashes = 0
  /\ attempts = 0
  /\ entry = [present |-> FALSE, gen |-> 0]
  /\ gctr = 0
  /\ exp = 0
  /\ spc = "idle"
  /\ sg0 = 0
  /\ scache = S
  /\ rpc = "idle"
  /\ rs = [x \in Nodes |-> [ver |-> {}, st |-> "none", rows |-> {}]]
  /\ rfirst = S
  /\ rcache = S
  /\ readOK = TRUE
  /\ applied = {}

-----------------------------------------------------------------------------
(* clients *)

ClientVars == <<cpc, opi, cache, cnode, csnap, cown, crg, cgen>>
OtherVars0 == <<wpc, wsnap, wcopy, crashes, attempts, spc, sg0, scache,
                rpc, rs, rfirst, rcache, readOK, applied>>

\* create, step 1: write the directory entry (idempotent upsert; each write
\* gets a new generation, which also carries a new create deadline)
C1(i) ==
  /\ cpc[i] = "c1"
  /\ gctr < MaxGen
  /\ gctr' = gctr + 1
  /\ entry' = [present |-> TRUE, gen |-> gctr + 1]
  /\ cgen' = [cgen EXCEPT ![i] = gctr + 1]
  /\ cpc' = [cpc EXCEPT ![i] = "begin"]
  /\ UNCHANGED <<own, rows, rgen, log, ctr, shareH, updL, dir, opi, cache, cnode,
                 csnap, cown, crg, acked, writeOK, exp>>
  /\ UNCHANGED OtherVars0

\* a create gives up after writing its entry: an orphan entry
Abandon(i) ==
  /\ cpc[i] = "begin"
  /\ Op(i) = "create"
  /\ opi' = [opi EXCEPT ![i] = opi[i] + 1]
  /\ cpc' = [cpc EXCEPT ![i] = PcFor(i, opi[i] + 1)]
  /\ UNCHANGED <<own, rows, rgen, log, ctr, shareH, updL, dir, cache, cnode, csnap,
                 cown, crg, cgen, acked, writeOK, entry, gctr, exp>>
  /\ UNCHANGED OtherVars0

WBegin(i) ==
  /\ cpc[i] = "begin"
  /\ LET x == cache[i] IN
       /\ cnode' = [cnode EXCEPT ![i] = x]
       /\ csnap' = [csnap EXCEPT ![i] = Committed(x)]
       /\ cown' = [cown EXCEPT ![i] = own[x]]
       /\ crg' = [crg EXCEPT ![i] = rgen[x]]
  /\ cpc' = [cpc EXCEPT ![i] = "lock"]
  /\ UNCHANGED <<own, rows, rgen, log, ctr, shareH, updL, dir, opi, cache, cgen,
                 acked, writeOK, entry, gctr, exp>>
  /\ UNCHANGED OtherVars0

Moved(i) ==
  /\ cache' = [cache EXCEPT ![i] = dir]
  /\ cpc' = [cpc EXCEPT ![i] = "begin"]

WLock(i) ==
  LET x == cnode[i] IN
  /\ cpc[i] = "lock"
  /\ IF ForShare
       THEN /\ ~ updL[x]                                   \* waits behind the seal's UPDATE
            /\ IF own[x] /= cown[i]
                 THEN \* serialization failure (row changed after the snapshot): retry
                      /\ cpc' = [cpc EXCEPT ![i] = "begin"]
                      /\ UNCHANGED <<cache, shareH>>
                 ELSE IF own[x].st /= "active"
                 THEN Moved(i) /\ UNCHANGED shareH
                 ELSE /\ shareH' = [shareH EXCEPT ![x] = shareH[x] \cup {i}]
                      /\ cpc' = [cpc EXCEPT ![i] = "commit"]
                      /\ UNCHANGED cache
       ELSE \* negative: a plain read of the row in the write's snapshot
            /\ UNCHANGED shareH
            /\ IF cown[i].st /= "active"
                 THEN Moved(i)
                 ELSE cpc' = [cpc EXCEPT ![i] = "commit"] /\ UNCHANGED cache
  /\ UNCHANGED <<own, rows, rgen, log, ctr, updL, dir, opi, cnode, csnap, cown, crg,
                 cgen, acked, writeOK, entry, gctr, exp>>
  /\ UNCHANGED OtherVars0

WriteDelta(op, R) ==
  IF op = "create"
    THEN IF MF \in R THEN {} ELSE {<<MF, 1>>}
    ELSE LET v == IF op = "set1" THEN 1 ELSE 0
             cur == CHOOSE w \in CurC(R) : TRUE
         IN IF cur = v THEN {} ELSE {<<CF(cur), -1>>, <<CF(v), 1>>}

WCommit(i) ==
  LET x == cnode[i]
      op == Op(i)
      conflict == \/ \E d \in Committed(x) \ csnap[i] : log[d].keys \cap OpKeys(op) /= {}
                  \/ rgen[x] /= crg[i]
                  \/ (op /= "create" /\ CurC(rows[x]) = {})
      expired == op = "create" /\ SweepMode = "deadline" /\ cgen[i] <= exp
  IN
  /\ cpc[i] = "commit"
  /\ shareH' = [shareH EXCEPT ![x] = shareH[x] \ {i}]
  /\ IF conflict
       THEN \* serialization failure / row conflict: retry the transaction
            /\ cpc' = [cpc EXCEPT ![i] = "begin"]
            /\ UNCHANGED <<rows, log, ctr, opi, acked, writeOK>>
       ELSE IF expired
       THEN \* the create's deadline has passed: rewrite the entry first
            /\ cpc' = [cpc EXCEPT ![i] = "c1"]
            /\ UNCHANGED <<rows, log, ctr, opi, acked, writeOK>>
       ELSE LET n == ctr[x] + 1
                d == <<x, n>>
                delta == WriteDelta(op, rows[x])
            IN /\ n <= MaxDot
               /\ ctr' = [ctr EXCEPT ![x] = n]
               /\ log' = [log EXCEPT ![d] = [kind |-> "write", ctx |-> csnap[i],
                                              delta |-> delta, keys |-> OpKeys(op)]]
               /\ rows' = [rows EXCEPT ![x] = ApplyDelta(rows[x], delta)]
               /\ acked' = acked \cup {d}
               /\ writeOK' = (writeOK /\ own[x].st = "active")
               /\ opi' = [opi EXCEPT ![i] = opi[i] + 1]
               /\ cpc' = [cpc EXCEPT ![i] = PcFor(i, opi[i] + 1)]
  /\ UNCHANGED <<own, rgen, updL, dir, cache, cnode, csnap, cown, crg, cgen,
                 entry, gctr, exp>>
  /\ UNCHANGED OtherVars0

\* the instance dies, or the transaction aborts: Postgres rolls it back;
\* the client library retries the request
WAbort(i) ==
  /\ cpc[i] \in {"lock", "commit"}
  /\ shareH' = [shareH EXCEPT ![cnode[i]] = shareH[cnode[i]] \ {i}]
  /\ cpc' = [cpc EXCEPT ![i] = "begin"]
  /\ UNCHANGED <<own, rows, rgen, log, ctr, updL, dir, opi, cache, cnode, csnap, cown,
                 crg, cgen, acked, writeOK, entry, gctr, exp>>
  /\ UNCHANGED OtherVars0

-----------------------------------------------------------------------------
(* the move worker *)

ClientAll == <<cpc, opi, cache, cnode, csnap, cown, crg, cgen, acked, writeOK>>
WOther == <<entry, gctr, exp, spc, sg0, scache, rpc, rs, rfirst, rcache, readOK, applied>>

\* seal transaction begins: its snapshot
SealBegin ==
  /\ wpc = "idle"
  /\ attempts < MaxAttempts
  /\ own[S].st = "active"
  /\ attempts' = attempts + 1
  /\ wsnap' = Committed(S)
  /\ wpc' = "sealLock"
  /\ UNCHANGED <<own, rows, rgen, log, ctr, shareH, updL, dir, wcopy, crashes>>
  /\ UNCHANGED ClientAll /\ UNCHANGED WOther

\* UPDATE placement_owner: waits for every FOR SHARE holder to finish.
\* A FOR SHARE lock held by a transaction that has since committed doesn't
\* make the UPDATE fail ("not just locked it" in the Postgres docs).
SealLock ==
  /\ wpc = "sealLock"
  /\ shareH[S] = {}
  /\ updL' = [updL EXCEPT ![S] = TRUE]
  /\ wpc' = "sealCommit"
  /\ UNCHANGED <<own, rows, rgen, log, ctr, shareH, dir, wsnap, wcopy, crashes, attempts>>
  /\ UNCHANGED ClientAll /\ UNCHANGED WOther

SealCommit ==
  LET n == ctr[S] + 1 IN
  /\ wpc = "sealCommit"
  /\ n <= MaxDot
  /\ ctr' = [ctr EXCEPT ![S] = n]
  /\ log' = [log EXCEPT ![<<S, n>>] =
               [kind |-> "seal", ctx |-> IF SealCtxLate THEN Committed(S) ELSE wsnap,
                delta |-> {}, keys |-> {}]]
  /\ own' = [own EXCEPT ![S] = Own("sealed", own[S].v + 1)]
  /\ updL' = [updL EXCEPT ![S] = FALSE]
  /\ wpc' = "copy"
  /\ UNCHANGED <<rows, rgen, shareH, dir, wsnap, wcopy, crashes, attempts>>
  /\ UNCHANGED ClientAll /\ UNCHANGED WOther

\* copy G's rows from S (read at a snapshot after the seal) to T; no facts logged
Copy ==
  /\ wpc = "copy"
  /\ own[S].st = "sealed"
  /\ rows' = [rows EXCEPT ![T] = rows[S]]
  /\ rgen' = [rgen EXCEPT ![T] = rgen[T] + 1]
  /\ wcopy' = Committed(S)
  /\ IF CopyLogsFacts
       THEN LET n == ctr[T] + 1 IN
            /\ n <= MaxDot
            /\ ctr' = [ctr EXCEPT ![T] = n]
            /\ log' = [log EXCEPT ![<<T, n>>] =
                         [kind |-> "copy", ctx |-> Committed(S),
                          delta |-> {<<f, 1>> : f \in rows[S]}, keys |-> {"c", "m"}]]
       ELSE UNCHANGED <<ctr, log>>
  /\ wpc' = "activate"
  /\ UNCHANGED <<own, shareH, updL, dir, wsnap, crashes, attempts>>
  /\ UNCHANGED ClientAll /\ UNCHANGED WOther

Activate ==
  LET n == ctr[T] + 1
      sealDot == LastOf(KindDots(S, "seal"))
      ctx == CASE ActCtx = "seal" -> {sealDot} \cup Committed(T)
               [] ActCtx = "copy" -> wcopy \cup Committed(T)
               [] ActCtx = "none" -> Committed(T)
  IN
  /\ wpc = "activate"
  /\ n <= MaxDot
  /\ ctr' = [ctr EXCEPT ![T] = n]
  /\ log' = [log EXCEPT ![<<T, n>>] = [kind |-> "activate", ctx |-> ctx, delta |-> {}, keys |-> {}]]
  /\ own' = [own EXCEPT ![T] = Own("active", own[T].v + 1)]
  /\ wpc' = "switch"
  /\ UNCHANGED <<rows, rgen, shareH, updL, dir, wsnap, wcopy, crashes, attempts>>
  /\ UNCHANGED ClientAll /\ UNCHANGED WOther

Switch ==
  /\ wpc = "switch"
  /\ dir' = T
  /\ wpc' = "cleanup"
  /\ UNCHANGED <<own, rows, rgen, log, ctr, shareH, updL, wsnap, wcopy, crashes, attempts>>
  /\ UNCHANGED ClientAll /\ UNCHANGED WOther

Cleanup ==
  /\ wpc = "cleanup"
  /\ rows' = [rows EXCEPT ![S] = {}]
  /\ rgen' = [rgen EXCEPT ![S] = rgen[S] + 1]
  /\ own' = [own EXCEPT ![S] = Own("none", own[S].v + 1)]
  /\ IF CleanupLogged
       THEN LET n == ctr[S] + 1 IN
            /\ n <= MaxDot
            /\ ctr' = [ctr EXCEPT ![S] = n]
            /\ log' = [log EXCEPT ![<<S, n>>] =
                         [kind |-> "cleanup",
                          ctx |-> Committed(S) \cup KindDots(T, "activate"),
                          delta |-> {}, keys |-> {}]]
       ELSE UNCHANGED <<ctr, log>>
  /\ wpc' = "done"
  /\ UNCHANGED <<shareH, updL, dir, wsnap, wcopy, crashes, attempts>>
  /\ UNCHANGED ClientAll /\ UNCHANGED WOther

\* the worker dies in the middle of a step: that step's transaction rolls back
WCrash ==
  /\ wpc \in {"sealLock", "sealCommit", "copy", "activate", "switch", "cleanup"}
  /\ crashes < MaxCrashes
  /\ crashes' = crashes + 1
  /\ updL' = [updL EXCEPT ![S] = FALSE]
  /\ wpc' = "crashed"
  /\ UNCHANGED <<own, rows, rgen, log, ctr, shareH, dir, wsnap, wcopy, attempts>>
  /\ UNCHANGED ClientAll /\ UNCHANGED WOther

\* a new worker resumes from what the databases say
Recover ==
  /\ wpc = "crashed"
  /\ wpc' = IF own[T].st = "active" THEN (IF dir = T THEN "cleanup" ELSE "switch")
            ELSE IF own[S].st = "sealed" THEN "copy"
            ELSE "idle"
  /\ UNCHANGED <<own, rows, rgen, log, ctr, shareH, updL, dir, wsnap, wcopy, crashes, attempts>>
  /\ UNCHANGED ClientAll /\ UNCHANGED WOther

\* roll the move back: only before T is active
Rollback ==
  LET n == ctr[S] + 1 IN
  /\ AllowRollback
  /\ wpc \in {"crashed", "copy", "activate"}
  /\ own[S].st = "sealed"
  /\ own[T].st /= "active"
  /\ n <= MaxDot
  /\ rows' = [rows EXCEPT ![T] = {}]
  /\ rgen' = [rgen EXCEPT ![T] = rgen[T] + 1]
  /\ own' = [own EXCEPT ![S] = Own("active", own[S].v + 1)]
  /\ ctr' = [ctr EXCEPT ![S] = n]
  /\ log' = [log EXCEPT ![<<S, n>>] = [kind |-> "unseal", ctx |-> Committed(S),
                                        delta |-> {}, keys |-> {}]]
  /\ wpc' = "idle"
  /\ UNCHANGED <<shareH, updL, dir, wsnap, wcopy, crashes, attempts>>
  /\ UNCHANGED ClientAll /\ UNCHANGED WOther

WorkerStep == SealBegin \/ SealLock \/ SealCommit \/ Copy \/ Activate \/ Switch
              \/ Cleanup \/ Recover

-----------------------------------------------------------------------------
(* the orphan sweeper *)

SOther == <<own, rows, rgen, log, ctr, shareH, updL, dir, wpc, wsnap, wcopy, crashes,
            attempts, rpc, rs, rfirst, rcache, readOK, applied>>

\* time passes: the deadline of the oldest still-open generation expires
Expire ==
  /\ SweepMode = "deadline"
  /\ exp < gctr
  /\ exp' = exp + 1
  /\ UNCHANGED <<entry, gctr, spc, sg0, scache>> /\ UNCHANGED ClientAll /\ UNCHANGED SOther

\* look for m on G's PG node; a "moved" answer refreshes the sweeper's cache
SweepCheck ==
  LET x == scache
      usable == CASE SweepNode = "cache"  -> TRUE
                  [] SweepNode = "held"   -> own[x].st \in {"active", "sealed"}
                  [] SweepNode = "active" -> own[x].st = "active"
  IN
  /\ SweepMode /= "none"
  /\ spc = "idle"
  /\ entry.present
  /\ SweepMode = "deadline" => entry.gen <= exp
  /\ IF ~ usable
       THEN /\ scache /= dir
            /\ scache' = dir
            /\ UNCHANGED <<spc, sg0>>
       ELSE /\ MF \notin rows[x]
            /\ spc' = "del"
            /\ sg0' = entry.gen
            /\ UNCHANGED scache
  /\ UNCHANGED <<entry, gctr, exp>> /\ UNCHANGED ClientAll /\ UNCHANGED SOther

SweepDel ==
  /\ spc = "del"
  /\ entry' = IF SweepMode = "naive" \/ entry.gen = sg0
                THEN [entry EXCEPT !.present = FALSE] ELSE entry
  /\ spc' = "idle"
  /\ UNCHANGED <<gctr, exp, sg0, scache>> /\ UNCHANGED ClientAll /\ UNCHANGED SOther

\* the sweeper's cached PG node map is refreshed at some point
SweepRefresh ==
  /\ SweepMode /= "none"
  /\ scache /= dir
  /\ scache' = dir
  /\ UNCHANGED <<entry, gctr, exp, spc, sg0>> /\ UNCHANGED ClientAll /\ UNCHANGED SOther

-----------------------------------------------------------------------------
(* the reader *)

ROther == <<own, rows, rgen, log, ctr, shareH, updL, dir, wpc, wsnap, wcopy, crashes,
            attempts, entry, gctr, exp, spc, sg0, scache, applied>>

\* what a read returns is right when: the weight of each fact equals the
\* replay of the read's version, and G (its root c) appears exactly once
ReadRight(data, ver) ==
  /\ \A f \in AllFacts : data[f] = SumOver(ver, f)
  /\ data[CF(0)] + data[CF(1)] = 1

\* a routed read on the node the reader's cache names. Its version is that
\* node's snapshot; a single-node version is not closed (F12), so the data
\* must match the replay of its closure.
RoutedRead ==
  LET x == rcache IN
  /\ WithReader
  /\ rpc = "idle"
  /\ IF ReadRule /= "design" /\ own[x].st = "none"
       THEN /\ rcache /= dir
            /\ rcache' = dir
            /\ UNCHANGED readOK
       ELSE /\ readOK' = (readOK /\ ReadRight([f \in AllFacts |-> Ind(f, rows[x])],
                                              Clo(Committed(x))))
            /\ UNCHANGED rcache
  /\ UNCHANGED <<rpc, rs, rfirst>> /\ UNCHANGED ClientAll /\ UNCHANGED ROther

FanFirst(x) ==
  /\ WithReader
  /\ rpc = "idle"
  /\ rs' = [rs EXCEPT ![x] = Snap(x)]
  /\ rfirst' = x
  /\ rpc' = "second"
  /\ UNCHANGED <<rcache, readOK>> /\ UNCHANGED ClientAll /\ UNCHANGED ROther

FanSecond ==
  /\ rpc = "second"
  /\ rs' = [rs EXCEPT ![Other(rfirst)] = Snap(Other(rfirst))]
  /\ rpc' = "eval"
  /\ UNCHANGED <<rfirst, rcache, readOK>> /\ UNCHANGED ClientAll /\ UNCHANGED ROther

Merged == rs[S].ver \cup rs[T].ver

\* which snapshot's copy of G a read uses, by the ownership rows it saw
Holder == IF rs[T].st = "active" THEN {T}
          ELSE IF rs[S].st \in {"active", "sealed"} THEN {S}
          ELSE IF rs[T].st = "sealed" THEN {T} ELSE {}

FanResult ==
  LET ver == Core(Merged)
      dropped == Merged \ ver
      src == IF ReadRule = "design" THEN Nodes ELSE Holder
      data == [f \in AllFacts |->
                 Cardinality({x \in src : f \in rs[x].rows})
                 - SumOver(dropped, f)]
  IN ReadRight(data, ver)

FanEval ==
  /\ rpc = "eval"
  /\ IF ReadRule = "fixed" /\ ~ Closed(Merged)
       THEN \* roll forward: re-take a snapshot whose node has missing dots
            \E y \in Nodes :
              /\ \E d \in UNION {log[e].ctx : e \in Merged} \ Merged : d[1] = y
              /\ rs' = [rs EXCEPT ![y] = Snap(y)]
              /\ UNCHANGED <<rpc, readOK>>
       ELSE /\ readOK' = (readOK /\ FanResult)
            /\ rpc' = "idle"
            /\ UNCHANGED rs
  /\ UNCHANGED <<rfirst, rcache>> /\ UNCHANGED ClientAll /\ UNCHANGED ROther

-----------------------------------------------------------------------------
(* the causal log reader *)

LRApply ==
  /\ WithLogReader
  /\ \E d \in AllCommitted \ applied :
       /\ log[d].ctx \subseteq applied
       /\ applied' = applied \cup {d}
  /\ UNCHANGED <<own, rows, rgen, log, ctr, shareH, updL, dir, wpc, wsnap, wcopy, crashes,
                 attempts, entry, gctr, exp, spc, sg0, scache, rpc, rs, rfirst, rcache, readOK>>
  /\ UNCHANGED ClientAll

-----------------------------------------------------------------------------
ClientStep(i) == C1(i) \/ WBegin(i) \/ WLock(i) \/ WCommit(i)

Next ==
  \/ \E i \in Clients : ClientStep(i) \/ Abandon(i) \/ WAbort(i)
  \/ WorkerStep \/ WCrash \/ Rollback
  \/ Expire \/ SweepCheck \/ SweepDel \/ SweepRefresh
  \/ RoutedRead \/ (\E x \in Nodes : FanFirst(x)) \/ FanSecond \/ FanEval
  \/ LRApply

Fairness ==
  /\ \A i \in Clients : WF_vars(ClientStep(i))
  /\ WF_vars(WorkerStep)

Spec == Init /\ [][Next]_vars
LiveSpec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
(* properties *)

\* 1. No lost writes: every acknowledged write is in the log, and replaying
\*    every committed entry gives exactly G's authoritative rows (so every
\*    committed write's effect is there, and nothing is counted twice).
AckedInLog == \A d \in acked : log[d].kind = "write"
ReplayMatches == \A f \in AllFacts : SumOver(AllCommitted, f) = Ind(f, AuthRows)
NoLostWrites == AckedInLog /\ ReplayMatches

\* 2. Single writer: at most one PG node holds G active, and every write
\*    committed while its node held G active (so writes on S precede the seal
\*    and writes on T follow the activation, in commit order).
SingleWriter ==
  /\ Cardinality({x \in Nodes : own[x].st = "active"}) <= 1
  /\ writeOK

\* 3. Causal replay: applying entries in causal order never leaves a weight
\*    outside {0, 1} (no -1 without its +1, no double count).
CausalReplay == \A f \in AllFacts : SumOver(applied, f) \in {0, 1}

\* 4. Reads by version: every read saw G exactly once, matching its version.
ReadsByVersion == readOK

\* 5. Orphans. A lookup of m goes through its directory entry to G's rows.
LookupVisible == entry.present /\ MF \in AuthRows
OrphanInvisible == (entry.present /\ MF \notin AuthRows) => ~ LookupVisible
LiveHasEntry == MF \in AuthRows => entry.present
Orphans == OrphanInvisible /\ LiveHasEntry

Safety == NoLostWrites /\ SingleWriter /\ CausalReplay /\ ReadsByVersion /\ Orphans

\* 6. Liveness
MoveCompletes == <>(wpc = "done")
MoveTerminates == <>(wpc = "done" \/ (wpc = "idle" /\ attempts = MaxAttempts))
WritesCommit == \A i \in Clients : <>(cpc[i] = "done")

\* sanity (expected to be violated): the move can complete
MoveNeverDone == wpc /= "done"

\* client workloads (substituted for Ops in the configs)
OpsWrites == ("i1" :> <<"set1">>) @@ ("i2" :> <<"set0">>)
OpsCreate == ("i1" :> <<"set1">>) @@ ("i2" :> <<"create">>)
OpsBoth   == ("i1" :> <<"set1", "create">>) @@ ("i2" :> <<"set0">>)
=============================================================================
