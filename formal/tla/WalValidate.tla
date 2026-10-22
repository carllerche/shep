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
  Truncation,   \* TRUE: replicas may truncate committed log prefixes
  TruncFix,     \* TRUE: a leader's too_old window starts no earlier than
                \*   the first entry still in its log
  FailMode      \* what a front end does when a re-execution ends in a
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

ASSUME TableAt \in {"append", "commit"} /\ IdemAt \in {"append", "commit"}
ASSUME FailMode \in {"local", "probe", "logged"}

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
  nexp          \* sweeper expiries so far

raftVars == <<term, role, log, commitIndex, first>>
vars == <<term, role, log, commitIndex, first, vtab, itab, clog, win, iwin,
          net, nexec, replies, nexp>>

(*************************** entries and versions *************************)

IsTxn(e) == e.kind \in {"mut", "claim", "sweep"}     \* writes data
Keyed(e) == e.kind \in {"mut", "claim", "fail"}      \* has an idempotency key

Noop(t) == [term |-> t, kind |-> "noop", ik |-> "", pid |-> <<"", 0>>, S |-> {},
            R |-> NoR, obs |-> NoObs, w |-> NoW, fence |-> <<>>, iw |-> 0, wn |-> 0]

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
  /\ UNCHANGED <<raftVars, vtab, itab, clog, win, iwin, nexp>>

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

Process(s, p) ==
  /\ role[s] = "L"
  /\ p \in net
  \* idempotency first: a key in the table gets that entry's outcome (by
  \* Reply, once it commits) and nothing is appended
  /\ ~ \E x \in IT(s) : x.ik = p.ik
  /\ LET base == [term |-> term[s], kind |-> p.kind, ik |-> p.ik, pid |-> p.pid,
                  S |-> p.S, R |-> p.R, obs |-> p.obs, w |-> p.w, fence |-> p.fence,
                  iw |-> iwin, wn |-> win]
     IN
     CASE p.kind = "fail" /\ FailMode = "logged" ->
            /\ Len(log[s]) < MaxLen
            /\ AppendEntry(s, base)
            /\ UNCHANGED replies
       [] p.kind = "fail" /\ FailMode = "probe" ->
            /\ Confirmed(s)
            /\ replies' = replies \cup {[ik |-> p.ik, idx |-> 0, res |-> "failed"]}
            /\ UNCHANGED <<log, vtab, itab>>
       [] p.kind = "mut" ->
            /\ Len(log[s]) < MaxLen
            /\ ~ TooOld(s, p)
            /\ ~ \E x \in VT(s) : x.idx \notin p.S /\ Conflicts(x.w, p.R, CheckRanges)
            /\ AppendEntry(s, base)
            /\ UNCHANGED replies
       [] p.kind = "claim" ->
            LET avail == {c \in p.cands :
                            ~ \E x \in VT(s) : x.idx \notin p.S
                                 /\ (K(c[1]) \in DOMAIN x.w \/ LK(c[1]) \in DOMAIN x.w)}
                take  == IF avail = {} THEN {} ELSE {CHOOSE c \in avail : TRUE}
                R     == [keys |-> UNION {{K(c[1]), LK(c[1])} : c \in take}, preds |-> {}]
                w     == [k \in {LK(c[1]) : c \in take} |->
                            LET lv == CHOOSE c \in take : TRUE IN
                            <<lv[2], <<lv[2][1] + 1, "live">>>>]
            IN /\ Len(log[s]) < MaxLen
               /\ ~ TooOld(s, p)
               /\ AppendEntry(s, [base EXCEPT !.R = R, !.obs = Obs(p.S, R), !.w = w])
               /\ UNCHANGED replies
       [] OTHER -> FALSE
  /\ UNCHANGED <<term, role, commitIndex, first, clog, win, iwin, net, nexec, nexp>>

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
  /\ Len(log[s]) < MaxLen
  /\ lv[2] = "live"
  /\ AppendEntry(s, [term |-> term[s], kind |-> "sweep", ik |-> "", pid |-> <<"", 0>>,
                     S |-> S, R |-> R, obs |-> ObsIn(log[s], S, R),
                     w |-> (LK(t) :> <<lv, <<lv[1], "expired">>>>), fence |-> <<>>,
                     iw |-> iwin, wn |-> win])
  /\ nexp' = nexp + 1
  /\ UNCHANGED <<term, role, commitIndex, first, clog, win, iwin, net, nexec, replies>>

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
  /\ UNCHANGED <<commitIndex, first, clog, win, iwin, net, nexec, replies, nexp>>

