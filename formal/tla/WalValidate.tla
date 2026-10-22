----------------------------- MODULE WalValidate -----------------------------
(***************************************************************************)
(* Shep design §10.3 (validate, append, commit; idempotency; `claim`),     *)
(* §9.1-§9.2 (dots and DVVs), §8 (mutations, `claim`), §10.4 (log         *)
(* truncation), on ONE partition implemented as a Raft group. The         *)
(* partition is every mutation's anchor, so idempotency keys are recorded  *)
(* and checked here.                                                       *)
(*                                                                         *)
(* Raft is abstract: terms; elections by a quorum with the up-to-date      *)
(* check; a new leader's no-op; whole-log AppendEntries (a follower's      *)
(* conflicting suffix is overwritten); commit through a current-term       *)
(* quorum (own-term entries only, earlier ones indirectly). A deposed      *)
(* leader keeps believing it leads until it hears of a higher term: it     *)
(* can still validate, append to its own log, run its sweeper, and send    *)
(* AppendEntries to replicas whose term is not higher.                     *)
(*                                                                         *)
(* Data. Keys a, b with values v0/v1 (initially v0), plus one fresh key    *)
(* per execution of `create` (a generated UUID). A range predicate is      *)
(* "every key whose value is v1" (an index range such as state = Open); a  *)
(* write matches it if its old or new value is v1. The state at a version *)
(* S (a set of dots: one partition's DVV, a prefix plus extras) is, per    *)
(* key, the value written by the newest committed entry in S that wrote it *)
(* (visibility by stamp).                                                  *)
(*                                                                         *)
(* Requests (client calls), each with an idempotency key, run a fixed      *)
(* program (Prog):                                                         *)
(*   toggle k : read k, write k := flip(k)                                 *)
(*   guard k  : read the range "value = v1" and k; require it is empty,    *)
(*              then set k := v1 (an insert guarded by a count: phantoms)  *)
(*   create   : write a fresh key := v1 (it reads only that key)           *)
(*   claim    : lease-based claim(query, n = 1) over tasks a, b. A task    *)
(*              is its status key (v0 queued, v1 done) and its lease key   *)
(*              (<<generation, "none"|"live"|"expired">>). The candidate   *)
(*              predicate reads only the candidate's own keys (queued and  *)
(*              no live lease). The proposal names every candidate read at *)
(*              S; the candidate range is not in the read set; the leader  *)
(*              drops candidates written by an entry whose stamp is not in *)
(*              S, takes one of the rest, and writes its lease at the next *)
(*              generation. Its result is the claimed task and generation. *)
(*   complete : the worker's next request after its claim's reply: set the *)
(*              claimed task's status to done, fenced on the lease         *)
(*              generation (the lease key is read; `lease_lost` unless it  *)
(*              is still live at that generation)                          *)
(* The leader's sweeper writes @lease_expired for a live lease using the   *)
(* leader's clock: it reads the lease from its own log (uncommitted tail   *)
(* included) and appends the expiry. Any leader, deposed or not, may do so *)
(* at any time (an adversarial clock), at most MaxExpire times.            *)
(*                                                                         *)
(* A request may be executed several times (MaxExec): retries after        *)
(* contended / too_old, and retries after an unknown outcome through       *)
(* another front end, each re-executing at a new S while earlier proposals *)
(* for the same key may still be in flight. Proposals are never removed    *)
(* from the network: any proposal can be (re)sent to any leader, any       *)
(* number of times. Rejections (contended, too_old, not leader, a key      *)
(* already in the table) leave no trace: the front end's responses to      *)
(* them (re-send or re-execute) are already enabled actions, and an        *)
(* aborted attempt does not consume the key.                               *)
(*                                                                         *)
(* S is "just before some committed mutation" or the latest committed      *)
(* prefix (so it can be below the window), merged with the token of any    *)
(* committed reply a client received (its context plus its dot), which     *)
(* gives versions with extras.                                             *)
(*                                                                         *)
(* The leader's validation table holds write sets tagged with idx and      *)
(* term; its idempotency table holds (key, idx). Both are memory only. The *)
(* window start `win` advances with time (bounded by MaxWin). Replicas may *)
(* truncate their log below what is committed (§10.4); a follower missing *)
(* what the leader truncated installs a snapshot. The idempotency table is *)
(* in the Raft state machine and its snapshots, so truncation never loses  *)
(* it; the validation table is rebuilt from the log only.                  *)
(*                                                                         *)
(* The constants select the rule implemented; see the cfgs.                *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  Servers,      \* Raft replicas (model values)
  MaxTerm,      \* bound on terms
  MaxLen,       \* bound on each replica's log length
  Prog,         \* request (string) -> program record
  MaxExec,      \* executions per request (first try + retries)
  MaxWin,       \* bound on advances of the validation window start
  MaxIWin,      \* bound on advances of the idempotency window start
                \*   (0: "at most one commit per key", forever)
  MaxExpire,    \* bound on sweeper expiries
  TableAt,      \* "append": write sets enter the table at append (design)
                \* "commit": they enter only once committed (C1)
  Rebuild,      \* TRUE: a new leader rebuilds both tables from its log
                \*   before accepting (design); FALSE: it keeps whatever
                \*   tables it had when it last led (empty if never)
  IdemAt,       \* "append": keys enter the idempotency table at append
                \*   (design); "commit": only committed entries are checked
  CheckRanges,  \* TRUE: validation matches writes against range predicates
  CheckTooOld,  \* TRUE: reject when S's prefix is below the window start
  Fence,        \* TRUE: `complete` reads the lease key (validated) and
                \*   requires it live at its generation; FALSE: it doesn't
  Split,        \* FALSE: the leader validates a proposal and appends it in
                \*   one atomic step (shep-raft's core loop); TRUE: it
                \*   validates, and appends the validated entry in a later
                \*   step, possibly after stepping down and being re-elected
  VoidRule,     \* TRUE: an entry records the term it was validated in, and
                \*   if it is in a log at another term it is void (a no-op
                \*   that consumes no key); FALSE: no such rule
  Durability,   \* "sync": an append is durable at once, no crashes (the
                \*   configs above); "flushed": Redpanda's rule, a replica's
                \*   log is durable up to its flushed offset, a crash loses
                \*   the rest, commit = a majority of FLUSHED offsets (the
                \*   leader counts itself only once flushed; followers ack
                \*   after fsync); "unflushed": as flushed, but commit counts
                \*   appended, unflushed entries (negative)
  MaxCrash,     \* bound on replica crashes (with Durability # "sync")
  Truncation,   \* TRUE: replicas may truncate committed log prefixes
  TruncFix,     \* TRUE: a leader's too_old window starts no earlier than
                \*   the first entry still in its log
  FailMode,     \* what a front end does when a re-execution ends in a
                \* failure it would report to the client (a failed
                \* `require`, `lease_lost`, a claim with no candidates):
                \* "local":  returns it itself (the design as written)
                \* "probe":  asks the anchor's leader, which returns the
                \*           key's outcome if the key is in its table, and
                \*           otherwise confirms the failure without logging
                \*           it (an aborted attempt doesn't consume the key)
                \* "logged": the leader returns the key's outcome if the key
                \*           is in its table, and otherwise appends a failure
                \*           record under the key (the failure is the key's
                \*           one outcome)
                \* "fast":   the fast path: a failure goes through the anchor
                \*           (as "logged") only when the key may already have
                \*           an outcome: the attempt carries the client's
                \*           retry mark. A first attempt returns its failure
                \*           directly, and the client takes it only if it has
                \*           not moved on to a retry. With "fast", an attempt
                \*           is begun (Begin) and runs later (Run), so an
                \*           abandoned attempt can still be in flight.
  MarkRetry,    \* "fast": TRUE: the client marks every retry; FALSE: never
  Refusal,      \* "none": validate+append is one step (every config above)
                \* otherwise the validator puts a Pending slot in its table
                \* (it conflicts with everything, holds its key) and hands
                \* the entry to raft, which later, at receipt, appends it
                \* (if it is leader in the same term with its lease
                \* established) or refuses it, never appending:
                \* "design": a refused slot becomes NotAppended (dropped:
                \*   conflicts with nothing, key released); appended-then-
                \*   truncated (LogEntryDiscarded) and unknown stay Pending
                \* "discard_na": LogEntryDiscarded is also treated as
                \*   NotAppended: the table drops what it holds at that index
                \* "unknown_release": an appended entry whose response is
                \*   unknown has its table records released
                \* "stay_pending": a refused slot stays Pending (the old
                \*   behaviour; liveness negative)
  LeaseWindow,  \* (Refusal) TRUE: a fresh leader refuses writes until its
                \*   first quorum-acked heartbeat; FALSE: no lease window
  RecvOrder,    \* (Refusal) "fifo": raft receives entries in the order the
                \*   validator validated them; "any": in any order (negative)
  HitReply      \* "wait": an idempotency hit on an appended entry gets its
                \*   outcome once it commits (Reply), and nothing if it is
                \*   overwritten (the waiter gets not_leader and retries);
                \* "immediate": the leader answers a hit from its table at
                \*   once, committed or not (negative)

ASSUME TableAt \in {"append", "commit"} /\ IdemAt \in {"append", "commit"}
ASSUME FailMode \in {"local", "probe", "logged", "fast"}
ASSUME HitReply \in {"wait", "immediate"}
ASSUME Refusal \in {"none", "design", "discard_na", "unknown_release", "stay_pending"}
ASSUME Refusal # "none" => ~ Split /\ Durability = "sync"
ASSUME RecvOrder \in {"fifo", "any"}
ASSUME Durability \in {"sync", "flushed", "unflushed"}
ASSUME ~ (Truncation /\ Durability # "sync")

Reqs == DOMAIN Prog
Quorums == {Q \in SUBSET Servers : 2 * Cardinality(Q) > Cardinality(Servers)}

Tasks == {"a", "b"}
K(n) == <<n, 0>>            \* a's / b's value, or a task's status
LK(n) == <<n, 1>>           \* a task's lease
BaseKeys == {K(n) : n \in Tasks}
NewKey(q, e) == <<q, e>>
NewKeys == {NewKey(q, e) : q \in {r \in Reqs : Prog[r].kind = "create"}, e \in 1..MaxExec}
HasClaims == \E q \in Reqs : Prog[q].kind = "claim"
LeaseKeys == IF HasClaims THEN {LK(n) : n \in Tasks} ELSE {}
RangeKeys == BaseKeys \cup NewKeys          \* keys the range predicate ranges over
Keys == RangeKeys \cup LeaseKeys
PredVal == "v1"
NoLease == <<0, "none">>
InitVal(k) == IF k \in BaseKeys THEN "v0" ELSE IF k \in LeaseKeys THEN NoLease ELSE "absent"
Flip(v) == IF v = "v0" THEN "v1" ELSE "v0"

Max(a, b) == IF a > b THEN a ELSE b
Min(a, b) == IF a < b THEN a ELSE b
MaxSet(X) == CHOOSE x \in X : \A y \in X : y <= x

NoR == [keys |-> {}, preds |-> {}]
NoW == [k \in {} |-> <<"", "">>]
NoObs == <<[k \in {} |-> ""], [v \in {} |-> {}]>>

VARIABLES
  term,         \* currentTerm per replica
  role,         \* "L" or "F" (a deposed leader still says "L")
  log,          \* per replica: sequence of entries
  commitIndex,  \* per replica: highest index it knows committed
  first,        \* per replica: first index still in its log (truncation)
  vtab,         \* per replica: validation table, {[idx, term, w]}
  itab,         \* per replica: idempotency table, {[ik, idx]}
  clog,         \* ghost: the committed log (longest committed prefix)
  win,          \* validation window start: the table covers idx > win
  iwin,         \* idempotency window start: keys of idx > iwin are kept
  net,          \* proposals sent (never removed: re-sends, duplicates)
  nexec,        \* per request: executions so far
  replies,      \* outcomes delivered to clients: [ik, idx, res]
  nexp,         \* sweeper expiries so far
  pend,         \* per replica (Split): the validated entry not yet appended
  flushed,      \* per replica: its log is durable up to here (not "sync")
  ncrash,       \* crashes so far
  running,      \* "fast": per request, the attempts begun and not yet run
  slots,        \* per replica (Refusal): Pending slots [id, ik, w, term];
                \*   only those of its current term count
  inbox,        \* per replica (Refusal): entries handed to raft, not yet
                \*   received (appended or refused)
  lease,        \* per replica: the term whose leader lease it established
  appBy,        \* per replica: entries it appended [id, idx, pid, term],
                \*   for LogEntryDiscarded / unknown notifications
  naIds,        \* ghost: slots resolved NotAppended [id, bad], bad: an
                \*   entry of its proposal and term was in a log then
  appIds        \* ghost: slot ids ever appended to a log

raftVars == <<term, role, log, commitIndex, first>>
rcvVars == <<slots, inbox, lease, appBy, naIds, appIds>>
extVars == <<pend, flushed, ncrash, running, slots, inbox, lease, appBy, naIds, appIds>>
vars == <<term, role, log, commitIndex, first, vtab, itab, clog, win, iwin,
          net, nexec, replies, nexp, pend, flushed, ncrash, running,
          slots, inbox, lease, appBy, naIds, appIds>>

(*************************** entries and versions *************************)

\* an entry validated in one term and in a log at another is void (VoidRule)
Void(e) == VoidRule /\ e.vterm # e.term
IsTxn(e) == e.kind \in {"mut", "claim", "sweep"} /\ ~ Void(e)   \* writes data
Keyed(e) == e.kind \in {"mut", "claim", "fail"} /\ ~ Void(e)    \* has an idempotency key

\* vterm: the term it was validated in; dep (ghost): its leader was already
\* deposed (a quorum in a higher term) when it appended it
Noop(t) == [term |-> t, kind |-> "noop", ik |-> "", pid |-> <<"", 0>>, S |-> {},
            R |-> NoR, obs |-> NoObs, w |-> NoW, fence |-> <<>>, iw |-> 0, wn |-> 0,
            vterm |-> t, dep |-> FALSE]

Prefix(n) == 1..n
\* a DVV's prefix: the largest n with 1..n in S
Pre(S) == CHOOSE n \in 0..MaxLen : Prefix(n) \subseteq S /\ (n + 1) \notin S

\* the state at version S of log l (by stamp: the newest writer in S)
WritersIn(l, S, k) == {i \in S : i <= Len(l) /\ IsTxn(l[i]) /\ k \in DOMAIN l[i].w}
ValIn(l, S, k) == IF WritersIn(l, S, k) = {} THEN InitVal(k)
                  ELSE l[MaxSet(WritersIn(l, S, k))].w[k][2]
ObsIn(l, S, R) == <<[k \in R.keys |-> ValIn(l, S, k)],
                    [v \in R.preds |-> {k \in RangeKeys : ValIn(l, S, k) = v}]>>
\* reads by front ends: S names only committed dots
Val(S, k) == ValIn(clog, S, k)
Obs(S, R) == ObsIn(clog, S, R)

\* a write set w touches read set R: a key in it, or a fact matching one of
\* its range predicates (old or new value)
Conflicts(w, R, ranges) ==
  \E k \in DOMAIN w :
     \/ k \in R.keys
     \/ ranges /\ k \in RangeKeys /\ \E v \in R.preds : w[k][1] = v \/ w[k][2] = v

Tok(i) == clog[i].S \cup {i}
OkReplies == {x \in replies : x.res = "ok" /\ x.idx \in 1..Len(clog)}
ReadPoints == {0, Len(clog)} \cup {i - 1 : i \in {j \in 1..Len(clog) : IsTxn(clog[j])}}
Versions ==
  {Prefix(c) : c \in ReadPoints}
  \cup {Prefix(c) \cup Tok(r.idx) : c \in ReadPoints, r \in OkReplies}

(*************************** front end: execute ***************************)

\* the claim a worker's `complete` follows: its reply's entry
ClaimOf(q) == {r \in OkReplies : r.ik = Prog[q].claim}
ClaimedBy(r) == {t \in Tasks : LK(t) \in DOMAIN clog[r.idx].w}

Plan(q, e, S) ==
  LET op == Prog[q] IN
  CASE op.kind = "toggle" ->
         LET k == K(op.key) IN
         [ok |-> TRUE, kind |-> "mut", R |-> [keys |-> {k}, preds |-> {}],
          w |-> (k :> <<Val(S, k), Flip(Val(S, k))>>), cands |-> {}, fence |-> <<>>]
    [] op.kind = "guard" ->
         LET k == K(op.key) IN
         [ok |-> \A x \in RangeKeys : Val(S, x) # PredVal, kind |-> "mut",
          R |-> [keys |-> {k}, preds |-> {PredVal}],
          w |-> (k :> <<Val(S, k), PredVal>>), cands |-> {}, fence |-> <<>>]
    [] op.kind = "create" ->
         LET k == NewKey(q, e) IN
         [ok |-> TRUE, kind |-> "mut", R |-> [keys |-> {k}, preds |-> {}],
          w |-> (k :> <<"absent", PredVal>>), cands |-> {}, fence |-> <<>>]
    [] op.kind = "claim" ->
         LET c == {<<t, Val(S, LK(t))>> :
                     t \in {u \in Tasks : Val(S, K(u)) = "v0" /\ Val(S, LK(u))[2] # "live"}}
         IN [ok |-> c # {}, kind |-> "claim", R |-> NoR, w |-> NoW, cands |-> c,
             fence |-> <<>>]
    [] op.kind = "complete" ->
         LET r == CHOOSE x \in ClaimOf(q) : TRUE
             t == CHOOSE u \in ClaimedBy(r) : TRUE
             g == clog[r.idx].w[LK(t)][2][1]
         IN [ok |-> (~ Fence \/ Val(S, LK(t)) = <<g, "live">>)       \* else lease_lost
                    /\ Val(S, K(t)) = "v0",
             kind |-> "mut",
             R |-> [keys |-> IF Fence THEN {K(t), LK(t)} ELSE {K(t)}, preds |-> {}],
             w |-> (K(t) :> <<"v0", "v1">>), cands |-> {}, fence |-> <<t, g>>]

\* `complete` runs only after its claim's reply named a task
CanRun(q) ==
  Prog[q].kind = "complete" =>
    \E r \in ClaimOf(q) : ClaimedBy(r) # {}

\* a program may bound its own executions with a `max` field
Execs(q) == IF "max" \in DOMAIN Prog[q] THEN Prog[q].max ELSE MaxExec

Execute(q) ==
  /\ FailMode # "fast"
  /\ nexec[q] < Execs(q)
  /\ ~ \E r \in replies : r.ik = q          \* the client has its outcome
  /\ CanRun(q)
  /\ LET e == nexec[q] + 1 IN
     \E S \in Versions :
       LET p == Plan(q, e, S)
           prop == [pid |-> <<q, e>>, ik |-> q, kind |-> p.kind, S |-> S, R |-> p.R,
                    obs |-> Obs(S, p.R), w |-> p.w, cands |-> p.cands, fence |-> p.fence]
           failp == [pid |-> <<q, e>>, ik |-> q, kind |-> "fail", S |-> S, R |-> NoR,
                     obs |-> NoObs, w |-> NoW, cands |-> {}, fence |-> <<>>]
       IN
       /\ nexec' = [nexec EXCEPT ![q] = e]
       /\ IF p.ok
            THEN net' = net \cup {prop} /\ UNCHANGED replies
          ELSE IF FailMode = "local"
            THEN replies' = replies \cup {[ik |-> q, idx |-> 0, res |-> "failed"]}
                 /\ UNCHANGED net
            ELSE net' = net \cup {failp} /\ UNCHANGED replies
  /\ UNCHANGED <<raftVars, extVars, vtab, itab, clog, win, iwin, nexp>>

\* "fast": the client begins attempt e (through some front end); it runs
\* later. Begining attempt e >= 2 means the client gave up waiting on the
\* earlier ones (they may still be in flight).
Begin(q) ==
  /\ FailMode = "fast"
  /\ nexec[q] < Execs(q)
  /\ ~ \E r \in replies : r.ik = q
  /\ CanRun(q)
  /\ nexec' = [nexec EXCEPT ![q] = nexec[q] + 1]
  /\ running' = [running EXCEPT ![q] = running[q] \cup {nexec[q] + 1}]
  /\ UNCHANGED <<raftVars, vtab, itab, clog, win, iwin, net, replies, nexp, pend, flushed, ncrash,
                 rcvVars>>

Marked(e) == MarkRetry /\ e >= 2

Run(q, e) ==
  /\ e \in running[q]
  /\ \E S \in Versions :
       LET p == Plan(q, e, S)
           prop == [pid |-> <<q, e>>, ik |-> q, kind |-> p.kind, S |-> S, R |-> p.R,
                    obs |-> Obs(S, p.R), w |-> p.w, cands |-> p.cands, fence |-> p.fence]
           failp == [pid |-> <<q, e>>, ik |-> q, kind |-> "fail", S |-> S, R |-> NoR,
                     obs |-> NoObs, w |-> NoW, cands |-> {}, fence |-> <<>>]
       IN
       IF p.ok
         THEN net' = net \cup {prop} /\ UNCHANGED replies
       ELSE IF Marked(e)
         THEN net' = net \cup {failp} /\ UNCHANGED replies
       \* unmarked: returned directly, taken only by a client still waiting
       \* on this attempt
       ELSE /\ replies' = IF e = nexec[q] /\ ~ \E r \in replies : r.ik = q
                          THEN replies \cup {[ik |-> q, idx |-> 0, res |-> "failed"]}
                          ELSE replies
            /\ UNCHANGED net
  /\ running' = [running EXCEPT ![q] = running[q] \ {e}]
  /\ UNCHANGED <<raftVars, vtab, itab, clog, win, iwin, nexec, nexp, pend, flushed, ncrash,
                 rcvVars>>

(*************************** leader: validate and append ******************)

\* what the leader's tables answer (the window, and C1 / committed-only)
VT(s) == {x \in vtab[s] : x.idx > win /\ (TableAt = "append" \/ x.idx <= commitIndex[s])}
IT(s) == {x \in itab[s] : x.idx > iwin /\ (IdemAt = "append" \/ x.idx <= commitIndex[s])}
\* the window the leader enforces with too_old
EffWin(s) == IF TruncFix THEN Max(win, first[s] - 1) ELSE win

TooOld(s, p) == CheckTooOld /\ Pre(p.S) < EffWin(s)

AppendEntry(s, ent) ==
  LET idx == Len(log[s]) + 1 IN
  /\ log' = [log EXCEPT ![s] = Append(log[s], ent)]
  /\ vtab' = IF IsTxn(ent)
               THEN [vtab EXCEPT ![s] = vtab[s] \cup {[idx |-> idx, term |-> ent.term, w |-> ent.w]}]
               ELSE vtab
  /\ itab' = IF Keyed(ent)
               THEN [itab EXCEPT ![s] = itab[s] \cup {[ik |-> ent.ik, idx |-> idx]}]
               ELSE itab

\* the leader knows it still leads (as Raft's ReadIndex does): an entry of
\* its own term has committed, and a quorum is in its term right now
Confirmed(s) ==
  /\ \E i \in 1..commitIndex[s] : log[s][i].term = term[s]
  /\ \E Q \in Quorums : \A q \in Q : term[q] = term[s]

\* a quorum is in a higher term: s's term is over, whether s knows or not
Deposed(s) == role[s] = "L" /\ \E Q \in Quorums : \A q \in Q : term[q] > term[s]

\* a validated entry: appended in the same step (atomic), or held for a
\* later AppendPending (Split; one at a time, so validation stays in order)
\* (Refusal) or handed to raft behind a Pending slot
SlotId(s, ent) == <<ent.pid, ent.term, s>>
Accept(s, ent) ==
  IF Split
    THEN /\ pend[s] = {}
         /\ pend' = [pend EXCEPT ![s] = {ent}]
         /\ UNCHANGED <<log, vtab, itab, slots, inbox>>
  ELSE IF Refusal # "none"
    THEN /\ slots' = [slots EXCEPT ![s] = slots[s] \cup
                        {[id |-> SlotId(s, ent), ik |-> ent.ik, w |-> ent.w, term |-> ent.term]}]
         /\ inbox' = [inbox EXCEPT ![s] = Append(inbox[s], [id |-> SlotId(s, ent), ent |-> ent])]
         /\ UNCHANGED <<log, vtab, itab, pend>>
    ELSE /\ AppendEntry(s, ent)
         /\ UNCHANGED <<pend, slots, inbox>>

\* the validator's Pending slots of its current term
PS(s) == {x \in slots[s] : x.term = term[s]}

Process(s, p) ==
  /\ role[s] = "L"
  /\ p \in net
  \* idempotency first: a key in the table gets that entry's outcome (by
  \* Reply, once it commits) and nothing is appended
  /\ ~ \E x \in IT(s) : x.ik = p.ik
  /\ ~ \E x \in PS(s) : x.ik = p.ik
  /\ LET base == [term |-> term[s], kind |-> p.kind, ik |-> p.ik, pid |-> p.pid,
                  S |-> p.S, R |-> p.R, obs |-> p.obs, w |-> p.w, fence |-> p.fence,
                  iw |-> iwin, wn |-> win, vterm |-> term[s], dep |-> Deposed(s)]
     IN
     CASE p.kind = "fail" /\ FailMode \in {"logged", "fast"} ->
            /\ Len(log[s]) < MaxLen
            /\ Accept(s, base)
            /\ UNCHANGED replies
       [] p.kind = "fail" /\ FailMode = "probe" ->
            /\ Confirmed(s)
            /\ replies' = replies \cup {[ik |-> p.ik, idx |-> 0, res |-> "failed"]}
            /\ UNCHANGED <<log, vtab, itab, pend, slots, inbox>>
       [] p.kind = "mut" ->
            /\ Len(log[s]) < MaxLen
            /\ ~ TooOld(s, p)
            /\ ~ \E x \in VT(s) : x.idx \notin p.S /\ Conflicts(x.w, p.R, CheckRanges)
            /\ ~ \E x \in PS(s) : Conflicts(x.w, p.R, CheckRanges)
            /\ Accept(s, base)
            /\ UNCHANGED replies
       [] p.kind = "claim" ->
            LET avail == {c \in p.cands :
                            /\ ~ \E x \in VT(s) : x.idx \notin p.S
                                   /\ (K(c[1]) \in DOMAIN x.w \/ LK(c[1]) \in DOMAIN x.w)
                            /\ ~ \E y \in PS(s) : K(c[1]) \in DOMAIN y.w \/ LK(c[1]) \in DOMAIN y.w}
                take  == IF avail = {} THEN {} ELSE {CHOOSE c \in avail : TRUE}
                R     == [keys |-> UNION {{K(c[1]), LK(c[1])} : c \in take}, preds |-> {}]
                w     == [k \in {LK(c[1]) : c \in take} |->
                            LET lv == CHOOSE c \in take : TRUE IN
                            <<lv[2], <<lv[2][1] + 1, "live">>>>]
            IN /\ Len(log[s]) < MaxLen
               /\ ~ TooOld(s, p)
               /\ Accept(s, [base EXCEPT !.R = R, !.obs = Obs(p.S, R), !.w = w])
               /\ UNCHANGED replies
       [] OTHER -> FALSE
  /\ UNCHANGED <<term, role, commitIndex, first, clog, win, iwin, net, nexec, nexp,
                 flushed, ncrash, running, lease, appBy, naIds, appIds>>

\* this submission's entry (same proposal, same term) is in some log
InAnyLog(ent) == \E r \in Servers : \E i \in 1..Len(log[r]) :
                   log[r][i].pid = ent.pid /\ log[r][i].term = ent.term

\* (Refusal) raft receives an entry the validator handed it: it appends it
\* if it is leader in the entry's term with its lease established, and
\* otherwise refuses it at receipt, never appending it
RemoveAt(sq, k) == SubSeq(sq, 1, k - 1) \o SubSeq(sq, k + 1, Len(sq))
Receipt(s, k) ==
  LET x  == inbox[s][k]
      ok == /\ role[s] = "L" /\ term[s] = x.ent.term
            /\ (~ LeaseWindow \/ lease[s] = term[s])
            /\ Len(log[s]) < MaxLen
      idx == Len(log[s]) + 1
      track == Refusal \in {"discard_na", "unknown_release"}
  IN
  /\ k \in 1..Len(inbox[s])
  /\ RecvOrder = "fifo" => k = 1
  /\ inbox' = [inbox EXCEPT ![s] = RemoveAt(inbox[s], k)]
  /\ IF ok
       THEN /\ AppendEntry(s, [x.ent EXCEPT !.dep = Deposed(s)])
            /\ slots' = [slots EXCEPT ![s] = {y \in slots[s] : y.id # x.id}]
            /\ appIds' = appIds \cup {x.id}
            /\ appBy' = IF track
                          THEN [appBy EXCEPT ![s] = appBy[s] \cup
                                 {[id |-> x.id, idx |-> idx, pid |-> x.ent.pid, term |-> x.ent.term]}]
                          ELSE appBy
            /\ UNCHANGED naIds
       ELSE /\ UNCHANGED <<log, vtab, itab, appIds, appBy>>
            /\ IF Refusal = "stay_pending"
                 THEN UNCHANGED <<slots, naIds>>
                 ELSE /\ slots' = [slots EXCEPT ![s] = {y \in slots[s] : y.id # x.id}]
                      /\ naIds' = naIds \cup {[id |-> x.id, bad |-> InAnyLog(x.ent)]}
  /\ UNCHANGED <<term, role, commitIndex, first, clog, win, iwin, net, nexec, replies, nexp,
                 pend, flushed, ncrash, running, lease>>

\* (Refusal) a fresh leader's first quorum-acked heartbeat establishes its lease
EstablishLease(s) ==
  /\ Refusal # "none" /\ LeaseWindow
  /\ role[s] = "L"
  /\ lease[s] # term[s]
  /\ \E Q \in Quorums : \A q \in Q : term[q] = term[s]
  /\ lease' = [lease EXCEPT ![s] = term[s]]
  /\ UNCHANGED <<term, role, log, commitIndex, first, vtab, itab, clog, win, iwin, net, nexec,
                 replies, nexp, pend, flushed, ncrash, running, slots, inbox, appBy, naIds, appIds>>

\* (Refusal = "discard_na", negative) a LogEntryDiscarded notification: an
\* entry s appended is no longer at its index in s's log; the validator
\* treats it as NotAppended and drops what its table holds at that index
Gone(s, a) == ~ (Len(log[s]) >= a.idx /\ log[s][a.idx].pid = a.pid /\ log[s][a.idx].term = a.term)
Discard(s, a) ==
  /\ Refusal = "discard_na"
  /\ a \in appBy[s]
  /\ Gone(s, a)
  /\ appBy' = [appBy EXCEPT ![s] = appBy[s] \ {a}]
  /\ vtab' = [vtab EXCEPT ![s] = {r \in vtab[s] : r.idx # a.idx}]
  /\ itab' = [itab EXCEPT ![s] = {r \in itab[s] : r.idx # a.idx}]
  /\ naIds' = naIds \cup {[id |-> a.id, bad |-> TRUE]}
  /\ UNCHANGED <<raftVars, clog, win, iwin, net, nexec, replies, nexp, pend, flushed, ncrash,
                 running, slots, inbox, lease, appIds>>

\* (Refusal = "unknown_release", negative) the response for an appended
\* entry never came; the validator releases its records
Unknown(s, a) ==
  /\ Refusal = "unknown_release"
  /\ a \in appBy[s]
  /\ role[s] = "L"
  /\ ~ Gone(s, a)
  /\ appBy' = [appBy EXCEPT ![s] = appBy[s] \ {a}]
  /\ vtab' = [vtab EXCEPT ![s] = {r \in vtab[s] : r.idx # a.idx}]
  /\ itab' = [itab EXCEPT ![s] = {r \in itab[s] : r.idx # a.idx}]
  /\ UNCHANGED <<raftVars, clog, win, iwin, net, nexec, replies, nexp, pend, flushed, ncrash,
                 running, slots, inbox, lease, naIds, appIds>>

\* HitReply = "immediate" (negative): a hit in the idempotency table is
\* answered at once from the table, whether or not the entry has committed
HitNow(s, p) ==
  /\ HitReply = "immediate"
  /\ role[s] = "L"
  /\ p \in net
  /\ \E x \in IT(s) :
       /\ x.ik = p.ik
       /\ x.idx <= Len(log[s])
       /\ LET r == [ik |-> p.ik, idx |-> x.idx,
                    res |-> IF log[s][x.idx].kind = "fail" THEN "failed" ELSE "ok"]
          IN r \notin replies /\ replies' = replies \cup {r}
  /\ UNCHANGED <<raftVars, extVars, vtab, itab, clog, win, iwin, net, nexec, nexp>>

\* Split: a leader appends the entry it validated earlier, in its current
\* term (it may have stepped down and been re-elected since); its tables
\* were not consulted again
AppendPending(s) ==
  /\ Split
  /\ role[s] = "L"
  /\ pend[s] # {}
  /\ Len(log[s]) < MaxLen
  /\ LET ent == CHOOSE x \in pend[s] : TRUE IN
     AppendEntry(s, [ent EXCEPT !.term = term[s], !.dep = Deposed(s)])
  /\ pend' = [pend EXCEPT ![s] = {}]
  /\ UNCHANGED <<term, role, commitIndex, first, clog, win, iwin, net, nexec, replies, nexp,
                 flushed, ncrash, running, rcvVars>>

\* the leader's sweeper: by its own clock, a live lease (as its own log,
\* uncommitted tail included, says) has expired
Sweep(s, t) ==
  LET S  == Prefix(Len(log[s]))
      lv == ValIn(log[s], S, LK(t))
      R  == [keys |-> {LK(t)}, preds |-> {}]
  IN
  /\ HasClaims
  /\ role[s] = "L"
  /\ nexp < MaxExpire
  /\ pend[s] = {}            \* (Split) the sweeper goes through the same pipeline
  /\ Len(log[s]) < MaxLen
  /\ lv[2] = "live"
  /\ AppendEntry(s, [term |-> term[s], kind |-> "sweep", ik |-> "", pid |-> <<"", 0>>,
                     S |-> S, R |-> R, obs |-> ObsIn(log[s], S, R),
                     w |-> (LK(t) :> <<lv, <<lv[1], "expired">>>>), fence |-> <<>>,
                     iw |-> iwin, wn |-> win, vterm |-> term[s], dep |-> Deposed(s)])
  /\ nexp' = nexp + 1
  /\ UNCHANGED <<term, role, commitIndex, first, clog, win, iwin, net, nexec, replies, extVars>>

(*************************** Raft *****************************************)

LastTerm(l) == IF l = << >> THEN 0 ELSE l[Len(l)].term
UpToDate(a, b) == LastTerm(a) > LastTerm(b) \/ (LastTerm(a) = LastTerm(b) /\ Len(a) >= Len(b))

RebuildV(s, l) == {[idx |-> i, term |-> l[i].term, w |-> l[i].w] :
                     i \in {j \in 1..Len(l) : IsTxn(l[j]) /\ j >= first[s]}}
RebuildI(l) == {[ik |-> l[i].ik, idx |-> i] : i \in {j \in 1..Len(l) : Keyed(l[j])}}

\* a replica that stops leading: with Rebuild its tables are dropped (they
\* are rebuilt if it leads again); without, they stay as they were
Drop(tab, X) == IF Rebuild THEN [x \in Servers |-> IF x \in X THEN {} ELSE tab[x]] ELSE tab

\* s (a follower) wins an election in a new term with the votes of Q,
\* appends its no-op, and (Rebuild) rebuilds both tables from its log
\* before accepting anything
BecomeLeader(s, Q) ==
  LET t == 1 + MaxSet({term[q] : q \in Q})
      nl == Append(log[s], Noop(t))
  IN
  /\ role[s] = "F"
  /\ s \in Q
  /\ t <= MaxTerm
  /\ Len(log[s]) < MaxLen
  /\ \A q \in Q : UpToDate(log[s], log[q])
  /\ term' = [x \in Servers |-> IF x \in Q THEN t ELSE term[x]]
  /\ role' = [x \in Servers |-> IF x = s THEN "L" ELSE IF x \in Q THEN "F" ELSE role[x]]
  /\ log' = [log EXCEPT ![s] = nl]
  /\ IF Rebuild
       THEN /\ vtab' = [Drop(vtab, Q) EXCEPT ![s] = RebuildV(s, nl)]
            /\ itab' = [Drop(itab, Q) EXCEPT ![s] = RebuildI(nl)]
       ELSE UNCHANGED <<vtab, itab>>
  /\ UNCHANGED <<commitIndex, first, clog, win, iwin, net, nexec, replies, nexp, extVars>>

\* a leader that hears of a higher term steps down
StepDown(s) ==
  /\ role[s] = "L"
  /\ \E x \in Servers : term[x] > term[s]
  /\ role' = [role EXCEPT ![s] = "F"]
  /\ vtab' = Drop(vtab, {s})
  /\ itab' = Drop(itab, {s})
  /\ UNCHANGED <<term, log, commitIndex, first, clog, win, iwin, net, nexec, replies, nexp, extVars>>

\* l replicates its whole log to f (f's conflicting suffix is overwritten);
\* rejected by f if f's term is higher. If f lacks entries l has truncated,
\* l installs a snapshot (f's log then starts where l's does).
AEWhole(l, f) ==
  /\ role[l] = "L"
  /\ l # f
  /\ term[l] >= term[f]
  /\ log[f] # log[l] \/ term[f] # term[l] \/ role[f] # "F"
  /\ log' = [log EXCEPT ![f] = log[l]]
  /\ term' = [term EXCEPT ![f] = term[l]]
  /\ role' = [role EXCEPT ![f] = "F"]
  /\ vtab' = Drop(vtab, {f})
  /\ itab' = Drop(itab, {f})
  /\ commitIndex' = [commitIndex EXCEPT ![f] = Max(commitIndex[f], commitIndex[l])]
  /\ first' = [first EXCEPT ![f] =
                 IF Len(log[f]) >= first[l] - 1
                    /\ SubSeq(log[f], 1, first[l] - 1) = SubSeq(log[l], 1, first[l] - 1)
                 THEN first[f] ELSE Max(first[f], first[l])]
  /\ UNCHANGED <<clog, win, iwin, net, nexec, replies, nexp, extVars>>

\* not "sync": pipelined AppendEntries. Any prefix 1..n of l's log may
\* arrive, in any order and more than once (several in flight). f keeps its
\* log if it already holds that prefix; otherwise it truncates at the
\* conflict and takes the prefix. What it appends is unflushed.
LCP(a, b) == MaxSet({k \in 0..Len(a) : k <= Len(b) /\ SubSeq(a, 1, k) = SubSeq(b, 1, k)})
AEPrefix(l, f, n) ==
  LET pre == SubSeq(log[l], 1, n)
      nl  == IF Len(log[f]) >= n /\ SubSeq(log[f], 1, n) = pre THEN log[f] ELSE pre
      ci  == Max(commitIndex[f], Min(commitIndex[l], n))
  IN
  /\ role[l] = "L"
  /\ l # f
  /\ term[l] >= term[f]
  /\ nl # log[f] \/ term[f] # term[l] \/ role[f] # "F" \/ ci # commitIndex[f]
  /\ log' = [log EXCEPT ![f] = nl]
  /\ term' = [term EXCEPT ![f] = term[l]]
  /\ role' = [role EXCEPT ![f] = "F"]
  /\ vtab' = Drop(vtab, {f})
  /\ itab' = Drop(itab, {f})
  /\ commitIndex' = [commitIndex EXCEPT ![f] = ci]
  /\ flushed' = [flushed EXCEPT ![f] = Min(flushed[f], LCP(log[f], nl))]
  /\ UNCHANGED <<first, clog, win, iwin, net, nexec, replies, nexp, pend, ncrash, running, rcvVars>>

AppendEntries(l, f) ==
  IF Durability = "sync" THEN AEWhole(l, f)
  ELSE \E n \in 1..Len(log[l]) : AEPrefix(l, f, n)

\* a replica fsyncs its whole log
Flush(s) ==
  /\ Durability # "sync"
  /\ flushed[s] < Len(log[s])
  /\ flushed' = [flushed EXCEPT ![s] = Len(log[s])]
  /\ UNCHANGED <<raftVars, vtab, itab, clog, win, iwin, net, nexec, replies, nexp, pend, ncrash,
                 running, rcvVars>>

\* a replica crashes and restarts: it keeps its term (persisted before it
\* votes or acks) and its flushed log; it loses the unflushed tail, its
\* commit index, its tables, and anything validated but not appended
Crash(s) ==
  /\ Durability # "sync"
  /\ ncrash < MaxCrash
  /\ log' = [log EXCEPT ![s] = SubSeq(log[s], 1, flushed[s])]
  /\ role' = [role EXCEPT ![s] = "F"]
  /\ commitIndex' = [commitIndex EXCEPT ![s] = 0]
  /\ vtab' = [vtab EXCEPT ![s] = {}]
  /\ itab' = [itab EXCEPT ![s] = {}]
  /\ pend' = [pend EXCEPT ![s] = {}]
  /\ ncrash' = ncrash + 1
  /\ UNCHANGED <<term, first, clog, win, iwin, net, nexec, replies, nexp, flushed, running, rcvVars>>

\* commit through a current-term quorum: the highest own-term index that a
\* quorum of replicas in l's term holds
\* what a replica's ack for index n means: it holds it ("sync",
\* "unflushed"), or it has flushed it ("flushed"; the leader counts itself
\* only once flushed)
Acked(q, n) == IF Durability = "flushed" THEN flushed[q] >= n ELSE Len(log[q]) >= n
CommitOK(l, n) ==
  /\ log[l][n].term = term[l]
  /\ \E Q \in Quorums : \A q \in Q :
       term[q] = term[l] /\ Acked(q, n) /\ log[q][n] = log[l][n]
AdvanceCommit(l) ==
  /\ role[l] = "L"
  /\ LET ns == {n \in (commitIndex[l] + 1)..Len(log[l]) : CommitOK(l, n)} IN
     /\ ns # {}
     /\ LET n == MaxSet(ns) IN
        /\ commitIndex' = [commitIndex EXCEPT ![l] = n]
        /\ clog' = IF n > Len(clog) THEN SubSeq(log[l], 1, n) ELSE clog
  /\ UNCHANGED <<term, role, log, first, vtab, itab, win, iwin, net, nexec, replies, nexp, extVars>>

\* the leader tells a client an entry's outcome once it knows it committed
\* (the original proposal's reply, or a retry that found the key)
Reply(s, i) ==
  /\ role[s] = "L"
  /\ i \in 1..commitIndex[s]
  /\ Keyed(log[s][i])
  /\ LET r == [ik |-> log[s][i].ik, idx |-> i,
               res |-> IF log[s][i].kind = "fail" THEN "failed" ELSE "ok"]
     IN /\ r \notin replies
        /\ replies' = replies \cup {r}
  /\ UNCHANGED <<raftVars, extVars, vtab, itab, clog, win, iwin, net, nexec, nexp>>

\* §10.4: a replica drops committed entries that every materializer applied
Truncate(s) ==
  /\ Truncation
  /\ \E n \in first[s]..Len(clog) :
       /\ n <= Len(log[s])
       /\ (Durability = "sync" \/ n <= flushed[s])
       /\ SubSeq(log[s], 1, n) = SubSeq(clog, 1, n)
       /\ first' = [first EXCEPT ![s] = n + 1]
  /\ UNCHANGED <<term, role, log, commitIndex, vtab, itab, clog, win, iwin, net, nexec,
                 replies, nexp, extVars>>

\* time passes: the windows' starts move up (never past what is committed;
\* the idempotency window is the longer one)
AdvanceWin ==
  /\ win < MaxWin /\ win < Len(clog)
  /\ win' = win + 1
  /\ UNCHANGED <<raftVars, extVars, vtab, itab, clog, iwin, net, nexec, replies, nexp>>
AdvanceIWin ==
  /\ iwin < MaxIWin /\ iwin < win
  /\ iwin' = iwin + 1
  /\ UNCHANGED <<raftVars, extVars, vtab, itab, clog, win, net, nexec, replies, nexp>>

Init ==
  /\ term = [s \in Servers |-> 0]
  /\ role = [s \in Servers |-> "F"]
  /\ log = [s \in Servers |-> << >>]
  /\ commitIndex = [s \in Servers |-> 0]
  /\ first = [s \in Servers |-> 1]
  /\ vtab = [s \in Servers |-> {}]
  /\ itab = [s \in Servers |-> {}]
  /\ clog = << >>
  /\ win = 0
  /\ iwin = 0
  /\ net = {}
  /\ nexec = [q \in Reqs |-> 0]
  /\ replies = {}
  /\ nexp = 0
  /\ pend = [s \in Servers |-> {}]
  /\ flushed = [s \in Servers |-> 0]
  /\ ncrash = 0
  /\ running = [q \in Reqs |-> {}]
  /\ slots = [s \in Servers |-> {}]
  /\ inbox = [s \in Servers |-> << >>]
  /\ lease = [s \in Servers |-> 0]
  /\ appBy = [s \in Servers |-> {}]
  /\ naIds = {}
  /\ appIds = {}

Next ==
  \/ \E s \in Servers :
       \/ \E Q \in Quorums : BecomeLeader(s, Q)
       \/ StepDown(s)
       \/ \E f \in Servers : AppendEntries(s, f)
       \/ AdvanceCommit(s)
       \/ \E p \in net : Process(s, p) \/ HitNow(s, p)
       \/ \E t \in Tasks : Sweep(s, t)
       \/ \E i \in 1..MaxLen : Reply(s, i)
       \/ Truncate(s)
       \/ AppendPending(s)
       \/ Flush(s)
       \/ Crash(s)
       \/ EstablishLease(s)
       \/ \E k \in 1..Len(inbox[s]) : Receipt(s, k)
       \/ \E a \in appBy[s] : Discard(s, a) \/ Unknown(s, a)
  \/ \E q \in Reqs : Execute(q) \/ Begin(q) \/ \E e \in running[q] : Run(q, e)
  \/ AdvanceWin
  \/ AdvanceIWin

Spec == Init /\ [][Next]_vars

\* liveness (Refusal configs): a stable leader exists (MaxTerm bounds the
\* elections), raft and the validator keep running, front ends re-send
LiveSpec ==
  /\ Spec
  /\ WF_vars(\E s \in Servers, Q \in Quorums : BecomeLeader(s, Q))
  /\ \A s \in Servers :
       /\ WF_vars(EstablishLease(s))
       /\ WF_vars(\E f \in Servers : AppendEntries(s, f))
       /\ WF_vars(AdvanceCommit(s))
       /\ WF_vars(\E p \in net : Process(s, p))
       /\ WF_vars(\E k \in 1..Len(inbox[s]) : Receipt(s, k))
       /\ WF_vars(\E i \in 1..MaxLen : Reply(s, i))
  /\ \A q \in Reqs : WF_vars(Execute(q))
\* every request eventually gets an outcome
EventuallyOutcome == \A q \in Reqs : <>(\E r \in replies : r.ik = q)

(*************************** properties ***********************************)

TypeOK ==
  /\ role \in [Servers -> {"L", "F"}]
  /\ \A s \in Servers : Len(log[s]) <= MaxLen /\ term[s] <= MaxTerm
  /\ win <= Len(clog) /\ iwin <= win

\* Raft: one leader per term; every replica's known-committed prefix is the
\* committed log; the newest leader holds the whole committed log
ElectionSafety ==
  \A s, t \in Servers : s # t /\ role[s] = "L" /\ role[t] = "L" => term[s] # term[t]
LogSafety ==
  \A s \in Servers :
    /\ commitIndex[s] <= Len(log[s])
    /\ commitIndex[s] <= Len(clog)
    /\ SubSeq(log[s], 1, commitIndex[s]) = SubSeq(clog, 1, commitIndex[s])
MaxTermNow == MaxSet({term[s] : s \in Servers})
LeaderCompleteness ==
  \A s \in Servers : role[s] = "L" /\ term[s] = MaxTermNow
     => Len(log[s]) >= Len(clog) /\ SubSeq(log[s], 1, Len(clog)) = clog

Txns == {i \in 1..Len(clog) : IsTxn(clog[i])}

(* 1. Committed history is serializable in log order: replaying committed  *)
(*    entries serially, each one's reads (keys and range predicates) at    *)
(*    its commit position equal what it read at S. A mutation's writes are *)
(*    a function of its reads, so serial replay produces the same writes.  *)
Serializable ==
  \A j \in Txns : Obs(Prefix(j - 1), clog[j].R) = clog[j].obs

(* 2. Nothing committed after a write it didn't see: no entry before it    *)
(*    in the log (committed or not when it was appended) whose stamp is    *)
(*    not in S wrote into its read set or a fact matching its predicates.  *)
NoUnseenConflict ==
  \A i, j \in Txns : i < j /\ i \notin clog[j].S => ~ Conflicts(clog[i].w, clog[j].R, TRUE)

(* 3. too_old: nothing is appended whose S's prefix is below the window    *)
(*    start at the time (every replica's log, uncommitted entries too).    *)
TooOldEnforced ==
  \A s \in Servers : \A i \in 1..Len(log[s]) :
    IsTxn(log[s][i]) => Pre(log[s][i].S) >= log[s][i].wn

(* 4. A leader's tables cover its whole log since the window start, so it  *)
(*    accepts nothing before they are rebuilt.                             *)
TablesComplete ==
  \A s \in Servers : role[s] = "L" =>
    /\ \A i \in (EffWin(s) + 1)..Len(log[s]) :
         IsTxn(log[s][i]) => [idx |-> i, term |-> log[s][i].term, w |-> log[s][i].w] \in VT(s)
    /\ \A i \in (iwin + 1)..Len(log[s]) :
         Keyed(log[s][i]) => [ik |-> log[s][i].ik, idx |-> i] \in IT(s)

(* 5. A deposed leader commits nothing, and every outcome a client is told *)
(*    from an entry is durable: the entry is in the committed log.         *)
DeposedCommitsNothing ==
  [][\A s \in Servers : Deposed(s) /\ role'[s] = "L" => commitIndex'[s] = commitIndex[s]]_vars
RepliesDurable ==
  \A r \in replies : r.idx > 0 =>
    /\ r.idx <= Len(clog)
    /\ Keyed(clog[r.idx]) /\ clog[r.idx].ik = r.ik
    /\ (r.res = "failed") = (clog[r.idx].kind = "fail")

(* With atomic validate+append the void rule never fires: every entry in  *)
(* every log, committed or not, carries the term it was validated in.      *)
VoidNeverFires ==
  Split \/ \A s \in Servers : \A i \in 1..Len(log[s]) : log[s][i].vterm = log[s][i].term

(* Weaker than RepliesDurable: no outcome a client was told is contradicted *)
(* by the committed log (it may still be pending).                         *)
RepliesNotContradicted ==
  \A r \in replies : 0 < r.idx /\ r.idx <= Len(clog) =>
    /\ Keyed(clog[r.idx]) /\ clog[r.idx].ik = r.ik
    /\ (r.res = "failed") = (clog[r.idx].kind = "fail")

(* Refusal: a slot becomes NotAppended only if its entry was never        *)
(* appended to any log (entries reach other logs only as copies of the     *)
(* one Receipt appends).                                                   *)
(* Checked when the slot is resolved: nothing with its proposal and term  *)
(* is in any log then, and this submission can no longer be appended.      *)
NASound == \A n \in naIds : ~ n.bad

(* 6. Writes to one key are causally ordered.                              *)
KeyCausal ==
  \A i, j \in Txns : i < j /\ (DOMAIN clog[i].w \cap DOMAIN clog[j].w) # {} => i \in clog[j].S

(* 7. An idempotency key commits at most once (within the idempotency      *)
(*    window), and a client is only ever told one outcome for a key.       *)
IdemOnce ==
  \A i, j \in 1..Len(clog) :
    i < j /\ Keyed(clog[i]) /\ Keyed(clog[j]) /\ clog[i].ik = clog[j].ik => i <= clog[j].iw
OneOutcome ==
  /\ \A r1, r2 \in replies :
       r1.ik = r2.ik /\ 0 < r1.idx /\ r1.idx < r2.idx /\ r2.idx <= Len(clog)
         => r1.idx <= clog[r2.idx].iw
  \* a failure reported without an entry must not hide a commit of the key
  /\ \A r \in replies : r.idx = 0 =>
       ~ \E j \in Txns : clog[j].ik = r.ik

(* Claims. ClaimGen(i, t): the lease generation entry i claimed t at.     *)
ClaimGen(i, t) == IF clog[i].kind = "claim" /\ LK(t) \in DOMAIN clog[i].w
                  THEN clog[i].w[LK(t)][2][1] ELSE 0
(* 8. No write fenced at generation g commits after a claim of the same    *)
(*    task at a generation greater than g.                                 *)
FenceHolds ==
  \A j \in Txns : clog[j].fence # <<>> =>
    ~ \E i \in Txns : i < j /\ ClaimGen(i, clog[j].fence[1]) > clog[j].fence[2]
(* 9. Two unexpired claims of one task never both commit: at a claim's     *)
(*    commit position the task's lease is not live.                        *)
NoDoubleLease ==
  \A j \in Txns : clog[j].kind = "claim" =>
    \A k \in DOMAIN clog[j].w : Val(Prefix(j - 1), k)[2] # "live"
(* 10. A completed task was completed by the holder of its newest lease.   *)
CompletedByHolder ==
  \A j \in Txns : clog[j].fence # <<>> =>
    Val(Prefix(j - 1), LK(clog[j].fence[1])) = <<clog[j].fence[2], "live">>

Sym == Permutations(Servers)

(*************************** programs used by the cfgs ********************)
ProgMain   == [q1 |-> [kind |-> "toggle", key |-> "a", max |-> 1],
               q2 |-> [kind |-> "guard", key |-> "b", max |-> 1],
               q3 |-> [kind |-> "create", max |-> 2]]
ProgSmall  == [q1 |-> [kind |-> "toggle", key |-> "a", max |-> 1],
               q2 |-> [kind |-> "guard", key |-> "b", max |-> 1]]
ProgTerms  == [q1 |-> [kind |-> "toggle", key |-> "a", max |-> 2],
               q2 |-> [kind |-> "guard", key |-> "b", max |-> 1]]
ProgRetry  == [q1 |-> [kind |-> "toggle", key |-> "a", max |-> 2],
               q2 |-> [kind |-> "toggle", key |-> "a", max |-> 1]]
ProgDiscard == [q1 |-> [kind |-> "toggle", key |-> "b", max |-> 1],
                q2 |-> [kind |-> "toggle", key |-> "a", max |-> 1],
                q3 |-> [kind |-> "toggle", key |-> "a", max |-> 1]]
ProgOne    == [q1 |-> [kind |-> "toggle", key |-> "a", max |-> 1]]
ProgToggles == [q1 |-> [kind |-> "toggle", key |-> "a", max |-> 1],
                q2 |-> [kind |-> "toggle", key |-> "a", max |-> 1]]
ProgGuard1 == [q1 |-> [kind |-> "guard", key |-> "b"],
               q2 |-> [kind |-> "toggle", key |-> "a"]]
\* two separate toggles of a, so a retry can read at a version older than
\* the original's and see a = v1 between them
ProgGuard2 == [q1 |-> [kind |-> "guard", key |-> "b", max |-> 2],
               q2 |-> [kind |-> "toggle", key |-> "a", max |-> 1],
               q3 |-> [kind |-> "toggle", key |-> "a", max |-> 1]]
\* worker 1 claims (c1) and completes (d1); so does worker 2. Only c1 may
\* be retried (MaxExec executions); the others run once.
ProgClaim  == [c1 |-> [kind |-> "claim"],
               d1 |-> [kind |-> "complete", claim |-> "c1", max |-> 1],
               c2 |-> [kind |-> "claim", max |-> 1],
               d2 |-> [kind |-> "complete", claim |-> "c2", max |-> 1]]
\* for leader changes: worker 1 claims and completes, worker 2 only claims
ProgClaimLC == [c1 |-> [kind |-> "claim", max |-> 1],
                d1 |-> [kind |-> "complete", claim |-> "c1", max |-> 1],
                c2 |-> [kind |-> "claim", max |-> 1]]

(*************************** reachability (sanity) ************************)
(* Each is EXPECTED TO FAIL (run with -continue): the scenario it negates  *)
(* is reachable, so the invariants above are not vacuous.                  *)
NeverExtras == \A j \in Txns : clog[j].S = Prefix(Pre(clog[j].S))
NeverOldTermCommittedLater ==   \* an earlier term's mutation committed by a later leader
  ~ \E i, j \in 1..Len(clog) :
      i < j /\ IsTxn(clog[i]) /\ clog[j].kind = "noop" /\ clog[j].term > clog[i].term
NeverOverwritten ==             \* an appended entry overwritten on some replica
  \A s \in Servers : \A i \in 1..Len(log[s]) :
    IsTxn(log[s][i]) /\ i <= Len(clog) => log[s][i] = clog[i]
NeverDeposedAppend ==           \* a deposed leader appends a mutation in its term
  ~ \E s \in Servers : Deposed(s) /\ \E i \in 1..Len(log[s]) :
       IsTxn(log[s][i]) /\ log[s][i].term = term[s] /\ log[s][i].kind # "sweep"
NeverBelowWindow == \A p \in net : p.kind = "fail" \/ Pre(p.S) >= win
NeverRetryAnswered ==           \* a retry's proposal reached a leader holding the key
  ~ \E q \in Reqs, s \in Servers :
      /\ role[s] = "L" /\ \E x \in IT(s) : x.ik = q
      /\ \E p \in net : p.ik = q /\ p.pid[2] = 2 /\ ~ \E i \in 1..Len(log[s]) : log[s][i].pid = p.pid
NeverExpiredReclaimed ==        \* a task re-claimed after its lease expired
  \A i \in Txns : \A t \in Tasks : ClaimGen(i, t) < 2
NeverLeaseLost ==               \* a completion fenced out (lease_lost or contended)
  ~ \E q \in Reqs : Prog[q].kind = "complete" /\ nexec[q] >= 1
      /\ \E r \in ClaimOf(q) : ClaimedBy(r) # {} /\
           \E t \in ClaimedBy(r) : \E i \in Txns : ClaimGen(i, t) > clog[r.idx].w[LK(t)][2][1]

(* Follow-up: shep-raft (atomic validate+append, void rule, flushed commit) *)
\* an entry appended by an already-deposed leader is committed (by a later
\* leader of a higher term whose log holds it)
NeverDeposedEntryCommitted == ~ \E i \in 1..Len(clog) : clog[i].dep
\* the same, for a mutation (not a no-op or sweep)
NeverDeposedTxnCommitted ==
  ~ \E i \in 1..Len(clog) : clog[i].dep /\ clog[i].kind = "mut"
\* an entry validated in one term sits in the committed log at another
NeverStaleValidated == ~ \E i \in 1..Len(clog) : clog[i].vterm # clog[i].term
\* a leader has committed an index it has not flushed itself
NeverLeaderUnflushedCommit ==
  ~ \E l \in Servers : role[l] = "L" /\ commitIndex[l] > flushed[l]
\* a crash lost a committed entry from a replica's log
NeverCrashLostCommitted ==
  ~ \E s \in Servers : ncrash > 0 /\ Len(log[s]) < Len(clog) /\ flushed[s] = Len(log[s])
                       /\ \E t \in Servers : Len(log[t]) >= Len(clog)
=============================================================================
