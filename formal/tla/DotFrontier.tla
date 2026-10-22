---------------------------- MODULE DotFrontier ----------------------------
(***************************************************************************)
(* Shep design §9.1-§9.4 and §10.3, on ONE Postgres node.                  *)
(*                                                                         *)
(*  - a non-transactional counter (nextval)                                *)
(*  - mutation transactions: snapshot+context, nextval, insert log row,    *)
(*    commit, or abort (incl. serialization failure after the insert),     *)
(*    or crash at any step                                                 *)
(*  - the void fence on the log's primary key, with Postgres semantics:    *)
(*    an insert of key n waits while another transaction has an           *)
(*    uncommitted insert of n, fails if a committed row exists, and        *)
(*    succeeds if the other inserter rolled back                           *)
(*  - the service's fast-path void after an abort                          *)
(*  - the frontier advancer: voids the first hole above the frontier       *)
(*    (any time: the timeout is abstracted to nondeterminism) and          *)
(*    advances over the contiguous resolved prefix                         *)
(*  - snapshot versions: frontier row + visible committed rows above it    *)
(*  - client tokens and at_least reads                                     *)
(*                                                                         *)
(* SSI conflicts are abstracted as nondeterministic serialization          *)
(* failures; SerialLog.tla looks at what SSI actually does here.           *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
  Txns,              \* client requests (each may retry with a new dot)
  MaxDot,            \* bound on the counter
  HangPossible,      \* TRUE: a transaction holding an uncommitted log row may
                     \*   stall forever (no idle-in-transaction timeout)
  AdvReadsSequence,  \* TRUE: the advancer knows the counter's last value and
                     \*   can void holes above the highest visible row
  MaxAdvCrashes,     \* bound on advancer crashes (re-elections)
  CompactMode        \* how a token is compacted (§9.2 "folding contiguous
                     \*   dots into the vector"): "exact" folds the next dot
                     \*   only if it is an explicit dot of the token or void;
                     \*   "frontier" folds everything up to the node's frontier

Dots == 1..MaxDot

(* A one-node dotted version vector: every dot <= f is resolved, plus the  *)
(* explicit committed dots ex above f.                                     *)
Ver(f, ex) == [f |-> f, ex |-> ex]
Contains(v, n) == n <= v.f \/ n \in v.ex
Max2(a, b) == IF a >= b THEN a ELSE b
Merge(v, w) == LET m == Max2(v.f, w.f)
               IN Ver(m, {n \in v.ex \cup w.ex : n > m})
AddDot(v, n) == IF n <= v.f THEN v ELSE Ver(v.f, v.ex \cup {n})
\* NB: containment is semantic (on the committed dots a version names, see
\* Den below); with voids folded into the vector, a syntactic comparison of
\* vectors is not enough (finding F8)

Owners == Txns \cup {"adv", "nobody"}
RowSt  == {"none", "pendEntry", "pendVoid", "entry", "void"}
Resolved(st) == st \in {"entry", "void"}

VARIABLES
  ctr,       \* last value returned by nextval (0 = none yet)
  rows,      \* rows[n]: the log's primary-key slot n
  frontier,  \* committed value of the frontier row
  pc,        \* request state
  ctx,       \* context of the current attempt (snapshot version + at_least)
  dot,       \* dot held by the current attempt (0 = none)
  tok,       \* the client's merged token (Db handle)
  monoOK,    \* history: every at_least snapshot covered the client's token
  compOK,    \* history: every compaction preserved the token's meaning
  adv,       \* frontier advancer state
  advCrashes \* number of advancer crashes so far

vars == <<ctr, rows, frontier, pc, ctx, dot, tok, monoOK, compOK, adv, advCrashes>>

PCs == {"idle", "run", "dot", "ins", "fv", "fvw", "done", "crashed", "gaveup"}

TypeOK ==
  /\ ctr \in 0..MaxDot
  /\ rows \in [Dots -> [st : RowSt, own : Owners]]
  /\ frontier \in 0..MaxDot
  /\ pc \in [Txns -> PCs]
  /\ dot \in [Txns -> 0..MaxDot]
  /\ monoOK \in BOOLEAN
  /\ adv \in [st : {"idle", "ins", "pend"}, n : 0..MaxDot]

Init ==
  /\ ctr = 0
  /\ rows = [n \in Dots |-> [st |-> "none", own |-> "nobody"]]
  /\ frontier = 0
  /\ pc = [t \in Txns |-> "idle"]
  /\ ctx = [t \in Txns |-> Ver(0, {})]
  /\ dot = [t \in Txns |-> 0]
  /\ tok = [t \in Txns |-> Ver(0, {})]
  /\ monoOK = TRUE
  /\ compOK = TRUE
  /\ adv = [st |-> "idle", n |-> 0]
  /\ advCrashes = 0

\* the set of committed mutations a version denotes
Den(v) == {n \in 1..v.f : rows[n].st = "entry"} \cup v.ex
Subsumes(v, w) == Den(w) \subseteq Den(v)

(* §9.4: the frontier row read in the snapshot, plus the visible committed *)
(* entries above it. Uncommitted rows are invisible.                       *)
SnapshotVersion == Ver(frontier, {n \in Dots : n > frontier /\ rows[n].st = "entry"})

Row(n, st, o) == [st |-> st, own |-> o]

(*************************** mutation path (§10.3) *************************)

\* steps 1-2: begin, read the snapshot's version, merge at_least
Begin(t) ==
  /\ pc[t] = "idle"
  /\ LET v == SnapshotVersion IN
       /\ ctx' = [ctx EXCEPT ![t] = Merge(v, tok[t])]
       /\ monoOK' = (monoOK /\ Subsumes(v, tok[t]))
  /\ pc' = [pc EXCEPT ![t] = "run"]
  /\ UNCHANGED <<ctr, rows, frontier, dot, tok, compOK, adv, advCrashes>>

\* step 5: n = nextval('dot_counter')
Alloc(t) ==
  /\ pc[t] = "run"
  /\ ctr < MaxDot
  /\ ctr' = ctr + 1
  /\ dot' = [dot EXCEPT ![t] = ctr + 1]
  /\ pc' = [pc EXCEPT ![t] = "dot"]
  /\ UNCHANGED <<rows, frontier, ctx, tok, monoOK, compOK, adv, advCrashes>>

\* modelling artefact: the counter bound is reached
GiveUp(t) ==
  /\ pc[t] = "run"
  /\ ctr = MaxDot
  /\ pc' = [pc EXCEPT ![t] = "gaveup"]
  /\ UNCHANGED <<ctr, rows, frontier, ctx, dot, tok, monoOK, compOK, adv, advCrashes>>

\* step 6: insert the log row; primary-key semantics
Insert(t) ==
  LET n == dot[t] IN
  /\ pc[t] = "dot"
  /\ rows[n].st /= "pendVoid"                   \* waits on an in-flight void
  /\ IF rows[n].st = "none"
       THEN /\ rows' = [rows EXCEPT ![n] = Row(n, "pendEntry", t)]
            /\ pc' = [pc EXCEPT ![t] = "ins"]
            /\ dot' = dot
       ELSE \* unique violation: the dot was voided; abort and retry
            /\ Assert(rows[n].st = "void", "unexpected row state")
            /\ rows' = rows
            /\ pc' = [pc EXCEPT ![t] = "idle"]
            /\ dot' = [dot EXCEPT ![t] = 0]
  /\ UNCHANGED <<ctr, frontier, ctx, tok, monoOK, compOK, adv, advCrashes>>

\* steps 7-8: observe, commit. The token is the context plus the new dot.
Commit(t) ==
  LET n == dot[t] IN
  /\ pc[t] = "ins"
  /\ rows' = [rows EXCEPT ![n] = Row(n, "entry", t)]
  /\ tok' = [tok EXCEPT ![t] = Merge(tok[t], AddDot(ctx[t], n))]
  /\ pc' = [pc EXCEPT ![t] = "done"]
  /\ UNCHANGED <<ctr, frontier, ctx, dot, monoOK, compOK, adv, advCrashes>>

\* abort (serialization failure or any error) at any step before commit
Abort(t) ==
  /\ pc[t] \in {"run", "dot", "ins"}
  /\ IF pc[t] = "run"
       THEN /\ pc' = [pc EXCEPT ![t] = "idle"]     \* no dot yet: plain retry
            /\ rows' = rows
       ELSE /\ pc' = [pc EXCEPT ![t] = "fv"]       \* fast-path void, then retry
            /\ rows' = IF pc[t] = "ins"
                         THEN [rows EXCEPT ![dot[t]] = Row(dot[t], "none", "nobody")]
                         ELSE rows
  /\ UNCHANGED <<ctr, frontier, ctx, dot, tok, monoOK, compOK, adv, advCrashes>>

\* §9.3 fast path: the service voids its own dot after the abort
FastVoidInsert(t) ==
  LET n == dot[t] IN
  /\ pc[t] = "fv"
  /\ rows[n].st \notin {"pendEntry", "pendVoid"}   \* waits on in-flight inserts
  /\ IF rows[n].st = "none"
       THEN /\ rows' = [rows EXCEPT ![n] = Row(n, "pendVoid", t)]
            /\ pc' = [pc EXCEPT ![t] = "fvw"]
            /\ dot' = dot
       ELSE \* already resolved (the advancer got there first)
            /\ rows' = rows
            /\ pc' = [pc EXCEPT ![t] = "idle"]
            /\ dot' = [dot EXCEPT ![t] = 0]
  /\ UNCHANGED <<ctr, frontier, ctx, tok, monoOK, compOK, adv, advCrashes>>

FastVoidCommit(t) ==
  LET n == dot[t] IN
  /\ pc[t] = "fvw"
  /\ rows' = [rows EXCEPT ![n] = Row(n, "void", t)]
  /\ pc' = [pc EXCEPT ![t] = "idle"]
  /\ dot' = [dot EXCEPT ![t] = 0]
  /\ UNCHANGED <<ctr, frontier, ctx, tok, monoOK, compOK, adv, advCrashes>>

\* the service instance dies; Postgres rolls back its open transaction
Crash(t) ==
  /\ pc[t] \in {"run", "dot", "ins", "fv", "fvw"}
  /\ rows' = [n \in Dots |->
                IF rows[n].own = t /\ rows[n].st \in {"pendEntry", "pendVoid"}
                  THEN Row(n, "none", "nobody") ELSE rows[n]]
  /\ pc' = [pc EXCEPT ![t] = "crashed"]
  /\ UNCHANGED <<ctr, frontier, ctx, dot, tok, monoOK, compOK, adv, advCrashes>>

\* a plain read with at_least = the client's token (§9.4); on a primary
\* the snapshot must already cover the token
Read(t) ==
  /\ pc[t] \in {"idle", "done"}
  /\ LET v == SnapshotVersion IN
       /\ monoOK' = (monoOK /\ Subsumes(v, tok[t]))
       /\ tok' = [tok EXCEPT ![t] = Merge(tok[t], v)]
  /\ UNCHANGED <<ctr, rows, frontier, pc, ctx, dot, compOK, adv, advCrashes>>


RECURSIVE FoldExact(_)
FoldExact(v) ==
  IF v.f < MaxDot /\ (v.f + 1 \in v.ex \/ rows[v.f + 1].st = "void")
    THEN FoldExact(Ver(v.f + 1, v.ex \ {v.f + 1}))
    ELSE v

Compacted(v) ==
  IF CompactMode = "exact" THEN FoldExact(v)
  ELSE LET m == Max2(v.f, frontier) IN Ver(m, {n \in v.ex : n > m})

\* the service compacts a token before returning it
Compact(t) ==
  /\ pc[t] \in {"idle", "done"}
  /\ LET v2 == Compacted(tok[t]) IN
       /\ v2 /= tok[t]
       /\ tok' = [tok EXCEPT ![t] = v2]
       /\ compOK' = (compOK /\ Den(v2) = Den(tok[t]))
  /\ UNCHANGED <<ctr, rows, frontier, pc, ctx, dot, monoOK, adv, advCrashes>>

(*************************** frontier advancer (§9.3) **********************)

\* the highest dot the advancer knows was handed out
KnownTop == IF AdvReadsSequence THEN ctr
            ELSE LET vis == {n \in Dots : Resolved(rows[n].st)}
                 IN IF vis = {} THEN 0 ELSE CHOOSE m \in vis : \A k \in vis : k <= m

\* pick the first hole above the frontier (not visibly resolved); the
\* "older than a timeout" condition is abstracted: any time is allowed
AdvPick ==
  /\ adv.st = "idle"
  /\ frontier + 1 <= KnownTop
  /\ ~ Resolved(rows[frontier + 1].st)
  /\ adv' = [st |-> "ins", n |-> frontier + 1]
  /\ UNCHANGED <<ctr, rows, frontier, pc, ctx, dot, tok, monoOK, compOK, advCrashes>>

AdvInsertVoid ==
  LET n == adv.n IN
  /\ adv.st = "ins"
  /\ rows[n].st \notin {"pendEntry", "pendVoid"}      \* waits on the owner
  /\ IF rows[n].st = "none"
       THEN /\ rows' = [rows EXCEPT ![n] = Row(n, "pendVoid", "adv")]
            /\ adv' = [adv EXCEPT !.st = "pend"]
       ELSE /\ rows' = rows                           \* owner committed: fails
            /\ adv' = [st |-> "idle", n |-> 0]
  /\ UNCHANGED <<ctr, frontier, pc, ctx, dot, tok, monoOK, compOK, advCrashes>>

AdvCommitVoid ==
  /\ adv.st = "pend"
  /\ rows' = [rows EXCEPT ![adv.n] = Row(adv.n, "void", "adv")]
  /\ adv' = [st |-> "idle", n |-> 0]
  /\ UNCHANGED <<ctr, frontier, pc, ctx, dot, tok, monoOK, compOK, advCrashes>>

\* advancer instance dies (re-elected through the advisory lock)
AdvCrash ==
  /\ adv.st \in {"ins", "pend"}
  /\ advCrashes < MaxAdvCrashes
  /\ advCrashes' = advCrashes + 1
  /\ rows' = IF adv.st = "pend"
               THEN [rows EXCEPT ![adv.n] = Row(adv.n, "none", "nobody")]
               ELSE rows
  /\ adv' = [st |-> "idle", n |-> 0]
  /\ UNCHANGED <<ctr, frontier, pc, ctx, dot, tok, monoOK, compOK>>

\* one tiny transaction: advance over the contiguous resolved prefix
ContigTop ==
  LET ok(m) == \A k \in (frontier + 1)..m : Resolved(rows[k].st)
  IN CHOOSE m \in frontier..MaxDot : ok(m) /\ (m = MaxDot \/ ~ ok(m + 1))

AdvAdvance ==
  /\ ContigTop > frontier
  /\ frontier' = ContigTop
  /\ UNCHANGED <<ctr, rows, pc, ctx, dot, tok, monoOK, compOK, adv, advCrashes>>

(*************************** spec ******************************************)

TxnProgress(t) ==
  Begin(t) \/ Alloc(t) \/ GiveUp(t) \/ Insert(t) \/ FastVoidInsert(t) \/ FastVoidCommit(t)

Next ==
  \/ \E t \in Txns : TxnProgress(t) \/ Commit(t) \/ Abort(t) \/ Crash(t) \/ Read(t) \/ Compact(t)
  \/ AdvPick \/ AdvInsertVoid \/ AdvCommitVoid \/ AdvCrash \/ AdvAdvance

Fairness ==
  /\ \A t \in Txns : WF_vars(TxnProgress(t))
  \* Postgres eventually ends every open transaction (commit, or abort by
  \* idle_in_transaction_session_timeout) unless HangPossible
  /\ IF HangPossible THEN TRUE
     ELSE \A t \in Txns : WF_vars(Commit(t) \/ Abort(t))
  /\ WF_vars(AdvPick) /\ WF_vars(AdvInsertVoid) /\ WF_vars(AdvCommitVoid)
  /\ WF_vars(AdvAdvance)

Spec == Init /\ [][Next]_vars /\ Fairness

(*************************** properties ************************************)

\* every dot <= frontier is resolved
FrontierResolved == \A n \in 1..frontier : Resolved(rows[n].st)

\* a token never contains a dot that is not resolved, and its explicit dots
\* are committed entries (never voids)
TokenSound(v) ==
  /\ \A n \in Dots : Contains(v, n) => Resolved(rows[n].st)
  /\ \A n \in v.ex : rows[n].st = "entry"

TokensSound == \A t \in Txns : TokenSound(tok[t])
ContextsSound == \A t \in Txns : pc[t] \in {"run", "dot", "ins"} => TokenSound(ctx[t])

\* a transaction's own dot is never in its own context (acyclic causality)
OwnDotNotInContext ==
  \A t \in Txns : pc[t] \in {"dot", "ins"} => ~ Contains(ctx[t], dot[t])

\* primary-key fence: a slot is never claimed by two in-flight inserters,
\* and a dot below the counter is never re-issued (by construction)
MonotonicReads == monoOK

\* compaction never changes which mutations a token names
CompactionPreservesMeaning == compOK

Inv == TypeOK /\ FrontierResolved /\ TokensSound /\ ContextsSound
       /\ OwnDotNotInContext /\ MonotonicReads /\ CompactionPreservesMeaning

\* a resolved dot never changes again: never both void and committed
ResolvedStable ==
  [][\A n \in Dots : Resolved(rows[n].st) => rows'[n].st = rows[n].st]_vars

FrontierMonotonic == [][frontier' >= frontier]_vars

\* liveness: every allocated dot eventually falls under the frontier
AllocatedEventuallyResolved == \A n \in Dots : (ctr >= n) ~> (frontier >= n)

Sym == Permutations(Txns)

\* sanity: out-of-order commit is reachable (expected to be VIOLATED)
NoOutOfOrderCommit ==
  ~ \E a, b \in Dots : a < b /\ rows[b].st = "entry" /\ rows[a].st = "pendEntry"
=============================================================================