\* a leader that hears of a higher term steps down
StepDown(s) ==
  /\ role[s] = "L"
  /\ \E x \in Servers : term[x] > term[s]
  /\ role' = [role EXCEPT ![s] = "F"]
  /\ vtab' = Drop(vtab, {s})
  /\ itab' = Drop(itab, {s})
  /\ UNCHANGED <<term, log, commitIndex, first, clog, win, iwin, net, nexec, replies, nexp>>

\* l replicates its whole log to f (f's conflicting suffix is overwritten);
\* rejected by f if f's term is higher. If f lacks entries l has truncated,
\* l installs a snapshot (f's log then starts where l's does).
AppendEntries(l, f) ==
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
  /\ UNCHANGED <<clog, win, iwin, net, nexec, replies, nexp>>

\* commit through a current-term quorum: the highest own-term index that a
\* quorum of replicas in l's term holds
CommitOK(l, n) ==
  /\ log[l][n].term = term[l]
  /\ \E Q \in Quorums : \A q \in Q :
       term[q] = term[l] /\ Len(log[q]) >= n /\ log[q][n] = log[l][n]
AdvanceCommit(l) ==
  /\ role[l] = "L"
  /\ LET ns == {n \in (commitIndex[l] + 1)..Len(log[l]) : CommitOK(l, n)} IN
     /\ ns # {}
     /\ LET n == MaxSet(ns) IN
        /\ commitIndex' = [commitIndex EXCEPT ![l] = n]
        /\ clog' = IF n > Len(clog) THEN SubSeq(log[l], 1, n) ELSE clog
  /\ UNCHANGED <<term, role, log, first, vtab, itab, win, iwin, net, nexec, replies, nexp>>

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
  /\ UNCHANGED <<raftVars, vtab, itab, clog, win, iwin, net, nexec, nexp>>

\* §10.4: a replica drops committed entries that every materializer applied
Truncate(s) ==
  /\ Truncation
  /\ \E n \in first[s]..Len(clog) :
       /\ n <= Len(log[s])
       /\ SubSeq(log[s], 1, n) = SubSeq(clog, 1, n)
       /\ first' = [first EXCEPT ![s] = n + 1]
  /\ UNCHANGED <<term, role, log, commitIndex, vtab, itab, clog, win, iwin, net, nexec,
                 replies, nexp>>

\* time passes: the windows' starts move up (never past what is committed;
\* the idempotency window is the longer one)
AdvanceWin ==
  /\ win < MaxWin /\ win < Len(clog)
  /\ win' = win + 1
  /\ UNCHANGED <<raftVars, vtab, itab, clog, iwin, net, nexec, replies, nexp>>
AdvanceIWin ==
  /\ iwin < MaxIWin /\ iwin < win
  /\ iwin' = iwin + 1
  /\ UNCHANGED <<raftVars, vtab, itab, clog, win, net, nexec, replies, nexp>>

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

Next ==
  \/ \E s \in Servers :
       \/ \E Q \in Quorums : BecomeLeader(s, Q)
       \/ StepDown(s)
       \/ \E f \in Servers : AppendEntries(s, f)
       \/ AdvanceCommit(s)
       \/ \E p \in net : Process(s, p)
       \/ \E t \in Tasks : Sweep(s, t)
       \/ \E i \in 1..MaxLen : Reply(s, i)
       \/ Truncate(s)
  \/ \E q \in Reqs : Execute(q)
  \/ AdvanceWin
  \/ AdvanceIWin

Spec == Init /\ [][Next]_vars

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
Deposed(s) == role[s] = "L" /\ \E Q \in Quorums : \A q \in Q : term[q] > term[s]
DeposedCommitsNothing ==
  [][\A s \in Servers : Deposed(s) /\ role'[s] = "L" => commitIndex'[s] = commitIndex[s]]_vars
RepliesDurable ==
  \A r \in replies : r.idx > 0 =>
    /\ r.idx <= Len(clog)
    /\ Keyed(clog[r.idx]) /\ clog[r.idx].ik = r.ik
    /\ (r.res = "failed") = (clog[r.idx].kind = "fail")

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
=============================================================================
