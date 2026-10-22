---------------------------- MODULE ReplicatedLog ----------------------------
(***************************************************************************)
(* The generic replicated-log contract that `shep-raft` offers embedders   *)
(* (crates/shep-raft/src/{machine,proposer,state_machine}.rs). Background: *)
(* design §9.6 (the controller), §9.9 (the runtime), §10.3 (admission,     *)
(* idempotency), §10.4 (snapshots).                                        *)
(*                                                                         *)
(* The embedder's `Machine`:                                               *)
(*   admit(ctx, proposal) -> Command | Rejection   leader only, pre-append *)
(*   lead(term, floor)     drop leader-side state; entries above `floor`   *)
(*                         are replayed next                               *)
(*   replay(entry)         every non-void entry above the floor, oldest    *)
(*                         first, committed or not                         *)
(*   resolve(seq, Appended(i) | Void(i) | NotAppended | Unknown)           *)
(*   apply(entry)          every replica, log order, never a void entry    *)
(*   snapshot / restore                                                    *)
(* shep-raft owns: the envelope (validated_term, commit_time,              *)
(* idempotency_floor, key) and the void rule; the idempotency table        *)
(* key -> (index, commit_time, response), forgotten by each applied        *)
(* entry's floor and kept in snapshots; the proposer's key map (Waiting:   *)
(* handed to Raft this term; AtIndex: found unapplied in the log at        *)
(* rebuild); commit times (max(last, clock), seeded at rebuild); oversize  *)
(* commands (resolve NotAppended, never appended); refusal at receipt      *)
(* (openraft refuses, never appending, unless it leads with a valid lease; *)
(* the lease can lapse and come back within a term).                       *)
(*                                                                         *)
(* Raft is abstract as in WalValidate.tla: terms, elections with the       *)
(* up-to-date check, a new leader's no-op, whole-log AppendEntries (a      *)
(* follower's conflicting suffix is overwritten), commit through a         *)
(* current-term quorum, deposed leaders, sync durability. Added here:      *)
(* local snapshots that purge the log, snapshot install, restarts (the log *)
(* and the committed index survive; everything in memory is lost: applied *)
(* state goes back to the snapshot, the proposer and the machine's leader  *)
(* side are gone, entries handed to Raft but not received are dropped).    *)
(*                                                                         *)
(* The proposer and Raft are separate: `Process` admits and hands the      *)
(* entry to Raft (`inbox`); `Receipt` is Raft taking it, appending under   *)
(* its CURRENT term or refusing it. In between, the replica can lose and   *)
(* win leadership (the entry is then appended under a later term: void),   *)
(* or its lease can lapse and return (a refusal followed by an append).    *)
(* Fates reach the proposer asynchronously (`ResolveFate`), and are        *)
(* dropped if the proposer has rebuilt since (generation = term).          *)
(*                                                                         *)
(* Three embedders (constant Emb):                                         *)
(*   "occ"  history-dependent validation, as PartitionSm: a proposal names *)
(*          a read version rv and an object; admit rejects it if a         *)
(*          leader-side slot not seen at rv (index > rv, or pending)       *)
(*          wrote the object, and (FloorRule) if rv is below the floor.    *)
(*   "incr" leader-resolved operations (§8 field operations): the command  *)
(*          carries a value derived at admit from the newest slot on its   *)
(*          object (or from the proposal's base read at rv), so it         *)
(*          PRESUMES earlier admissions took effect.                       *)
(*   "ctl"  the WAL controller: admit places replicas against the APPLIED  *)
(*          state (stale on a leader); apply re-checks.                    *)
(*                                                                         *)
(* Leader-side state is the set of slots: replayed entries and this        *)
(* term's admissions not resolved NotAppended, with their status.          *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  Servers, MaxTerm, MaxLen,
  Prog,          \* request (= its idempotency key) -> what it asks for
  MaxSends,      \* proposals per request (each re-sendable forever)
  Emb,           \* "occ" | "incr" | "ctl"
  MaxTime,       \* each replica's clock starts anywhere in 0..MaxTime
  Ticks,         \* clocks also advance (up to MaxTime); FALSE: fixed skew
  W,             \* idempotency window (commit-time units)
  AW,            \* admission window (time) for the rebuild walk
  WinEntries,    \* admission_window_entries for the rebuild walk
  FloorMode,     \* "impl": the rebuild walk of proposer.rs; "any": any
                 \*   floor between the log's start and the applied index
  FloorRule,     \* the embedder refuses a proposal whose rv < floor
  PendingRule,   \* (incr) "presume": derive from pending slots;
                 \*   "block": a pending slot on the object rejects
  Evict,         \* the embedder may evict resolved slots, raising its floor
  VoidRule,      \* "all": void judged on every replica; "leader": only on
                 \*   the replica that appended it (negative)
  Seed,          \* commit-time seed at rebuild: "fix" max(applied max,
                 \*   replayed tail); "tail": replayed tail only (the code
                 \*   before the fix); "none": 0 (negative)
  MctVoid,       \* the applied max counts void entries (the landed fix)
  LeadDrop,      \* lead() drops earlier leader-side state
  ReplayUncommitted, \* replay covers uncommitted entries too
  ReplayVoid,    \* replay includes void entries (negative)
  DiscardNA,     \* appended-then-truncated resolves NotAppended (negative)
  ResolveTwice,  \* a timeout resolves Unknown, then the fate again (neg.)
  WaitInFlight,  \* a retry of a key handed to Raft this term waits
  SnapKeys,      \* snapshots carry the idempotency table
  Snapshots,     \* replicas take snapshots and purge their logs
  MaxCrash,      \* restarts
  LeaseToggle,   \* a leader's lease can lapse and return within its term
  Oversize,      \* a command can be too large (NotAppended at once)
  CtlId,         \* (ctl) "apply": ids assigned by apply (the code);
                 \*   "admit": chosen by admit from the applied state
  CtlRecheck     \* (ctl) apply re-checks a create (member, id free)

ASSUME Emb \in {"occ", "incr", "ctl"}
ASSUME FloorMode \in {"impl", "any"} /\ Seed \in {"fix", "tail", "none"}
ASSUME VoidRule \in {"all", "leader"} /\ PendingRule \in {"presume", "block"}
ASSUME CtlId \in {"apply", "admit"}

Reqs == DOMAIN Prog
Quorums == {Q \in SUBSET Servers : 2 * Cardinality(Q) > Cardinality(Servers)}
Max(a, b) == IF a > b THEN a ELSE b
Min(a, b) == IF a < b THEN a ELSE b
MaxS(X) == IF X = {} THEN 0 ELSE CHOOSE x \in X : \A y \in X : y <= x
Range(sq) == {sq[i] : i \in 1..Len(sq)}
Sub0(a, b) == IF a >= b THEN a - b ELSE 0     \* saturating_sub
Nodes == {1, 2}                               \* controller: WAL nodes
Objs == {Prog[q].n : q \in {r \in Reqs : "n" \in DOMAIN Prog[r]}}

NoC == [n |-> "", op |-> "", rep |-> 0, id |-> 0]

VARIABLES
  term, role, log, commitIndex, first, lease,     \* Raft
  applied, ms, kt, ord, mct, hist, snap,          \* replicated state
  lt, lct, nseq, lk, inbox, appBy, ls,            \* leader side
  clog, resv, dbl, naSet,                         \* ghosts
  clk, ncrash, net, nsend                         \* environment

raftV == <<term, role, log, commitIndex, first, lease>>
smV   == <<applied, ms, kt, ord, mct, hist, snap>>
prV   == <<lt, lct, nseq, lk, inbox, appBy, ls>>
ghV   == <<clog, resv, dbl, naSet>>
envV  == <<clk, ncrash, net, nsend>>
vars  == <<raftV, smV, prV, ghV, envV>>

(*************************** entries **************************************)
\* o: origin <<replica, term admitted in, seq>>; ap: the replica that
\* appended it; vt: validated_term; fl: idempotency floor (ct - W);
\* rv, d: the embedder's read version and derived value
Noop(s, t) == [term |-> t, kind |-> "noop", vt |-> t, o |-> <<s, t, 99>>, ap |-> s,
               k |-> "none", c |-> NoC, rv |-> 0, ct |-> 0, fl |-> 0, d |-> 0]
IsCmd(e) == e.kind = "cmd"
Void(e) == IsCmd(e) /\ e.vt # e.term
NV(e) == IsCmd(e) /\ ~ Void(e)
\* how replica s judges an entry when applying it
VoidAt(s, e) == Void(e) /\ (VoidRule = "all" \/ e.ap = s)

(*************************** the embedders' replicated state **************)
\* ctl: parts are <<id, replica, creating request>>
InitMs == IF Emb = "ctl" THEN [mem |-> Nodes, parts |-> {}, next |-> 1]
          ELSE [x \in Objs |-> 0]
Load(m, n) == Cardinality({p \in m.parts : p[2] = n})
LeastLoaded(m) == CHOOSE n \in m.mem : \A x \in m.mem :
                    Load(m, n) < Load(m, x) \/ (Load(m, n) = Load(m, x) /\ n <= x)
IdTaken(m, id) == \E p \in m.parts : p[1] = id

\* apply: deterministic, a function of the state and the entry
ApplyMs(m, e) ==
  IF Emb # "ctl" THEN [m EXCEPT ![e.c.n] = @ + 1]
  ELSE IF e.c.op = "remove"
    THEN IF \E p \in m.parts : p[2] = e.c.rep THEN m ELSE [m EXCEPT !.mem = @ \ {e.c.rep}]
  ELSE IF CtlRecheck /\ (e.c.rep \notin m.mem \/ (CtlId = "admit" /\ IdTaken(m, e.c.id)))
    THEN m
  ELSE LET id == IF CtlId = "apply" THEN m.next ELSE e.c.id IN
       [m EXCEPT !.parts = @ \cup {<<id, e.c.rep, e.k>>}, !.next = Max(@, id + 1)]
ApplyResp(m, e) ==
  IF Emb # "ctl" THEN <<"ok", 0>>
  ELSE IF e.c.op = "remove"
    THEN IF \E p \in m.parts : p[2] = e.c.rep THEN <<"rej", 0>> ELSE <<"ok", 0>>
  ELSE IF CtlRecheck /\ (e.c.rep \notin m.mem \/ (CtlId = "admit" /\ IdTaken(m, e.c.id)))
    THEN <<"rej", 0>>
  ELSE <<"id", IF CtlId = "apply" THEN m.next ELSE e.c.id>>

(*************************** the idempotency table ************************)
\* Keys::forget_before: pop from the front while the record there is
\* superseded, gone, or older than the floor; stop at the first that isn't
RECURSIVE Forget(_, _, _)
Forget(t, o, f) ==
  IF o = << >> THEN <<t, o>>
  ELSE LET h == Head(o) IN
       IF h[2] \notin DOMAIN t \/ t[h[2]].i # h[1] THEN Forget(t, Tail(o), f)
       ELSE IF t[h[2]].ct >= f THEN <<t, o>>
       ELSE Forget([x \in DOMAIN t \ {h[2]} |-> t[x]], Tail(o), f)
SetKey(t, k, r) == [x \in DOMAIN t \cup {k} |-> IF x = k THEN r ELSE t[x]]
DelKey(t, k) == [x \in DOMAIN t \ {k} |-> t[x]]
NoKeys == [x \in {} |-> 0]
Live(t, o) == SelectSeq(o, LAMBDA h : h[2] \in DOMAIN t /\ t[h[2]].i = h[1])

(*************************** clients **************************************)
\* a client reads at a committed version rv; b is its base read (incr)
Base(rv, q) == Cardinality({i \in 1..rv : NV(clog[i]) /\ clog[i].c.n = Prog[q].n})
RVs == IF Emb = "ctl" THEN {0} ELSE 0..Len(clog)

Send(q) ==
  /\ nsend[q] < MaxSends
  /\ \E rv \in RVs :
       LET p == [q |-> q, rv |-> rv, b |-> IF Emb = "ctl" THEN 0 ELSE Base(rv, q)] IN
       /\ p \notin net
       /\ net' = net \cup {p}
  /\ nsend' = [nsend EXCEPT ![q] = @ + 1]
  /\ UNCHANGED <<raftV, smV, prV, ghV, clk, ncrash>>

(*************************** leader: admit ********************************)
Pend(sl) == sl.st \in {"p", "u"}           \* no index known: unseen by all
Unseen(sl, rv) == Pend(sl) \/ sl.i > rv
LS(s) == ls[s].slots

AdmitOK(s, p) ==
  CASE Emb = "occ" ->
         /\ FloorRule => p.rv >= ls[s].fl
         /\ ~ \E sl \in LS(s) : sl.st # "v" /\ sl.c.n = Prog[p.q].n /\ Unseen(sl, p.rv)
    [] Emb = "incr" ->
         /\ FloorRule => p.rv >= ls[s].fl
         /\ PendingRule = "block" => ~ \E sl \in LS(s) : Pend(sl) /\ sl.c.n = Prog[p.q].n
    [] Emb = "ctl" ->
         Prog[p.q].op = "remove" \/ ms[s].mem # {}

\* the command admit returns (ctl: placed against the APPLIED state)
Cmd(s, p) ==
  CASE Emb = "ctl" /\ Prog[p.q].op = "create" ->
         [n |-> "", op |-> "create", rep |-> LeastLoaded(ms[s]),
          id |-> IF CtlId = "admit" THEN ms[s].next ELSE 0]
    [] Emb = "ctl" -> [n |-> "", op |-> "remove", rep |-> Prog[p.q].rep, id |-> 0]
    [] OTHER -> [NoC EXCEPT !.n = Prog[p.q].n]
\* (incr) the value derived at admit: one past the newest unseen slot's
Derived(s, p) ==
  IF Emb # "incr" THEN 0
  ELSE LET above == {sl \in LS(s) : sl.st # "v" /\ sl.c.n = Prog[p.q].n /\ Unseen(sl, p.rv)}
       IN IF above = {} THEN p.b + 1 ELSE MaxS({sl.d : sl \in above}) + 1

\* the proposer: needs leader-side state for its current term; answers a
\* key decided in the applied table (no action: the table is checked by
\* KtDurable); makes a retry of a key Waiting / AtIndex wait; else admits
Process(s, p) ==
  /\ role[s] = "L" /\ lt[s] = term[s]
  /\ nseq[s] < MaxLen       \* bound: admissions per generation (refused and
                            \* oversize ones don't append, so bound them too)
  /\ p \in net
  /\ p.q \notin DOMAIN kt[s]
  /\ ~ (p.q \in DOMAIN lk[s] /\ (lk[s][p.q].w = "at" \/ WaitInFlight))
  /\ AdmitOK(s, p)
  /\ LET ct  == Max(lct[s], clk[s])
         seq == nseq[s]
         o   == <<s, term[s], seq>>
         ent == [term |-> term[s], kind |-> "cmd", vt |-> term[s], o |-> o, ap |-> s,
                 k |-> p.q, c |-> Cmd(s, p), rv |-> p.rv, ct |-> ct, fl |-> Sub0(ct, W),
                 d |-> Derived(s, p)]
     IN
     /\ lct' = [lct EXCEPT ![s] = ct]
     /\ nseq' = [nseq EXCEPT ![s] = @ + 1]
     /\ \/ /\ inbox' = [inbox EXCEPT ![s] = Append(@, ent)]
           /\ lk' = [lk EXCEPT ![s] = SetKey(@, p.q, [w |-> "w", seq |-> seq])]
           /\ ls' = [ls EXCEPT ![s].slots = @ \cup
                       {[o |-> o, c |-> ent.c, d |-> ent.d, st |-> "p", i |-> 0]}]
           /\ UNCHANGED <<resv, dbl, naSet>>
        \* too large: resolve(NotAppended), never handed to Raft
        \/ /\ Oversize
           /\ resv' = resv \cup {[o |-> o, f |-> "na", i |-> 0]}
           /\ naSet' = naSet \cup {o}
           /\ UNCHANGED <<inbox, lk, ls, dbl>>
  /\ UNCHANGED <<raftV, smV, lt, appBy, clog, envV>>

\* a fate reaches the proposer: dropped unless it's still the generation
\* (term) that admitted it; else the machine's resolve, and the Waiting key
\* is released
Upd(S, o, f, i) == IF f = "na" THEN {x \in S : x.o # o}
                   ELSE {IF x.o = o THEN [x EXCEPT !.st = f, !.i = i] ELSE x : x \in S}
Deliver(s, o, k, f, i) ==
  IF lt[s] = o[2]
    THEN /\ ls' = [ls EXCEPT ![s].slots = Upd(@, o, f, i)]
         /\ lk' = IF k \in DOMAIN lk[s] /\ lk[s][k] = [w |-> "w", seq |-> o[3]]
                    THEN [lk EXCEPT ![s] = DelKey(@, k)] ELSE lk
         /\ resv' = resv \cup {[o |-> o, f |-> f, i |-> i]}
         /\ dbl' = (dbl \/ o \in {r.o : r \in resv})
         /\ naSet' = IF f = "na" THEN naSet \cup {o} ELSE naSet
    ELSE UNCHANGED <<ls, lk, resv, dbl, naSet>>

\* Raft receives the oldest entry handed to it: appends it under its current
\* term if it leads with a valid lease, else refuses it (NotAppended)
Receipt(s) ==
  /\ inbox[s] # << >>
  /\ LET x == Head(inbox[s])
         ok == role[s] = "L" /\ lease[s] /\ Len(log[s]) < MaxLen
     IN
     /\ inbox' = [inbox EXCEPT ![s] = Tail(@)]
     /\ IF ok
          THEN /\ log' = [log EXCEPT ![s] = Append(@, [x EXCEPT !.term = term[s], !.ap = s])]
               /\ appBy' = [appBy EXCEPT ![s] = @ \cup {[o |-> x.o, k |-> x.k, i |-> Len(log[s]) + 1]}]
               /\ UNCHANGED <<ls, lk, resv, dbl, naSet>>
          ELSE /\ Deliver(s, x.o, x.k, "na", 0)
               /\ UNCHANGED <<log, appBy>>
  /\ UNCHANGED <<term, role, commitIndex, first, lease, smV, lt, lct, nseq, clog, envV>>

\* what became of an entry s appended: applied here (Appended / Void: its
\* responder fires at apply, see Apply), or no longer at its index,
\* truncated or purged by a snapshot install (LogEntryDiscarded: Unknown,
\* or NotAppended with DiscardNA)
Gone(s, a) == ~ (a.i # 0 /\ a.i <= Len(log[s]) /\ log[s][a.i].o = a.o)
FateOf(s, a) ==
  IF ~ Gone(s, a)
    THEN IF a.i <= applied[s] THEN (IF VoidAt(s, log[s][a.i]) THEN "v" ELSE "a") ELSE "none"
    ELSE IF DiscardNA THEN "na" ELSE "u"
ResolveFate(s, a) ==
  /\ a \in appBy[s]
  /\ Gone(s, a)
  /\ appBy' = [appBy EXCEPT ![s] = @ \ {a}]
  /\ Deliver(s, a.o, a.k, FateOf(s, a), 0)
  /\ UNCHANGED <<raftV, smV, lt, lct, nseq, inbox, clog, envV>>
\* (ResolveTwice, negative) the commit timeout resolves Unknown; the real
\* fate is still resolved when it arrives
TimeoutUnknown(s, a) ==
  /\ ResolveTwice
  /\ a \in appBy[s]
  /\ FateOf(s, a) = "none"
  /\ a.o \notin {r.o : r \in resv}
  /\ Deliver(s, a.o, a.k, "u", 0)
  /\ UNCHANGED <<raftV, smV, lt, lct, nseq, inbox, appBy, clog, envV>>

\* the embedder evicts resolved slots up to nf (PartitionSm's Table::evict),
\* never past an entry of a pending slot
Evicting(s) ==
  /\ Evict
  /\ role[s] = "L" /\ lt[s] = term[s]
  /\ \E nf \in (ls[s].fl + 1)..Len(log[s]) :
       /\ \A sl \in LS(s) : Pend(sl) => ~ \E i \in 1..nf : log[s][i].o = sl.o
       /\ ls' = [ls EXCEPT ![s] = [@ EXCEPT !.fl = nf,
                   !.slots = {sl \in @ : Pend(sl) \/ sl.i > nf}]]
  /\ UNCHANGED <<raftV, smV, lt, lct, nseq, lk, inbox, appBy, ghV, envV>>

(*************************** leader: rebuild ******************************)
\* proposer.rs rebuild: walk the log from the newest entry; stop at an
\* APPLIED entry that is old (a command whose commit time is before
\* clock - admission window) or once WinEntries entries were taken; never
\* below the log's start. floor = the stopping index.
Old(s, e) == IsCmd(e) /\ e.ct + AW < clk[s]
StopAt(s, i) == i <= applied[s] /\ (Old(s, log[s][i]) \/ Len(log[s]) - i >= WinEntries)
ImplFloor(s) == MaxS({first[s] - 1} \cup {i \in first[s]..Len(log[s]) : StopAt(s, i)})
Floors(s) == IF FloorMode = "impl" THEN {ImplFloor(s)}
             ELSE (first[s] - 1)..Min(applied[s], Len(log[s]))

Rebuild(s) ==
  /\ role[s] = "L" /\ lt[s] # term[s]
  /\ \E fl \in Floors(s) :
       LET L    == Len(log[s])
           Rep  == {i \in (fl + 1)..L : IsCmd(log[s][i]) /\ (ReplayVoid \/ ~ Void(log[s][i]))
                                       /\ (ReplayUncommitted \/ i <= commitIndex[s])}
           KI   == {i \in (fl + 1)..L : NV(log[s][i]) /\ i > applied[s]
                                       /\ (ReplayUncommitted \/ i <= commitIndex[s])}
           tail == MaxS({log[s][i].ct : i \in {j \in (fl + 1)..L : IsCmd(log[s][j])}})
           seed == CASE Seed = "fix" -> Max(mct[s], tail)
                     [] Seed = "tail" -> tail
                     [] OTHER -> 0
           slots == {[o |-> log[s][i].o, c |-> log[s][i].c, d |-> log[s][i].d, st |-> "r", i |-> i]
                       : i \in Rep}
       IN
       /\ ls' = [ls EXCEPT ![s] = [t |-> term[s], fl |-> fl,
                                   slots |-> IF LeadDrop THEN slots ELSE slots \cup ls[s].slots]]
       /\ lk' = [lk EXCEPT ![s] = [k \in {log[s][i].k : i \in KI} |->
                    [w |-> "at", seq |-> CHOOSE i \in KI : log[s][i].k = k]]]
       /\ lct' = [lct EXCEPT ![s] = seed]
  /\ lt' = [lt EXCEPT ![s] = term[s]]
  /\ nseq' = [nseq EXCEPT ![s] = 0]
  /\ UNCHANGED <<raftV, smV, inbox, appBy, ghV, envV>>

(*************************** replicas: apply, snapshot, restart ************)
Apply(s) ==
  /\ applied[s] < commitIndex[s]
  /\ LET i == applied[s] + 1
         e == log[s][i]
     IN
     /\ applied' = [applied EXCEPT ![s] = i]
     /\ IF ~ IsCmd(e)
          THEN /\ hist' = [hist EXCEPT ![s] = Append(@, <<"noop", 0>>)]
               /\ UNCHANGED <<ms, kt, ord, mct>>
        ELSE IF VoidAt(s, e)
          THEN /\ hist' = [hist EXCEPT ![s] = Append(@, <<"void", 0>>)]
               /\ mct' = IF MctVoid THEN [mct EXCEPT ![s] = Max(@, e.ct)] ELSE mct
               /\ UNCHANGED <<ms, kt, ord>>
        ELSE LET fo == Forget(kt[s], ord[s], e.fl)
                 r  == ApplyResp(ms[s], e)
             IN /\ ms' = [ms EXCEPT ![s] = ApplyMs(@, e)]
                /\ kt' = [kt EXCEPT ![s] = SetKey(fo[1], e.k, [i |-> i, ct |-> e.ct, r |-> r])]
                /\ ord' = [ord EXCEPT ![s] = Append(fo[2], <<i, e.k>>)]
                /\ mct' = [mct EXCEPT ![s] = Max(@, e.ct)]
                /\ hist' = [hist EXCEPT ![s] = Append(@, r)]
  \* the responder of an entry s appended fires as it applies: its fate
  \* reaches the proposer (one step, for a smaller state space)
  /\ LET i == applied[s] + 1
         A == {a \in appBy[s] : a.i = i /\ log[s][i].o = a.o}
     IN IF A = {} THEN UNCHANGED <<appBy, ls, lk, resv, dbl, naSet>>
        ELSE LET a == CHOOSE x \in A : TRUE IN
             /\ appBy' = [appBy EXCEPT ![s] = @ \ {a}]
             /\ Deliver(s, a.o, a.k, IF VoidAt(s, log[s][i]) THEN "v" ELSE "a", i)
  /\ UNCHANGED <<raftV, snap, lt, lct, nseq, inbox, clog, envV>>

NoSnap == [idx |-> 0, ms |-> InitMs, kt |-> NoKeys, ord |-> << >>, mct |-> 0, hist |-> << >>]

\* a snapshot at the applied index, then the log is purged below it
TakeSnapshot(s) ==
  /\ Snapshots
  /\ applied[s] >= first[s]
  /\ snap' = [snap EXCEPT ![s] =
       [idx |-> applied[s], ms |-> ms[s],
        kt |-> IF SnapKeys THEN kt[s] ELSE NoKeys,
        ord |-> IF SnapKeys THEN Live(kt[s], ord[s]) ELSE << >>,
        mct |-> mct[s], hist |-> hist[s]]]
  /\ first' = [first EXCEPT ![s] = applied[s] + 1]
  /\ UNCHANGED <<term, role, log, commitIndex, lease, applied, ms, kt, ord, mct, hist,
                 prV, ghV, envV>>

\* a restart: the log, term and committed index are durable; applied state
\* restarts from the snapshot; the proposer, the machine's leader side and
\* anything handed to Raft but not received are lost
Restore(s, sn) ==
  /\ applied' = [applied EXCEPT ![s] = sn.idx]
  /\ ms' = [ms EXCEPT ![s] = sn.ms]
  /\ kt' = [kt EXCEPT ![s] = sn.kt]
  /\ ord' = [ord EXCEPT ![s] = sn.ord]
  /\ mct' = [mct EXCEPT ![s] = sn.mct]
  /\ hist' = [hist EXCEPT ![s] = sn.hist]
Crash(s) ==
  /\ ncrash < MaxCrash
  /\ ncrash' = ncrash + 1
  /\ role' = [role EXCEPT ![s] = "F"]
  /\ lease' = [lease EXCEPT ![s] = FALSE]
  /\ Restore(s, snap[s])
  /\ lt' = [lt EXCEPT ![s] = 0]
  /\ lct' = [lct EXCEPT ![s] = 0]
  /\ nseq' = [nseq EXCEPT ![s] = 0]
  /\ lk' = [lk EXCEPT ![s] = NoKeys]
  /\ inbox' = [inbox EXCEPT ![s] = << >>]
  /\ appBy' = [appBy EXCEPT ![s] = {}]
  /\ ls' = [ls EXCEPT ![s] = [t |-> 0, fl |-> 0, slots |-> {}]]
  /\ UNCHANGED <<term, log, commitIndex, first, snap, ghV, clk, net, nsend>>

Tick(s) ==
  /\ Ticks /\ clk[s] < MaxTime
  /\ clk' = [clk EXCEPT ![s] = @ + 1]
  /\ UNCHANGED <<raftV, smV, prV, ghV, ncrash, net, nsend>>

(*************************** Raft *****************************************)
LastTerm(l) == IF l = << >> THEN 0 ELSE l[Len(l)].term
UpToDate(a, b) == LastTerm(a) > LastTerm(b) \/ (LastTerm(a) = LastTerm(b) /\ Len(a) >= Len(b))

BecomeLeader(s, Q) ==
  LET t == 1 + MaxS({term[q] : q \in Q}) IN
  /\ role[s] = "F" /\ s \in Q /\ t <= MaxTerm
  /\ Len(log[s]) < MaxLen
  /\ \A q \in Q : UpToDate(log[s], log[q])
  /\ term' = [x \in Servers |-> IF x \in Q THEN t ELSE term[x]]
  /\ role' = [x \in Servers |-> IF x = s THEN "L" ELSE IF x \in Q THEN "F" ELSE role[x]]
  \* Q is a quorum in t, so the lease is established at once, unless lease
  \* timing is modelled (LeaseToggle: SetLease establishes it later)
  /\ lease' = [x \in Servers |-> IF x = s THEN ~ LeaseToggle
                                 ELSE IF x \in Q THEN FALSE ELSE lease[x]]
  /\ log' = [log EXCEPT ![s] = Append(@, Noop(s, t))]
  /\ UNCHANGED <<commitIndex, first, smV, prV, ghV, envV>>

StepDown(s) ==
  /\ role[s] = "L"
  /\ \E x \in Servers : term[x] > term[s]
  /\ role' = [role EXCEPT ![s] = "F"]
  /\ lease' = [lease EXCEPT ![s] = FALSE]
  /\ UNCHANGED <<term, log, commitIndex, first, smV, prV, ghV, envV>>

\* a leader's lease: established by a quorum in its term; it may lapse
\* (LeaseToggle) and be re-established within the same term
SetLease(s) ==
  /\ LeaseToggle /\ role[s] = "L" /\ ~ lease[s]
  /\ \E Q \in Quorums : \A q \in Q : term[q] = term[s]
  /\ lease' = [lease EXCEPT ![s] = TRUE]
  /\ UNCHANGED <<term, role, log, commitIndex, first, smV, prV, ghV, envV>>
LapseLease(s) ==
  /\ LeaseToggle /\ role[s] = "L" /\ lease[s]
  /\ lease' = [lease EXCEPT ![s] = FALSE]
  /\ UNCHANGED <<term, role, log, commitIndex, first, smV, prV, ghV, envV>>

\* l replicates its whole log to f (f's conflicting suffix is overwritten);
\* if f lacks entries l purged, l installs its snapshot on f
AE(l, f) ==
  /\ role[l] = "L" /\ l # f /\ term[l] >= term[f]
  /\ log[f] # log[l] \/ term[f] # term[l] \/ role[f] # "F" \/ commitIndex[f] < commitIndex[l]
  /\ LET p == first[l] - 1
         needSnap == p > 0 /\ ~ (Len(log[f]) >= p /\ SubSeq(log[f], 1, p) = SubSeq(log[l], 1, p))
     IN IF needSnap
          THEN /\ snap' = [snap EXCEPT ![f] = snap[l]]
               /\ first' = [first EXCEPT ![f] = first[l]]
               /\ Restore(f, snap[l])
               \* responders for purged entries complete with
               \* LogEntryDiscarded (i = 0 marks it)
               /\ appBy' = [appBy EXCEPT ![f] =
                              {IF a.i < first[l] THEN [a EXCEPT !.i = 0] ELSE a : a \in @}]
          ELSE UNCHANGED <<first, smV, appBy>>
  /\ log' = [log EXCEPT ![f] = log[l]]
  /\ term' = [term EXCEPT ![f] = term[l]]
  /\ role' = [role EXCEPT ![f] = "F"]
  /\ lease' = [lease EXCEPT ![f] = FALSE]
  /\ commitIndex' = [commitIndex EXCEPT ![f] = Max(@, commitIndex[l])]
  /\ UNCHANGED <<lt, lct, nseq, lk, inbox, ls, ghV, envV>>

CommitOK(l, n) ==
  /\ log[l][n].term = term[l]
  /\ \E Q \in Quorums : \A q \in Q :
       term[q] = term[l] /\ Len(log[q]) >= n /\ log[q][n] = log[l][n]
AdvanceCommit(l) ==
  /\ role[l] = "L"
  /\ LET ns == {n \in (commitIndex[l] + 1)..Len(log[l]) : CommitOK(l, n)} IN
     /\ ns # {}
     /\ LET n == MaxS(ns) IN
        /\ commitIndex' = [commitIndex EXCEPT ![l] = n]
        /\ clog' = IF n > Len(clog) THEN SubSeq(log[l], 1, n) ELSE clog
  /\ UNCHANGED <<term, role, log, first, lease, smV, prV, resv, dbl, naSet, envV>>

Init ==
  /\ term = [s \in Servers |-> 0]
  /\ role = [s \in Servers |-> "F"]
  /\ log = [s \in Servers |-> << >>]
  /\ commitIndex = [s \in Servers |-> 0]
  /\ first = [s \in Servers |-> 1]
  /\ lease = [s \in Servers |-> FALSE]
  /\ applied = [s \in Servers |-> 0]
  /\ ms = [s \in Servers |-> InitMs]
  /\ kt = [s \in Servers |-> NoKeys]
  /\ ord = [s \in Servers |-> << >>]
  /\ mct = [s \in Servers |-> 0]
  /\ hist = [s \in Servers |-> << >>]
  /\ snap = [s \in Servers |-> NoSnap]
  /\ lt = [s \in Servers |-> 0]
  /\ lct = [s \in Servers |-> 0]
  /\ nseq = [s \in Servers |-> 0]
  /\ lk = [s \in Servers |-> NoKeys]
  /\ inbox = [s \in Servers |-> << >>]
  /\ appBy = [s \in Servers |-> {}]
  /\ ls = [s \in Servers |-> [t |-> 0, fl |-> 0, slots |-> {}]]
  /\ clog = << >>
  /\ resv = {}
  /\ dbl = FALSE
  /\ naSet = {}
  /\ clk \in [Servers -> 0..MaxTime]
  /\ ncrash = 0
  /\ net = {}
  /\ nsend = [q \in Reqs |-> 0]

Next ==
  \/ \E s \in Servers :
       \/ \E Q \in Quorums : BecomeLeader(s, Q)
       \/ StepDown(s) \/ SetLease(s) \/ LapseLease(s)
       \/ \E f \in Servers : AE(s, f)
       \/ AdvanceCommit(s)
       \/ Rebuild(s)
       \/ \E p \in net : Process(s, p)
       \/ Receipt(s)
       \/ \E a \in appBy[s] : ResolveFate(s, a) \/ TimeoutUnknown(s, a)
       \/ Evicting(s)
       \/ Apply(s) \/ TakeSnapshot(s) \/ Crash(s) \/ Tick(s)
  \/ \E q \in Reqs : Send(q)

Spec == Init /\ [][Next]_vars

(*************************** properties ***********************************)
ElectionSafety ==
  \A s, t \in Servers : s # t /\ role[s] = "L" /\ role[t] = "L" => term[s] # term[t]
LogSafety ==
  \A s \in Servers :
    /\ commitIndex[s] <= Len(log[s]) /\ commitIndex[s] <= Len(clog)
    /\ SubSeq(log[s], 1, commitIndex[s]) = SubSeq(clog, 1, commitIndex[s])
    /\ applied[s] <= commitIndex[s] /\ first[s] - 1 <= applied[s]

CI == 1..Len(clog)

(* (a) A void entry decides nothing on any replica, and the proposer      *)
(*     learns its fate truthfully.                                         *)
VoidDecidesNothing ==
  \A s \in Servers :
    /\ \A i \in 1..applied[s] : (hist[s][i] = <<"void", 0>>) <=> Void(clog[i])
    /\ \A k \in DOMAIN kt[s] : NV(clog[kt[s][k].i])
FatesTruthful ==
  \A r \in resv :
    /\ r.f = "a" => r.i \in CI /\ clog[r.i].o = r.o /\ NV(clog[r.i])
    /\ r.f = "v" => r.i \in CI /\ clog[r.i].o = r.o /\ Void(clog[r.i])

(* (b) At most one resolve per admitted seq; a NotAppended one never      *)
(*     becomes a write.                                                    *)
AtMostOneResolve == ~ dbl
NotAppendedNeverWrites == \A i \in CI : NV(clog[i]) => clog[i].o \notin naSet

(* (c) A current leader's leader-side state is exactly the non-void       *)
(*     entries of its log above the floor plus what it has handed to Raft  *)
(*     this term and not yet heard refused: what admitting them would      *)
(*     have produced.                                                      *)
LeaderStateMatches ==
  \A s \in Servers : role[s] = "L" /\ lt[s] = term[s] =>
    {sl.o : sl \in {x \in LS(s) : x.st # "v"}} =
      {log[s][i].o : i \in {j \in (ls[s].fl + 1)..Len(log[s]) : NV(log[s][j])}}
      \cup {x.o : x \in {y \in Range(inbox[s]) : y.vt = term[s]}}

(* Embedder obligations. occ: nothing commits over a write it didn't see; *)
(* incr: every derived value is what serial apply computes; ctl: ids are  *)
(* unique and every partition is on a member.                              *)
NoUnseenConflict ==
  Emb = "occ" =>
    \A i, j \in CI : i < j /\ NV(clog[i]) /\ NV(clog[j]) /\ clog[i].c.n = clog[j].c.n
                     => i <= clog[j].rv
DerivedSound ==
  Emb = "incr" =>
    \A j \in CI : NV(clog[j]) =>
      clog[j].d = 1 + Cardinality({i \in 1..(j - 1) : NV(clog[i]) /\ clog[i].c.n = clog[j].c.n})
UniqueIds == Emb = "ctl" =>
  \A s \in Servers : \A a, b \in ms[s].parts : a[1] = b[1] => a = b
PlacementSafe == Emb = "ctl" =>
  \A s \in Servers : \A p \in ms[s].parts : p[2] \in ms[s].mem

(* (d) Each key is decided at most once within the window (measured in   *)
(*     the second decision's commit time), and the table a retry is        *)
(*     answered from names that decision.                                  *)
IdemOnce ==
  \A i, j \in CI : i < j /\ NV(clog[i]) /\ NV(clog[j]) /\ clog[i].k = clog[j].k
                   => clog[i].ct < clog[j].fl
KtDurable ==
  \A s \in Servers : \A k \in DOMAIN kt[s] :
    LET i == kt[s][k].i IN
    i \in CI /\ clog[i].k = k /\ NV(clog[i]) /\ clog[i].ct = kt[s][k].ct

(* (e) Commit times                                                        *)
MonoInTerm ==
  \A s \in Servers : \A i, j \in 1..Len(log[s]) :
    i < j /\ NV(log[s][i]) /\ NV(log[s][j]) /\ log[s][i].term = log[s][j].term
      => log[s][i].ct <= log[s][j].ct
\* across terms, decided (non-void) entries, in every replica's log
MonoAcrossTerms ==
  \A s \in Servers : \A i, j \in 1..Len(log[s]) :
    i < j /\ NV(log[s][i]) /\ NV(log[s][j]) => log[s][i].ct <= log[s][j].ct
\* the same, void entries included (not a goal; see the cfgs)
MonoAllEntries ==
  \A s \in Servers : \A i, j \in 1..Len(log[s]) :
    i < j /\ IsCmd(log[s][i]) /\ IsCmd(log[s][j]) => log[s][i].ct <= log[s][j].ct

(* (f) Apply is deterministic: replicas agree at every index both applied, *)
(*     and have equal state at equal applied indexes.                      *)
ApplyDeterministic ==
  \A s, t \in Servers :
    /\ \A i \in 1..Min(applied[s], applied[t]) : hist[s][i] = hist[t][i]
    /\ applied[s] = applied[t] => ms[s] = ms[t] /\ kt[s] = kt[t] /\ mct[s] = mct[t]

Safety ==
  /\ ElectionSafety /\ LogSafety
  /\ VoidDecidesNothing /\ FatesTruthful
  /\ AtMostOneResolve /\ NotAppendedNeverWrites
  /\ LeaderStateMatches
  /\ NoUnseenConflict /\ DerivedSound /\ UniqueIds /\ PlacementSafe
  /\ IdemOnce /\ KtDurable
  /\ MonoInTerm /\ MonoAcrossTerms
  /\ ApplyDeterministic

Sym == Permutations(Servers)

(*************************** programs used by the cfgs ********************)
\* two requests (two idempotency keys) on one object
ProgTwo == [q1 |-> [n |-> "x"], q2 |-> [n |-> "x"]]
ProgOne == [q1 |-> [n |-> "x"]]
\* controller: two creates; and the same with a decommission of node 2 (the
\* second create is placed on node 2 once the first has applied on node 1)
ProgCtl == [c1 |-> [op |-> "create"], c2 |-> [op |-> "create"]]
ProgCtlRm == [c1 |-> [op |-> "create"], c2 |-> [op |-> "create"],
              r2 |-> [op |-> "remove", rep |-> 2]]

=============================================================================
