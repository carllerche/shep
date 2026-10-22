----------------------------- MODULE RaftDriver -----------------------------
(***************************************************************************)
(* Shep design §3, §9.6, §17: the runtime of `shep-raft`, Shep's           *)
(* multi-raft, which keeps openraft 0.10's engine (elections, votes, log   *)
(* matching, commit) and replaces its per-group runtime with a per-core    *)
(* driver. One Raft group of three replicas, at the level of messages.     *)
(* The network reorders, drops and duplicates every message (FreeNet), or  *)
(* does so a bounded number of times (liveness).                           *)
(*                                                                         *)
(* Driver = "landed": what shep-raft runs today.                           *)
(*  - Replication sessions. A leader's stream to one follower is a session *)
(*    numbered <<term, counter>> (the per-core counter; a new term or a    *)
(*    new stream gives a higher number). Requests carry consecutive seq    *)
(*    numbers from 0, at most Window in flight, one entry each. The leader *)
(*    takes results strictly in request order; the first error, conflict  *)
(*    or partial success ends the stream, and a new session restarts from *)
(*    the follower's match. A lost request or reply (an RPC timeout) or a  *)
(*    connection failure also ends it.                                     *)
(*  - The follower keeps {session, next_seq, waiting set}. An older        *)
(*    session is rejected; a newer one rejects every waiting request of    *)
(*    the old one and resets next_seq to 0; a seq below next_seq is a      *)
(*    duplicate and rejected; otherwise the request waits, and each one    *)
(*    that is next goes to the engine in seq order. A request still        *)
(*    waiting after append_gap_timeout is rejected ("waited for request    *)
(*    N"). A restart loses all of this and drops the node's connections,   *)
(*    which fails every in-flight call at the other end.                   *)
(*  - Heartbeats are full AppendEntries (term, prev = the follower's       *)
(*    match, leader commit, no entries), handled by the engine outside the *)
(*    session order. A leader sends one at once on election and accepts   *)
(*    writes only with a quorum-acked lease.                               *)
(*  - Not landed, so openraft's own behaviour: a follower replies to an    *)
(*    append only after its entries are flushed (fsyncs coalesce); the     *)
(*    leader counts its own flushed (not submitted) index in the quorum.   *)
(*    Elections: prevote, and a node with a live leader lease refuses      *)
(*    votes unless a leadership transfer reset it. No lightweight          *)
(*    heartbeats, no may_recover: recovery is the stream itself.           *)
(*                                                                         *)
(* Driver = "redpanda": the target, Redpanda's driver (src/v/raft:         *)
(* consensus.cc, heartbeat_manager.cc, recovery_stm.cc,                    *)
(* append_entries_buffer.cc, recovery_scheduler.cc).                       *)
(*  - (a) Pipelined appends to a follower expected at the leader's end,    *)
(*    with follower_req_seq (last sent / received / successful) and the    *)
(*    reorder check of consensus.cc:445; replies carry the follower's      *)
(*    dirty and flushed index.                                             *)
(*  - (b) Commit at a quorum of each follower's durable match (AckRule)   *)
(*    and the leader's flushed offset, clamped to it, own-term only; a     *)
(*    follower commits min(leader commit, own flushed).                    *)
(*  - (c) Group flush: buffered requests are appended in order, one fsync *)
(*    if any asked, then every reply is stamped with the flushed offset.  *)
(*  - (d) Lightweight heartbeats (group only, no term) and full ones;      *)
(*    stickiness with the transfer exception; prevote.                     *)
(*  - (e) Follower-admitted recovery: a follower with a gap registers with *)
(*    its node's scheduler, which admits it into one of MaxSlots slots;    *)
(*    the leader's recovery loop starts only once a reply carries          *)
(*    may_recover; ending recovery frees the slot. Other groups on a node *)
(*    compete for the slots (EnvRequest).                                  *)
(*                                                                         *)
(* Time is abstract: a node's "heard from a leader" flag (stickiness, or  *)
(* openraft's lease) expires by Expire. Per-node-pair batching of          *)
(* heartbeats is transport only and not modelled: each group's entry is    *)
(* handled on its own. Not modelled: membership changes, snapshots,        *)
(* quorum-loss probing and check-quorum (except as faults), priorities.    *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, Sequences, TLC

CONSTANTS
  Servers, Nil,
  Driver,         \* "landed" or "redpanda"
  MaxTerm,        \* bound on terms
  MaxClient,      \* bound on client entries (no-ops are not bounded)
  Budget,         \* > 0: at most this many messages are ever sent (safety)
  Window,         \* requests in flight per follower
  MaxSess,        \* landed: sessions per leader per term
  UseSeq,         \* redpanda: requests carry follower_req_seq ...
  MaxSeq,         \*   ... at most this many per follower per leadership
  MaxBuf,         \* redpanda: follower append_entries_buffer size
  MaxSlots,       \* redpanda: recovery slots per node
  MaxCrash, MaxDrop, MaxDup, MaxFault, MaxXfer, MaxEnv,
  FreeNet,        \* TRUE: unbounded drops and duplicates (safety configs)
  FreeTime,       \* TRUE: flags expire, timers fire and votes split freely
  AtomicElect,    \* TRUE: an election is one step (a quorum votes for an
                  \*   up-to-date candidate); FALSE: prevote, vote and
                  \*   timeout_now messages with stickiness or the lease
  Prevote,        \* TRUE: a timeout runs a prevote first
  XferException,  \* TRUE: a transfer election ignores stickiness / the lease
  \* ---- rules: the design unless a negative cfg changes one ----
  ReplyAfterFlush,\* landed: a follower replies to an append only once flushed
  LeaderSelf,     \* "flushed": the leader counts its flushed index in the
                  \*   quorum; "submitted": its appended index (negative)
  LeaderClamp,    \* TRUE: commit is clamped to the leader's flushed index
  FollowerClamp,  \* TRUE: follower commit <= own flushed; FALSE: <= own log
  LeaderRestore,  \* openraft enable_leader_restore: a leader that restarts
                  \*   comes back as leader in the same term, its session
                  \*   counter reset (FALSE: the fix; it restarts a follower)
  HbLeaseRule,    \* landed (proposed): a follower that rejected a request of
                  \*   its current term (superseded, duplicate, gap) does not
                  \*   count heartbeats as leader contact until an append
                  \*   succeeds again
  CtrReset,       \* landed: the session counter may reset within a term
                  \*   (violates the stated assumption; negative)
  SeqCheck,       \* redpanda: "redpanda" drops a reply with seq < last
                  \*   received unless its dirty index >= the leader's;
                  \*   "strict" drops any; "none" drops none
  SessionCheck,   \* redpanda: TRUE drops a reply to a request of another term
  AckRule,        \* what an ack counts for commit:
                  \*   "redpanda": min(last flushed reported, match), each
                  \*   from whichever reply set it last
                  \*   "permatch": max over this term's success replies of
                  \*   min(dirty, flushed) of that one reply
                  \*   "dirty": the matched index alone (negative)
  LwHb,           \* redpanda: lightweight heartbeats
  SlotRelease     \* redpanda: ending recovery releases the slot

ASSUME Driver \in {"landed", "redpanda"}
ASSUME SeqCheck \in {"redpanda", "strict", "none"}
ASSUME AckRule \in {"redpanda", "permatch", "dirty"}
ASSUME LeaderSelf \in {"flushed", "submitted"}
ASSUME UseSeq \/ SeqCheck = "none"

Landed == Driver = "landed"
Quorums == {Q \in SUBSET Servers : 2 * Cardinality(Q) > Cardinality(Servers)}
Max(a, b) == IF a > b THEN a ELSE b
Min(a, b) == IF a < b THEN a ELSE b
\* an entry is <<term, id>>: id 0 for a no-op, else the client entry's
\* number. Replicas compare log ids (index, term) only, as Raft does; the
\* invariants compare entries, so two entries with one log id are caught.
LastTerm(l) == IF l = << >> THEN 0 ELSE l[Len(l)][1]
TermAt(l, i) == IF i = 0 THEN 0 ELSE l[i][1]
Range(s) == {s[i] : i \in DOMAIN s}
IsPrefix(a, b) == Len(a) <= Len(b) /\ a = SubSeq(b, 1, Len(a))
\* session numbers <<term, counter>>, ordered as (term << 32) | counter
Older(a, b) == a[1] < b[1] \/ (a[1] = b[1] /\ a[2] < b[2])
NoSess == << -1, -1 >>
\* state that only some modes read is frozen in the others (smaller state
\* space): votedFor and heard matter only to message-level elections,
\* leaderId only to lightweight heartbeats
VF(new, old) == IF AtomicElect THEN old ELSE new
HD(new, old) == IF AtomicElect THEN old ELSE new
LID(new, old) == IF Landed THEN old ELSE new

VARIABLES
  term, votedFor, role,   \* role: "F", "PC" (prevote), "C", "L"
  log,          \* entries are terms
  flushed,      \* durable prefix length
  commitIndex,
  leaderId, heard,
  votes,        \* (pre)votes granted to a PC / C
  xfer,         \* C: a transfer election; L: transferring leadership
  fs,           \* leader: per-follower state
  \* landed
  ctr,          \* leader: the session counter
  fsess, fnext, wbuf,   \* follower: session, next_seq, waiting requests
  fgi,          \* ghost: the leader incarnation of the follower's session
  nolease,      \* follower (HbLeaseRule): heartbeats do not renew the lease
  lpend,        \* follower: replies waiting for their entries' flush
  \* redpanda
  buf,          \* follower: append_entries_buffer
  pend,         \* follower: a processed batch awaiting fsync and replies
  frs,          \* follower recovery state: "none", "pending", "active"
  leak,         \* a slot never released (SlotRelease = FALSE)
  envWant, envActive,   \* other groups' recoveries on each node
  \* network, ghosts, budgets
  net, nSent,
  elected,      \* ghost: <<term, leader>>
  ginc, lgi,    \* ghost: leader incarnations so far; each leader's current one
  clog, clogT,  \* ghost: longest committed log; the term each index committed in
  nCrash, nDrop, nDup, nFault, nClient, nXfer, nEnv

coreVars == <<term, votedFor, role, log, flushed, commitIndex, leaderId, heard, votes, xfer>>
landVars == <<ctr, fsess, fnext, fgi, wbuf, lpend, nolease>>
rpVars == <<buf, pend, frs, leak, envWant, envActive>>
ghostVars == <<elected, ginc, lgi, clog, clogT>>
cntVars == <<nCrash, nDrop, nFault, nClient, nXfer, nEnv>>   \* nDup goes with the network
netVars == <<net, nSent, nDup>>
vars == <<coreVars, fs, landVars, rpVars, netVars, ghostVars, cntVars>>

NoPend == [on |-> FALSE, fl |-> FALSE, reps |-> << >>]
\* leader's state for a follower; n = its log length before the no-op
FInit(n) == [sent |-> 0, recv |-> 0, succ |-> 0, ok |-> FALSE, match |-> 0,
             fd |-> 0, ff |-> 0, dur |-> 0, exp |-> n, next |-> n + 1,
             recov |-> FALSE, inrec |-> FALSE, meta |-> << >>,
             sc |-> 1, nseq |-> 0, rnext |-> 0, ack |-> FALSE]

(*************************** budgets and the network **********************)
Fault == IF FreeTime THEN UNCHANGED nFault ELSE nFault < MaxFault /\ nFault' = nFault + 1
Count(k) == IF Budget > 0 THEN nSent + k ELSE 0

\* send out
Send(out) == /\ net' = net \cup out /\ nSent' = Count(Cardinality(out \ net)) /\ UNCHANGED nDup
\* consume m (or keep it: a duplicate) and send out
Net(m, keep, out) ==
  /\ net' = (IF keep THEN net ELSE net \ {m}) \cup out
  /\ nSent' = Count(Cardinality(out \ net))
  /\ nDup' = IF keep /\ ~FreeNet THEN nDup + 1 ELSE nDup
Keeps == IF FreeNet \/ nDup < MaxDup THEN BOOLEAN ELSE {FALSE}

Rep(st, r, ok, kind, d) ==
  [ty |-> "aer", src |-> r.dst, dst |-> r.src, term |-> st.term, qt |-> r.term,
   sess |-> r.sess, seq |-> r.seq, hb |-> r.hb, ok |-> ok, kind |-> kind, d |-> d,
   f |-> st.flushed, mr |-> st.frs = "active", rec |-> r.rec, pv |-> r.prev, fst |-> -1,
   gi |-> r.gi, he |-> r.ents = << >>]

(*************************** the follower's engine ************************)
LS(s) == [term |-> term[s], vf |-> votedFor[s], role |-> role[s], lid |-> leaderId[s],
          heard |-> heard[s], log |-> log[s], flushed |-> flushed[s],
          commit |-> commitIndex[s], frs |-> frs[s], leak |-> leak[s], xfer |-> xfer[s]]
SetLS(s, st) ==
  /\ term' = [term EXCEPT ![s] = st.term]
  /\ votedFor' = [votedFor EXCEPT ![s] = st.vf]
  /\ role' = [role EXCEPT ![s] = st.role]
  /\ leaderId' = [leaderId EXCEPT ![s] = st.lid]
  /\ heard' = [heard EXCEPT ![s] = st.heard]
  /\ log' = [log EXCEPT ![s] = st.log]
  /\ flushed' = [flushed EXCEPT ![s] = st.flushed]
  /\ commitIndex' = [commitIndex EXCEPT ![s] = st.commit]
  /\ xfer' = [xfer EXCEPT ![s] = st.xfer]

\* a request of term >= ours: adopt the term and the leader, refresh the
\* leader timestamp (stickiness / lease)
Follow(st, r) == [st EXCEPT !.term = r.term,
                            !.vf = VF(IF r.term > st.term THEN Nil ELSE st.vf, st.vf),
                            !.role = "F", !.lid = LID(r.src, st.lid),
                            !.heard = HD(TRUE, st.heard), !.xfer = FALSE]
CommitTo(st, lc, upto) ==
  Max(st.commit, Min(lc, Min(upto, IF FollowerClamp THEN st.flushed ELSE Len(st.log))))

\* how many of the request's entries the log already holds after prev
Matched(l, p, es, k) == \A j \in 1..k : p + j <= Len(l) /\ l[p + j][1] = es[j][1]
MK(l, p, es) == CHOOSE k \in 0..Len(es) :
                  Matched(l, p, es, k) /\ (k = Len(es) \/ ~Matched(l, p, es, k + 1))

\* openraft's engine: append entries (or a heartbeat). On a match it
\* truncates at the first conflicting entry and appends the rest.
OStep(st, r) ==
  IF r.term < st.term THEN [st |-> st, rep |-> Rep(st, r, FALSE, "term", Len(st.log))]
  ELSE
  LET s1 == Follow(st, r)  l == s1.log IN
  IF r.prev > Len(l) \/ (r.prev > 0 /\ l[r.prev][1] # r.pterm)
  THEN [st |-> s1, rep |-> Rep(s1, r, FALSE, "conflict", Len(l))]
  ELSE
  LET k == MK(l, r.prev, r.ents)
      e == r.prev + Len(r.ents)
      s2 == IF k = Len(r.ents) THEN s1
            ELSE [s1 EXCEPT !.log = SubSeq(l, 1, r.prev + k) \o SubSeq(r.ents, k + 1, Len(r.ents)),
                            !.flushed = Min(s1.flushed, r.prev + k)]
      s3 == [s2 EXCEPT !.commit = CommitTo(s2, r.lc, e)]
  IN [st |-> s3, rep |-> Rep(s3, r, TRUE, "ok", e)]

\* Redpanda's consensus::do_append_entries (no flush)
Upsert(st, force) ==
  IF st.frs = "none" THEN [st EXCEPT !.frs = IF force THEN "active" ELSE "pending"]
  ELSE IF force THEN [st EXCEPT !.frs = "active"] ELSE st
Release(st) == [st EXCEPT !.frs = "none",
                          !.leak = st.leak \/ (~SlotRelease /\ st.frs = "active")]
RStep(st, r) ==
  IF r.term < st.term THEN [st |-> st, rep |-> Rep(st, r, FALSE, "term", Len(st.log))]
  ELSE
  LET s1 == Follow(st, r)  l == s1.log IN
  IF r.prev > Len(l) \/ (r.prev > 0 /\ l[r.prev][1] # r.pterm)
  THEN \* a gap or a mismatch: register with the recovery scheduler
       LET s2 == Upsert(s1, r.rec) IN [st |-> s2, rep |-> Rep(s2, r, FALSE, "conflict", Len(l))]
  ELSE
  LET k == MK(l, r.prev, r.ents)
      adj == r.prev + k
      rest == SubSeq(r.ents, k + 1, Len(r.ents))
  IN
  IF k > 0 /\ rest = << >>
  THEN \* all already here (a delayed or duplicated request)
       [st |-> s1, rep |-> [Rep(s1, r, TRUE, "ok", adj) EXCEPT !.f = Min(adj, s1.flushed)]]
  ELSE IF rest = << >>
  THEN \* a full heartbeat: never truncates; ends recovery
       IF adj < Len(l) THEN [st |-> s1, rep |-> Rep(s1, r, FALSE, "conflict", Len(l))]
       ELSE LET s2 == Release([s1 EXCEPT !.commit = CommitTo(s1, r.lc, adj)])
            IN [st |-> s2, rep |-> Rep(s2, r, TRUE, "ok", Len(l))]
  ELSE
  LET s2 == IF r.rec THEN Upsert(s1, TRUE) ELSE s1 IN
  IF adj < Len(l)
  THEN IF adj < s2.commit
       THEN [st |-> s2, rep |-> [Rep(s2, r, TRUE, "ok", s2.commit) EXCEPT !.f = s2.commit]]
       ELSE \* truncate at the conflict and reply; the leader resends
            LET s3 == [s2 EXCEPT !.log = SubSeq(l, 1, adj), !.flushed = Min(s2.flushed, adj)]
            IN [st |-> s3, rep |-> Rep(s3, r, TRUE, "ok", adj)]
  ELSE LET nl == l \o rest
           s3 == [s2 EXCEPT !.log = nl, !.commit = CommitTo(s2, r.lc, Len(nl))]
           s4 == IF r.rec THEN s3 ELSE Release(s3)    \* a normal append ends recovery
       IN [st |-> s4, rep |-> Rep(s4, r, TRUE, "ok", Len(nl))]

RECURSIVE Fold(_, _, _)
Fold(st, rs, reps) ==
  IF rs = << >> THEN [st |-> st, reps |-> reps]
  ELSE LET o == RStep(st, Head(rs)) IN Fold(o.st, Tail(rs), Append(reps, o.rep))

(*************************** landed: follower *****************************)
SessOf(r) == << r.term, r.sess >>
\* fst (ghost): the term of the follower's session when it rejected
Reject(f, r, kind) == [Rep(LS(f), r, FALSE, kind, Len(log[f])) EXCEPT !.fst = fsess[f][1]]

\* HbLeaseRule: rejecting a request of the follower's current term
NoLease(f, r) == nolease' = [nolease EXCEPT ![f] = @ \/ (HbLeaseRule /\ r.term = term[f])]

\* a pipelined request reaches the session gate
LDeliver(m) ==
  LET f == m.dst
      newer == Older(fsess[f], SessOf(m))
      old == IF newer THEN wbuf[f] ELSE {}
      nx == IF newer THEN 0 ELSE fnext[f]
  IN
  /\ Landed /\ m.ty = "ae" /\ ~m.hb
  /\ \E keep \in Keeps :
     IF Older(SessOf(m), fsess[f])
       THEN /\ Net(m, keep, {Reject(f, m, "stale")})
            /\ NoLease(f, m)
            /\ UNCHANGED <<fsess, fnext, fgi, wbuf>>
     ELSE IF m.seq < nx
       THEN /\ Net(m, keep, {Reject(f, m, "dup")} \cup {[Reject(f, r, "stale") EXCEPT !.fst = m.term] : r \in old})
            /\ NoLease(f, m)
            /\ fsess' = [fsess EXCEPT ![f] = SessOf(m)]
            /\ fnext' = [fnext EXCEPT ![f] = nx]
            /\ fgi' = [fgi EXCEPT ![f] = IF newer THEN m.gi ELSE fgi[f]]
            /\ wbuf' = [wbuf EXCEPT ![f] = wbuf[f] \ old]
     ELSE /\ Net(m, keep, {[Reject(f, r, "stale") EXCEPT !.fst = m.term] : r \in old})
          /\ fsess' = [fsess EXCEPT ![f] = SessOf(m)]
          /\ fnext' = [fnext EXCEPT ![f] = nx]
          /\ fgi' = [fgi EXCEPT ![f] = IF newer THEN m.gi ELSE fgi[f]]
          /\ wbuf' = [wbuf EXCEPT ![f] = (wbuf[f] \ old) \cup {m}]
          /\ UNCHANGED nolease
  /\ UNCHANGED <<coreVars, fs, ctr, lpend, rpVars, ghostVars, cntVars>>

\* the request that is next goes to the engine; its reply waits for the
\* flush of its entries (or not: ReplyAfterFlush = FALSE)
LEngine(f) ==
  /\ Landed
  /\ \E r \in wbuf[f] :
     /\ SessOf(r) = fsess[f] /\ r.seq = fnext[f]
     /\ LET o == OStep(LS(f), r) IN
        /\ SetLS(f, o.st)
        /\ IF o.rep.ok /\ ReplyAfterFlush
             THEN lpend' = [lpend EXCEPT ![f] = lpend[f] \cup {o.rep}] /\ UNCHANGED netVars
             ELSE Send({o.rep}) /\ UNCHANGED lpend
     /\ wbuf' = [wbuf EXCEPT ![f] = wbuf[f] \ {r}]
     /\ fnext' = [fnext EXCEPT ![f] = fnext[f] + 1]
     /\ nolease' = [nolease EXCEPT ![f] = @ /\ ~OStep(LS(f), r).rep.ok]
  /\ UNCHANGED <<votes, fs, ctr, fsess, fgi, rpVars, ghostVars, cntVars>>

\* a coalesced fsync of everything appended, then the replies whose entries
\* it covers go out (in one step: a crash before it loses both)
LReplyFlushed(f) ==
  LET nf == Len(log[f])
      R == {x \in lpend[f] : x.d <= nf}
  IN
  /\ Landed /\ lpend[f] # {}
  /\ flushed' = [flushed EXCEPT ![f] = nf]
  /\ Send({[x EXCEPT !.f = nf] : x \in R})
  /\ lpend' = [lpend EXCEPT ![f] = lpend[f] \ R]
  /\ UNCHANGED <<term, votedFor, role, log, commitIndex, leaderId, heard, votes, xfer,
                 fs, ctr, fsess, fnext, fgi, wbuf, nolease, rpVars, ghostVars, cntVars>>

\* append_gap_timeout: a waiting request is rejected. It is a real gap if
\* some earlier seq of the session can no longer arrive; otherwise the
\* timer fired early (a fault, unless FreeTime).
GapReal(f, r) ==
  \E q \in fnext[f]..(r.seq - 1) :
    ~\E m \in net : m.ty = "ae" /\ ~m.hb /\ m.dst = f /\ SessOf(m) = SessOf(r) /\ m.seq = q
LGap(f, r) ==
  /\ Landed /\ r \in wbuf[f]
  /\ IF GapReal(f, r) THEN UNCHANGED nFault ELSE Fault
  /\ Send({Reject(f, r, "gap")})
  /\ NoLease(f, r)
  /\ wbuf' = [wbuf EXCEPT ![f] = wbuf[f] \ {r}]
  /\ UNCHANGED <<coreVars, fs, ctr, fsess, fnext, fgi, lpend, rpVars, ghostVars,
                 nCrash, nDrop, nClient, nXfer, nEnv>>

\* a heartbeat: the engine, at once, outside the session order
LHeartbeatIn(m) ==
  LET o == OStep(LS(m.dst), m) IN
  /\ Landed /\ m.ty = "ae" /\ m.hb
  /\ \E keep \in Keeps : Net(m, keep, {o.rep})
  \* HbLeaseRule: a heartbeat does not renew a lease the follower withdrew
  /\ SetLS(m.dst, IF nolease[m.dst] THEN [o.st EXCEPT !.heard = heard[m.dst]] ELSE o.st)
  /\ UNCHANGED <<votes, fs, landVars, rpVars, ghostVars, cntVars>>

(*************************** landed: leader *******************************)
Lease(l) == {l} \cup {f \in Servers : fs[l][f].ack} \in Quorums

\* the stream to f restarts: a new session from the follower's match
NewSess(l, fr, nxt) == [fr EXCEPT !.sc = IF ctr[l] < MaxSess THEN ctr[l] + 1 ELSE 0,
                                  !.nseq = 0, !.rnext = 0, !.next = nxt]
CtrNext(l) == IF ctr[l] < MaxSess THEN ctr[l] + 1 ELSE ctr[l]

LAE(l, f, sess, seq, prev, es, hb) ==
  [ty |-> "ae", src |-> l, dst |-> f, term |-> term[l], sess |-> sess, seq |-> seq,
   hb |-> hb, prev |-> prev, pterm |-> TermAt(log[l], prev), ents |-> es,
   lc |-> commitIndex[l], fl |-> TRUE, rec |-> FALSE, gi |-> lgi[l]]

LSend(l, f) ==
  LET fr == fs[l][f] IN
  /\ Landed /\ role[l] = "L" /\ f # l
  /\ fr.sc > 0
  /\ fr.next <= Len(log[l])
  /\ fr.nseq - fr.rnext < Window
  /\ Send({LAE(l, f, fr.sc, fr.nseq, fr.next - 1, << log[l][fr.next] >>, FALSE)})
  /\ fs' = [fs EXCEPT ![l][f].next = fr.next + 1, ![l][f].nseq = fr.nseq + 1]
  /\ UNCHANGED <<coreVars, landVars, rpVars, ghostVars, cntVars>>

HbInFlight(l, f) == \E m \in net : m.src = l /\ m.dst = f /\ (m.ty = "lw" \/ (m.ty = "ae" /\ m.hb))

LHeartbeat(l, f) ==
  /\ Landed /\ role[l] = "L" /\ f # l
  /\ ~HbInFlight(l, f)
  /\ Send({LAE(l, f, 0, 0, fs[l][f].match, << >>, TRUE)})
  /\ UNCHANGED <<coreVars, fs, landVars, rpVars, ghostVars, cntVars>>

\* results are taken strictly in request order: only the reply to rnext. A
\* reply returns only to the incarnation that made the call (its connection).
LReplyRelevant(m) ==
  role[m.dst] = "L" /\ m.qt = term[m.dst] /\ m.sess = fs[m.dst][m.src].sc /\ m.gi = lgi[m.dst]

StepDown(l, t) ==
  /\ term' = [term EXCEPT ![l] = t]
  /\ votedFor' = [votedFor EXCEPT ![l] = VF(Nil, @)]
  /\ role' = [role EXCEPT ![l] = "F"]
  /\ leaderId' = [leaderId EXCEPT ![l] = LID(Nil, @)]
  /\ heard' = [heard EXCEPT ![l] = HD(TRUE, @)]
  /\ xfer' = [xfer EXCEPT ![l] = FALSE]
  /\ UNCHANGED <<log, flushed, commitIndex, votes>>

LHandleAER(m) ==
  LET l == m.dst  f == m.src  fr == fs[l][f]
      full == m.ok /\ m.d = m.pv + 1
      upd == [fr EXCEPT !.match = Max(fr.match, m.d), !.ack = TRUE,
                        !.dur = Max(fr.dur, Min(m.d, m.f)), !.rnext = fr.rnext + 1]
      \* Conflict carries no index: step back one (openraft searches back)
      nxt == IF m.ok THEN m.d + 1
             ELSE IF m.kind = "conflict" THEN Max(fr.match + 1, m.pv)
             ELSE fr.match + 1
      ended == NewSess(l, [fr EXCEPT !.ack = TRUE], nxt)
  IN
  /\ Landed /\ m.ty = "aer" /\ ~m.hb
  /\ LReplyRelevant(m) => m.seq <= fr.rnext      \* a later result waits its turn
  /\ \E keep \in Keeps : Net(m, keep, {})
  /\ IF ~LReplyRelevant(m) \/ m.seq < fr.rnext
       THEN UNCHANGED <<coreVars, fs, ctr>>
     ELSE IF m.kind = "term" /\ m.term > term[l]     \* HigherVote; Rejected carries none
       THEN StepDown(l, m.term) /\ UNCHANGED <<fs, ctr>>
     ELSE IF full
       THEN fs' = [fs EXCEPT ![l][f] = upd] /\ UNCHANGED <<coreVars, ctr>>
     ELSE \* an error, a conflict or a partial success ends the stream
          /\ fs' = [fs EXCEPT ![l][f] = ended]
          /\ ctr' = [ctr EXCEPT ![l] = CtrNext(l)]
          /\ UNCHANGED coreVars
  /\ UNCHANGED <<fsess, fnext, fgi, wbuf, lpend, nolease, rpVars, ghostVars, cntVars>>

\* a heartbeat reply: the engine updates match monotonically (openraft's
\* update_matching), outside the stream's order
LHandleHbR(m) ==
  LET l == m.dst  f == m.src  fr == fs[l][f] IN
  /\ Landed /\ m.ty = "aer" /\ m.hb
  /\ \E keep \in Keeps : Net(m, keep, {})
  /\ IF role[l] # "L" \/ m.qt # term[l] \/ m.gi # lgi[l]
       THEN UNCHANGED <<coreVars, fs>>
     ELSE IF m.term > term[l]
       THEN StepDown(l, m.term) /\ UNCHANGED fs
     ELSE IF m.ok
       THEN /\ fs' = [fs EXCEPT ![l][f].match = Max(fr.match, m.d), ![l][f].ack = TRUE,
                                ![l][f].dur = Max(fr.dur, Min(m.d, m.f))]
            /\ UNCHANGED coreVars
     ELSE fs' = [fs EXCEPT ![l][f].ack = TRUE] /\ UNCHANGED coreVars
  /\ UNCHANGED <<landVars, rpVars, ghostVars, cntVars>>

\* the result of rnext can no longer arrive (lost request or reply, or the
\* connection failed): the call times out and the stream restarts
LResultLost(l, f) ==
  LET fr == fs[l][f]
      q(r) == r.src = l /\ r.term = term[l] /\ r.sess = fr.sc /\ r.seq = fr.rnext /\ r.gi = lgi[l]
      a(x) == x.dst = l /\ x.qt = term[l] /\ x.sess = fr.sc /\ x.seq = fr.rnext /\ x.gi = lgi[l]
  IN
  /\ ~\E m \in net : m.ty = "ae" /\ ~m.hb /\ m.dst = f /\ q(m)
  /\ ~\E r \in wbuf[f] : q(r)
  /\ ~\E x \in lpend[f] : a(x)
  /\ ~\E m \in net : m.ty = "aer" /\ ~m.hb /\ m.src = f /\ a(m)
LStreamTimeout(l, f) ==
  LET fr == fs[l][f] IN
  /\ Landed /\ role[l] = "L" /\ f # l
  /\ fr.sc > 0 /\ fr.rnext < fr.nseq
  /\ LResultLost(l, f)
  /\ fs' = [fs EXCEPT ![l][f] = NewSess(l, fr, fr.match + 1)]
  /\ ctr' = [ctr EXCEPT ![l] = CtrNext(l)]
  /\ UNCHANGED <<coreVars, fsess, fnext, fgi, wbuf, lpend, nolease, rpVars, netVars, ghostVars, cntVars>>

\* negative: the counter goes back within a term (violates the assumption
\* that a restart or core move forces a new term before leading again)
LCtrReset(l) ==
  /\ Landed /\ CtrReset /\ role[l] = "L" /\ ctr[l] > 1
  /\ Fault
  /\ ctr' = [ctr EXCEPT ![l] = 0]
  /\ UNCHANGED <<coreVars, fs, fsess, fnext, fgi, wbuf, lpend, nolease, rpVars, netVars, ghostVars,
                 nCrash, nDrop, nClient, nXfer, nEnv>>

(*************************** redpanda: follower ***************************)
DeliverAE(m) ==
  /\ ~Landed /\ m.ty = "ae"
  /\ Len(buf[m.dst]) < MaxBuf
  /\ \E keep \in Keeps : Net(m, keep, {})
  /\ buf' = [buf EXCEPT ![m.dst] = Append(buf[m.dst], m)]
  /\ UNCHANGED <<coreVars, fs, landVars, pend, frs, leak, envWant, envActive, ghostVars, cntVars>>

\* append_entries_buffer::flush, first half: every buffered request in order
ProcessBatch(f) ==
  /\ ~Landed
  /\ buf[f] # << >> /\ ~pend[f].on
  /\ LET o == Fold(LS(f), buf[f], << >>) IN
     /\ SetLS(f, o.st)
     /\ frs' = [frs EXCEPT ![f] = o.st.frs]
     /\ leak' = [leak EXCEPT ![f] = o.st.leak]
     /\ pend' = [pend EXCEPT ![f] = [on |-> TRUE, fl |-> \E i \in DOMAIN buf[f] : buf[f][i].fl,
                                     reps |-> o.reps]]
  /\ buf' = [buf EXCEPT ![f] = << >>]
  /\ UNCHANGED <<votes, fs, landVars, envWant, envActive, netVars, ghostVars, cntVars>>

\* second half: one fsync if any asked, then every reply is stamped with the
\* flushed offset and sent
FlushReply(f) ==
  /\ ~Landed /\ pend[f].on
  /\ LET nf == IF pend[f].fl THEN Len(log[f]) ELSE flushed[f] IN
     /\ flushed' = [flushed EXCEPT ![f] = nf]
     /\ Send({[r EXCEPT !.f = nf] : r \in Range(pend[f].reps)})
  /\ pend' = [pend EXCEPT ![f] = NoPend]
  /\ UNCHANGED <<term, votedFor, role, log, commitIndex, leaderId, heard, votes, xfer,
                 fs, landVars, buf, frs, leak, envWant, envActive, ghostVars, cntVars>>

\* a lightweight heartbeat: no term, no sequence
HandleLW(m) ==
  LET f == m.dst  ok == leaderId[f] = m.src /\ frs[f] # "active" IN
  /\ m.ty = "lw"
  /\ \E keep \in Keeps :
       Net(m, keep, {[ty |-> "lwr", src |-> f, dst |-> m.src, ok |-> ok, ft |-> term[f]]})
  /\ heard' = [heard EXCEPT ![f] = heard[f] \/ ok]
  /\ UNCHANGED <<term, votedFor, role, log, flushed, commitIndex, leaderId, votes, xfer,
                 fs, landVars, rpVars, ghostVars, cntVars>>

(*************************** redpanda: leader *****************************)
AEMsg(l, f, seq, prev, es, fl, rec) ==
  [ty |-> "ae", src |-> l, dst |-> f, term |-> term[l], sess |-> 0, seq |-> seq, hb |-> FALSE,
   prev |-> prev, pterm |-> TermAt(log[l], prev), ents |-> es, lc |-> commitIndex[l],
   fl |-> fl, rec |-> rec, gi |-> 0]
InFlightAE(l, f) == {m \in net : m.ty = "ae" /\ m.src = l /\ m.dst = f /\ m.ents # << >> /\ ~m.rec}
Pending(l, f) == \/ \E m \in net : m.src = l /\ m.dst = f /\ m.ty = "ae"
                 \/ \E m \in net : m.src = f /\ m.dst = l /\ m.ty = "aer"
                 \/ \E i \in DOMAIN buf[f] : buf[f][i].src = l
                 \/ \E i \in DOMAIN pend[f].reps : pend[f].reps[i].dst = l
CanSeq(l, f) == ~UseSeq \/ fs[l][f].sent < MaxSeq
NextSeq(l, f) == IF UseSeq THEN fs[l][f].sent + 1 ELSE 0
RpLeaderUnchanged == UNCHANGED <<coreVars, landVars, rpVars, ghostVars, cntVars>>

\* replicate_entries_stm: the next entry, to a follower expected at its end
SendAppend(l, f) ==
  LET fr == fs[l][f] IN
  /\ ~Landed /\ role[l] = "L" /\ f # l
  /\ ~fr.recov /\ fr.exp >= 0 /\ fr.exp < Len(log[l])
  /\ Cardinality(InFlightAE(l, f)) < Window
  /\ CanSeq(l, f)
  /\ Send({AEMsg(l, f, NextSeq(l, f), fr.exp, << log[l][fr.exp + 1] >>, TRUE, FALSE)})
  /\ fs' = [fs EXCEPT ![l][f].exp = fr.exp + 1, ![l][f].sent = NextSeq(l, f)]
  /\ RpLeaderUnchanged

\* heartbeat_manager: lightweight unless a full one is needed
LwOK(l, f) ==
  LET fr == fs[l][f] IN
  /\ LwHb
  /\ fr.meta = << commitIndex[l], Len(log[l]) >>
  /\ fr.ff = flushed[l]
  /\ IF UseSeq THEN fr.sent = fr.succ ELSE fr.ok /\ ~Pending(l, f)

SendHeartbeat(l, f) ==
  /\ ~Landed /\ role[l] = "L" /\ f # l
  /\ ~\E m \in net : m.src = l /\ m.dst = f /\ (m.ty = "lw" \/ (m.ty = "ae" /\ m.ents = << >>))
  /\ IF LwOK(l, f)
       THEN Send({[ty |-> "lw", src |-> l, dst |-> f]}) /\ UNCHANGED fs
       ELSE /\ CanSeq(l, f)
            /\ Send({AEMsg(l, f, NextSeq(l, f), Len(log[l]), << >>, FALSE, FALSE)})
            /\ fs' = [fs EXCEPT ![l][f].sent = NextSeq(l, f),
                                ![l][f].meta = << commitIndex[l], Len(log[l]) >>]
  /\ RpLeaderUnchanged

\* recovery_stm: one request at a time from next; the flush only sometimes
RecoverySend(l, f) ==
  LET fr == fs[l][f]  i == Max(1, Min(fr.next, Len(log[l]))) IN
  /\ ~Landed /\ role[l] = "L" /\ f # l
  /\ fr.recov /\ ~fr.inrec
  /\ fr.match < Len(log[l])
  /\ CanSeq(l, f)
  /\ \E fl \in BOOLEAN : Send({AEMsg(l, f, NextSeq(l, f), i - 1, << log[l][i] >>, fl, TRUE)})
  /\ fs' = [fs EXCEPT ![l][f].sent = NextSeq(l, f), ![l][f].inrec = TRUE]
  /\ RpLeaderUnchanged

RecoveryDone(l, f) ==
  /\ ~Landed /\ role[l] = "L"
  /\ fs[l][f].recov /\ ~fs[l][f].inrec
  /\ fs[l][f].match = Len(log[l])
  /\ fs' = [fs EXCEPT ![l][f].recov = FALSE]
  /\ UNCHANGED netVars /\ RpLeaderUnchanged

\* the outstanding recovery request or its reply was lost: recovery aborts
RecoveryTimeout(l, f) ==
  /\ ~Landed /\ fs[l][f].recov /\ fs[l][f].inrec
  /\ ~\E m \in net : m.ty \in {"ae", "aer"} /\ m.rec /\ {m.src, m.dst} = {l, f}
  /\ ~\E i \in DOMAIN buf[f] : buf[f][i].src = l /\ buf[f][i].rec
  /\ ~\E i \in DOMAIN pend[f].reps : pend[f].reps[i].dst = l /\ pend[f].reps[i].rec
  /\ fs' = [fs EXCEPT ![l][f].recov = FALSE, ![l][f].inrec = FALSE]
  /\ UNCHANGED netVars /\ RpLeaderUnchanged

NeedsRec(fr, ld) == fr.match < ld \/ fr.match > fr.fd

\* consensus::update_follower_index, and recovery_stm's reply handling
HandleAER(m) ==
  LET l == m.dst  f == m.src  fr == fs[l][f]
      stale == \/ role[l] # "L"
               \/ SessionCheck /\ m.qt # term[l]
               \/ SeqCheck = "redpanda" /\ m.seq < fr.recv /\ m.d < Len(log[l])
               \/ SeqCheck = "strict" /\ m.seq < fr.recv
      recv1 == Min(Max(m.seq, fr.recv), fr.sent)
      fr1 == [fr EXCEPT !.recv = recv1, !.inrec = IF m.rec THEN FALSE ELSE fr.inrec]
      fr2 == IF ~fr.recov THEN [fr1 EXCEPT !.fd = m.d, !.ff = m.f, !.next = m.d + 1] ELSE fr1
      abort == fr.recov /\ m.rec /\ m.seq < fr2.recv     \* recovery_stm: reordered
      frOk == [fr2 EXCEPT !.fd = m.d, !.ff = m.f, !.match = m.d, !.next = m.d + 1,
                          !.succ = recv1, !.ok = TRUE, !.exp = Max(m.d, fr.exp),
                          !.dur = Max(fr.dur, Min(m.d, m.f)), !.recov = fr.recov /\ ~abort]
      fr3 == [fr2 EXCEPT !.exp = -1, !.ok = FALSE]
      frBad ==
        IF fr.recov
        THEN LET a == IF fr3.fd > m.d THEN [fr3 EXCEPT !.fd = m.d, !.ff = m.f, !.next = m.d + 1]
                       ELSE fr3
                 b == IF m.rec THEN [a EXCEPT !.next = Max(1, m.pv)] ELSE a
             IN [b EXCEPT !.recov = ~abort]
        ELSE IF NeedsRec(fr3, Len(log[l])) /\ m.mr
        THEN \* dispatch_recovery: the follower admitted it
             [fr3 EXCEPT !.recov = TRUE, !.inrec = FALSE,
                         !.next = IF fr3.fd >= Len(log[l]) THEN Len(log[l]) ELSE fr3.next]
        ELSE fr3
  IN
  /\ ~Landed /\ m.ty = "aer"
  /\ \E keep \in Keeps : Net(m, keep, {})
  /\ IF stale
       THEN UNCHANGED <<coreVars, fs>>
     ELSE IF m.term > term[l]
       THEN StepDown(l, m.term) /\ UNCHANGED fs
     ELSE fs' = [fs EXCEPT ![l][f] = IF m.ok THEN frOk ELSE frBad] /\ UNCHANGED coreVars
  /\ UNCHANGED <<landVars, rpVars, ghostVars, cntVars>>

\* a failed lightweight heartbeat forces the next one to be full
HandleLWR(m) ==
  /\ m.ty = "lwr"
  /\ \E keep \in Keeps : Net(m, keep, {})
  /\ IF ~m.ok /\ role[m.dst] = "L"
       THEN fs' = [fs EXCEPT ![m.dst][m.src].meta = << >>]
       ELSE UNCHANGED fs
  /\ UNCHANGED <<coreVars, landVars, rpVars, ghostVars, cntVars>>

(*************************** commit ***************************************)
\* what each replica counts for in the leader's quorum
View(l, f) ==
  IF f = l THEN (IF LeaderSelf = "flushed" THEN flushed[l] ELSE Len(log[l]))
  ELSE CASE AckRule = "permatch" -> fs[l][f].dur
         [] AckRule = "redpanda" -> Min(fs[l][f].match, fs[l][f].ff)
         [] AckRule = "dirty"    -> fs[l][f].match
QuorumMatch(l) == CHOOSE n \in 0..Len(log[l]) :
                    /\ \E Q \in Quorums : \A q \in Q : View(l, q) >= n
                    /\ ~\E Q \in Quorums : \A q \in Q : View(l, q) >= n + 1

AdvanceCommit(l) ==
  LET n == IF LeaderClamp THEN Min(QuorumMatch(l), flushed[l]) ELSE QuorumMatch(l) IN
  /\ role[l] = "L"
  /\ n > commitIndex[l]
  /\ log[l][n][1] = term[l]
  /\ commitIndex' = [commitIndex EXCEPT ![l] = n]
  /\ clog' = IF n > Len(clog) THEN SubSeq(log[l], 1, n) ELSE clog
  /\ clogT' = IF n > Len(clog) THEN clogT \o [i \in 1..(n - Len(clog)) |-> term[l]] ELSE clogT
  /\ UNCHANGED <<term, votedFor, role, log, flushed, leaderId, heard, votes, xfer,
                 fs, landVars, rpVars, netVars, elected, ginc, lgi, cntVars>>

\* a background fsync: the leader's own log (and any replica's, Redpanda);
\* a landed follower fsyncs in LReplyFlushed
Flush(s) ==
  /\ ~Landed \/ role[s] = "L"
  /\ flushed[s] < Len(log[s])
  /\ flushed' = [flushed EXCEPT ![s] = Len(log[s])]
  /\ UNCHANGED <<term, votedFor, role, log, commitIndex, leaderId, heard, votes, xfer,
                 fs, landVars, rpVars, netVars, ghostVars, cntVars>>

ClientAppend(l) ==
  /\ role[l] = "L" /\ ~xfer[l]
  /\ Landed => Lease(l)
  /\ nClient < MaxClient
  /\ log' = [log EXCEPT ![l] = Append(log[l], << term[l], nClient + 1 >>)]
  /\ nClient' = nClient + 1
  /\ UNCHANGED <<term, votedFor, role, flushed, commitIndex, leaderId, heard, votes, xfer,
                 fs, landVars, rpVars, netVars, ghostVars, nCrash, nDrop, nFault, nXfer, nEnv>>

(*************************** elections ************************************)
UpToDate(lt, li, l) == lt > LastTerm(l) \/ (lt = LastTerm(l) /\ li >= Len(l))
\* stickiness (Redpanda) / a live leader lease (openraft): a leader, or a
\* node that heard from one recently, refuses (pre)votes
Sticky(v, m) ==
  /\ role[v] = "L" \/ heard[v]
  /\ ~(m.x /\ XferException)
  /\ ~Landed => votedFor[v] # m.src
LiveLeaderOf(s) == \E l \in Servers : role[l] = "L" /\ term[l] = term[s]

\* the new leader's state; it appends its no-op
LeaderInit(c, t) ==
  /\ log' = [log EXCEPT ![c] = Append(log[c], << t, 0 >>)]
  /\ fs' = [fs EXCEPT ![c] = [f \in Servers |-> FInit(Len(log[c]))]]
  /\ ctr' = [ctr EXCEPT ![c] = 1]
  /\ elected' = elected \cup {<< t, c >>}
  /\ ginc' = ginc + 1
  /\ lgi' = [lgi EXCEPT ![c] = ginc + 1]

\* AtomicElect: c wins the next term with the votes of Q, none of whose logs
\* is more up to date than c's. Under FreeNet a refused vote (stickiness, a
\* lease) is the same as a lost vote request, so it adds no behaviour here.
\* the newest term has no leader
NoLeader == ~\E l \in Servers : role[l] = "L" /\ \A s \in Servers : term[s] <= term[l]
\* HbLeaseRule: a quorum of followers no longer renews the leader's lease
LeaseGone == \E Q \in Quorums : \A q \in Q : role[q] # "L" /\ nolease[q]

AtomicElection(c, Q) ==
  LET t == 1 + CHOOSE x \in {term[q] : q \in Q} : \A q \in Q : term[q] <= x IN
  /\ AtomicElect
  /\ c \in Q /\ t <= MaxTerm
  \* an election nobody needed is a spurious timeout: a fault
  /\ IF NoLeader \/ LeaseGone THEN UNCHANGED nFault ELSE Fault
  /\ \A q \in Q : UpToDate(LastTerm(log[c]), Len(log[c]), log[q])
  /\ term' = [s \in Servers |-> IF s \in Q THEN t ELSE term[s]]
  /\ role' = [s \in Servers |-> IF s = c THEN "L" ELSE IF s \in Q THEN "F" ELSE role[s]]
  /\ leaderId' = [s \in Servers |-> LID(IF s = c THEN c ELSE IF s \in Q THEN Nil ELSE leaderId[s],
                                       leaderId[s])]
  /\ UNCHANGED <<votedFor, heard>>
  /\ xfer' = [s \in Servers |-> IF s \in Q THEN FALSE ELSE xfer[s]]
  /\ LeaderInit(c, t)
  /\ UNCHANGED <<flushed, commitIndex, votes, fsess, fnext, fgi, wbuf, lpend, nolease, rpVars, netVars,
                 clog, clogT, nCrash, nDrop, nClient, nXfer, nEnv>>

\* the election timeout (or lease) passes without word from a leader
Expire(s) ==
  /\ ~AtomicElect
  /\ heard[s] /\ role[s] # "L"
  /\ IF LiveLeaderOf(s) /\ ~nolease[s] THEN Fault ELSE UNCHANGED nFault
  /\ heard' = [heard EXCEPT ![s] = FALSE]
  /\ UNCHANGED <<term, votedFor, role, log, flushed, commitIndex, leaderId, votes, xfer,
                 fs, landVars, rpVars, netVars, ghostVars, nCrash, nDrop, nClient, nXfer, nEnv>>
ExpireOK(s) == Expire(s) /\ (~LiveLeaderOf(s) \/ nolease[s])

VoteMsgs(c, t, x) ==
  {[ty |-> "v", src |-> c, dst |-> v, term |-> t, lt |-> LastTerm(log[c]),
    li |-> Len(log[c]), x |-> x] : v \in Servers \ {c}}

\* c becomes a candidate in its next term; a split vote (another candidate
\* already in that term) is a fault
Elect(c, x) ==
  /\ term[c] < MaxTerm
  /\ IF \E y \in Servers \ {c} : role[y] = "C" /\ term[y] = term[c] + 1
       THEN Fault ELSE UNCHANGED nFault
  /\ term' = [term EXCEPT ![c] = term[c] + 1]
  /\ votedFor' = [votedFor EXCEPT ![c] = c]
  /\ role' = [role EXCEPT ![c] = "C"]
  /\ leaderId' = [leaderId EXCEPT ![c] = Nil]
  /\ votes' = [votes EXCEPT ![c] = {c}]
  /\ xfer' = [xfer EXCEPT ![c] = x]
  /\ UNCHANGED <<log, flushed, commitIndex, heard>>

Timeout(s) ==
  /\ ~AtomicElect
  /\ role[s] # "L" /\ ~heard[s]
  /\ term[s] < MaxTerm
  /\ IF Prevote
       THEN /\ role' = [role EXCEPT ![s] = "PC"]
            /\ leaderId' = [leaderId EXCEPT ![s] = Nil]
            /\ votes' = [votes EXCEPT ![s] = {s}]
            /\ Send({[ty |-> "pv", src |-> s, dst |-> v, term |-> term[s] + 1,
                      lt |-> LastTerm(log[s]), li |-> Len(log[s]), x |-> FALSE] : v \in Servers \ {s}})
            /\ UNCHANGED <<term, votedFor, xfer, log, flushed, commitIndex, heard, nFault>>
       ELSE /\ Elect(s, FALSE)
            /\ Send(VoteMsgs(s, term[s] + 1, FALSE))
  /\ UNCHANGED <<fs, landVars, rpVars, ghostVars, nCrash, nDrop, nClient, nXfer, nEnv>>

HandlePV(m) ==
  LET v == m.dst
      ok == ~Sticky(v, m) /\ m.term > term[v] /\ UpToDate(m.lt, m.li, log[v])
  IN
  /\ m.ty = "pv"
  /\ \E keep \in Keeps :
       Net(m, keep, {[ty |-> "pvr", src |-> v, dst |-> m.src, term |-> term[v], ok |-> ok,
                      pt |-> m.term]})
  /\ UNCHANGED <<coreVars, fs, landVars, rpVars, ghostVars, cntVars>>

HandlePVR(m) ==
  LET c == m.dst IN
  /\ m.ty = "pvr"
  /\ \E keep \in Keeps :
     IF role[c] # "PC" \/ m.pt # term[c] + 1 \/ (m.term <= term[c] /\ ~m.ok)
       THEN Net(m, keep, {}) /\ UNCHANGED <<coreVars, nFault>>
     ELSE IF m.term > term[c]
       THEN /\ Net(m, keep, {})
            /\ term' = [term EXCEPT ![c] = m.term]
            /\ votedFor' = [votedFor EXCEPT ![c] = Nil]
            /\ role' = [role EXCEPT ![c] = "F"]
            /\ UNCHANGED <<log, flushed, commitIndex, leaderId, heard, votes, xfer, nFault>>
     ELSE IF votes[c] \cup {m.src} \notin Quorums
       THEN /\ Net(m, keep, {})
            /\ votes' = [votes EXCEPT ![c] = votes[c] \cup {m.src}]
            /\ UNCHANGED <<term, votedFor, role, log, flushed, commitIndex, leaderId, heard,
                           xfer, nFault>>
     ELSE \* the prevote won: the real election
          /\ Elect(c, FALSE)
          /\ Net(m, keep, VoteMsgs(c, term[c] + 1, FALSE))
  /\ UNCHANGED <<fs, landVars, rpVars, ghostVars, nCrash, nDrop, nClient, nXfer, nEnv>>

\* do_vote / openraft's vote handler: stickiness or the lease first, then
\* the term, then the log
HandleV(m) ==
  LET v == m.dst
      up == m.term > term[v]
      logok == UpToDate(m.lt, m.li, log[v])
      vf1 == IF up THEN Nil ELSE votedFor[v]
      grant == logok /\ (vf1 = Nil \/ vf1 = m.src)
  IN
  /\ m.ty = "v"
  /\ \E keep \in Keeps :
     IF Sticky(v, m) \/ m.term < term[v]
       THEN /\ Net(m, keep, {[ty |-> "vr", src |-> v, dst |-> m.src, term |-> term[v], ok |-> FALSE]})
            /\ UNCHANGED coreVars
       ELSE /\ Net(m, keep, {[ty |-> "vr", src |-> v, dst |-> m.src, term |-> m.term, ok |-> grant]})
            /\ term' = [term EXCEPT ![v] = m.term]
            /\ votedFor' = [votedFor EXCEPT ![v] = IF grant THEN m.src ELSE vf1]
            /\ role' = [role EXCEPT ![v] = IF up THEN "F" ELSE role[v]]
            /\ leaderId' = [leaderId EXCEPT ![v] = IF up THEN Nil ELSE leaderId[v]]
            /\ xfer' = [xfer EXCEPT ![v] = IF up THEN FALSE ELSE xfer[v]]
            \* stepping down refreshes the timestamp; a refused log resets it
            /\ heard' = [heard EXCEPT ![v] = IF up THEN logok ELSE heard[v]]
            /\ UNCHANGED <<log, flushed, commitIndex, votes>>
  /\ UNCHANGED <<fs, landVars, rpVars, ghostVars, cntVars>>

HandleVR(m) ==
  LET c == m.dst IN
  /\ m.ty = "vr"
  /\ \E keep \in Keeps : Net(m, keep, {})
  /\ IF m.term > term[c]
       THEN StepDown(c, m.term) /\ UNCHANGED <<fs, ctr, elected, ginc, lgi>>
     ELSE IF role[c] # "C" \/ m.term # term[c] \/ ~m.ok
       THEN UNCHANGED <<coreVars, fs, ctr, elected, ginc, lgi>>
     ELSE IF votes[c] \cup {m.src} \notin Quorums
       THEN /\ votes' = [votes EXCEPT ![c] = votes[c] \cup {m.src}]
            /\ UNCHANGED <<term, votedFor, role, log, flushed, commitIndex, leaderId, heard,
                           xfer, fs, ctr, elected, ginc, lgi>>
     ELSE \* elected
          /\ role' = [role EXCEPT ![c] = "L"]
          /\ leaderId' = [leaderId EXCEPT ![c] = c]
          /\ heard' = [heard EXCEPT ![c] = TRUE]
          /\ xfer' = [xfer EXCEPT ![c] = FALSE]
          /\ votes' = [votes EXCEPT ![c] = {}]
          /\ LeaderInit(c, term[c])
          /\ UNCHANGED <<term, votedFor, flushed, commitIndex>>
  /\ UNCHANGED <<fsess, fnext, fgi, wbuf, lpend, nolease, rpVars, clog, clogT, cntVars>>

(*************************** leadership transfer **************************)
StartTransfer(l, t) ==
  /\ ~AtomicElect
  /\ role[l] = "L" /\ ~xfer[l] /\ t # l
  /\ nXfer < MaxXfer
  /\ fs[l][t].match = Len(log[l])
  /\ xfer' = [xfer EXCEPT ![l] = TRUE]
  /\ nXfer' = nXfer + 1
  /\ Send({[ty |-> "tn", src |-> l, dst |-> t, term |-> term[l]]})
  /\ UNCHANGED <<term, votedFor, role, log, flushed, commitIndex, leaderId, heard, votes,
                 fs, landVars, rpVars, ghostVars, nCrash, nDrop, nFault, nClient, nEnv>>

\* timeout_now / openraft TransferLeader: the target elects at once
HandleTN(m) ==
  LET t == m.dst  ok == m.term = term[t] /\ role[t] = "F" /\ term[t] < MaxTerm IN
  /\ m.ty = "tn"
  /\ \E keep \in Keeps :
     IF ok
       THEN /\ Elect(t, TRUE)
            /\ Net(m, keep, VoteMsgs(t, term[t] + 1, TRUE) \cup
                            {[ty |-> "tnr", src |-> t, dst |-> m.src, term |-> m.term, ok |-> TRUE]})
       ELSE /\ Net(m, keep, {[ty |-> "tnr", src |-> t, dst |-> m.src, term |-> m.term, ok |-> FALSE]})
            /\ UNCHANGED <<coreVars, nFault>>
  /\ UNCHANGED <<fs, landVars, rpVars, ghostVars, nCrash, nDrop, nClient, nXfer, nEnv>>

\* the leader steps down once the target started its election
HandleTNR(m) ==
  LET l == m.dst  go == role[l] = "L" /\ term[l] = m.term /\ xfer[l] IN
  /\ m.ty = "tnr"
  /\ \E keep \in Keeps : Net(m, keep, {})
  /\ role' = [role EXCEPT ![l] = IF go /\ m.ok THEN "F" ELSE role[l]]
  /\ leaderId' = [leaderId EXCEPT ![l] = IF go /\ m.ok THEN Nil ELSE leaderId[l]]
  /\ xfer' = [xfer EXCEPT ![l] = IF go THEN FALSE ELSE xfer[l]]
  /\ UNCHANGED <<term, votedFor, log, flushed, commitIndex, heard, votes,
                 fs, landVars, rpVars, ghostVars, cntVars>>

XferTimeout(l) ==
  /\ xfer[l] /\ role[l] = "L"
  /\ ~\E m \in net : m.ty \in {"tn", "tnr"} /\ (m.src = l \/ m.dst = l)
  /\ xfer' = [xfer EXCEPT ![l] = FALSE]
  /\ UNCHANGED <<term, votedFor, role, log, flushed, commitIndex, leaderId, heard, votes,
                 fs, landVars, rpVars, netVars, ghostVars, cntVars>>

(*************************** redpanda: recovery slots *********************)
Active(s) == (IF frs[s] = "active" THEN 1 ELSE 0) + (IF envActive[s] THEN 1 ELSE 0)
             + (IF leak[s] THEN 1 ELSE 0)
GrantSlot(s) ==
  /\ ~Landed /\ frs[s] = "pending" /\ Active(s) < MaxSlots
  /\ frs' = [frs EXCEPT ![s] = "active"]
  /\ UNCHANGED <<coreVars, fs, landVars, buf, pend, leak, envWant, envActive, netVars,
                 ghostVars, cntVars>>
EnvRequest(s) ==
  /\ nEnv < MaxEnv /\ ~envWant[s] /\ ~envActive[s]
  /\ envWant' = [envWant EXCEPT ![s] = TRUE]
  /\ nEnv' = nEnv + 1
  /\ UNCHANGED <<coreVars, fs, landVars, buf, pend, frs, leak, envActive, netVars, ghostVars,
                 nCrash, nDrop, nFault, nClient, nXfer>>
EnvGrant(s) ==
  /\ envWant[s] /\ Active(s) < MaxSlots
  /\ envWant' = [envWant EXCEPT ![s] = FALSE]
  /\ envActive' = [envActive EXCEPT ![s] = TRUE]
  /\ UNCHANGED <<coreVars, fs, landVars, buf, pend, frs, leak, netVars, ghostVars, cntVars>>
EnvDone(s) ==
  /\ envActive[s]
  /\ envActive' = [envActive EXCEPT ![s] = FALSE]
  /\ UNCHANGED <<coreVars, fs, landVars, buf, pend, frs, leak, envWant, netVars, ghostVars,
                 cntVars>>

(*************************** failures *************************************)
\* crash and restart: term, vote and the flushed log survive. Landed: the
\* node's connections fail, so every message to or from it is lost.
Crash(s) ==
  LET restore == LeaderRestore /\ role[s] = "L"
      nl == SubSeq(log[s], 1, flushed[s])
  IN
  /\ nCrash < MaxCrash
  /\ nCrash' = nCrash + 1
  /\ log' = [log EXCEPT ![s] = nl]
  /\ role' = [role EXCEPT ![s] = IF restore THEN "L" ELSE "F"]
  /\ leaderId' = [leaderId EXCEPT ![s] = IF restore THEN @ ELSE Nil]
  /\ heard' = [heard EXCEPT ![s] = IF restore THEN @ ELSE FALSE]
  /\ votes' = [votes EXCEPT ![s] = {}]
  /\ xfer' = [xfer EXCEPT ![s] = FALSE]
  /\ commitIndex' = [commitIndex EXCEPT ![s] = Min(commitIndex[s], flushed[s])]
  \* the per-core session counter restarts at 0
  /\ ctr' = [ctr EXCEPT ![s] = IF restore THEN 1 ELSE 0]
  /\ fs' = IF restore THEN [fs EXCEPT ![s] = [f \in Servers |-> FInit(Max(0, Len(nl) - 1))]]
          ELSE fs
  /\ ginc' = IF restore THEN ginc + 1 ELSE ginc
  /\ lgi' = IF restore THEN [lgi EXCEPT ![s] = ginc + 1] ELSE lgi
  /\ fsess' = [fsess EXCEPT ![s] = NoSess]
  /\ fnext' = [fnext EXCEPT ![s] = 0]
  /\ fgi' = [fgi EXCEPT ![s] = 0]
  /\ nolease' = [nolease EXCEPT ![s] = FALSE]
  /\ wbuf' = [wbuf EXCEPT ![s] = {}]
  /\ lpend' = [lpend EXCEPT ![s] = {}]
  /\ buf' = [buf EXCEPT ![s] = << >>]
  /\ pend' = [pend EXCEPT ![s] = NoPend]
  /\ frs' = [frs EXCEPT ![s] = "none"]
  /\ leak' = [leak EXCEPT ![s] = FALSE]
  /\ net' = IF Landed THEN {m \in net : m.src # s /\ m.dst # s} ELSE net
  /\ UNCHANGED <<term, votedFor, flushed, envWant, envActive, nSent, nDup, elected, clog, clogT,
                 nDrop, nFault, nClient, nXfer, nEnv>>

Drop(m) ==
  /\ FreeNet \/ nDrop < MaxDrop
  /\ net' = net \ {m}
  /\ nDrop' = IF FreeNet THEN nDrop ELSE nDrop + 1
  /\ UNCHANGED <<coreVars, fs, landVars, rpVars, nSent, nDup, ghostVars,
                 nCrash, nFault, nClient, nXfer, nEnv>>

\* landed: a connection fails; every message between the two is lost
ConnFail(a, b) ==
  /\ Landed /\ a # b
  /\ ~FreeNet /\ nDrop < MaxDrop      \* under FreeNet it is several Drops
  /\ \E m \in net : {m.src, m.dst} = {a, b}
  /\ net' = {m \in net : {m.src, m.dst} # {a, b}}
  /\ nDrop' = IF FreeNet THEN nDrop ELSE nDrop + 1
  /\ UNCHANGED <<coreVars, fs, landVars, rpVars, nSent, nDup, ghostVars,
                 nCrash, nFault, nClient, nXfer, nEnv>>

(*************************** spec *****************************************)
Init ==
  /\ term = [s \in Servers |-> 0]
  /\ votedFor = [s \in Servers |-> Nil]
  /\ role = [s \in Servers |-> "F"]
  /\ log = [s \in Servers |-> << >>]
  /\ flushed = [s \in Servers |-> 0]
  /\ commitIndex = [s \in Servers |-> 0]
  /\ leaderId = [s \in Servers |-> Nil]
  /\ heard = [s \in Servers |-> FALSE]
  /\ votes = [s \in Servers |-> {}]
  /\ xfer = [s \in Servers |-> FALSE]
  /\ fs = [s \in Servers |-> [f \in Servers |-> FInit(0)]]
  /\ ctr = [s \in Servers |-> 0]
  /\ fsess = [s \in Servers |-> NoSess]
  /\ fgi = [s \in Servers |-> 0]
  /\ nolease = [s \in Servers |-> FALSE]
  /\ ginc = 0 /\ lgi = [s \in Servers |-> 0]
  /\ fnext = [s \in Servers |-> 0]
  /\ wbuf = [s \in Servers |-> {}]
  /\ lpend = [s \in Servers |-> {}]
  /\ buf = [s \in Servers |-> << >>]
  /\ pend = [s \in Servers |-> NoPend]
  /\ frs = [s \in Servers |-> "none"]
  /\ leak = [s \in Servers |-> FALSE]
  /\ envWant = [s \in Servers |-> FALSE]
  /\ envActive = [s \in Servers |-> FALSE]
  /\ net = {} /\ nSent = 0
  /\ elected = {} /\ clog = << >> /\ clogT = << >>
  /\ nCrash = 0 /\ nDrop = 0 /\ nDup = 0 /\ nFault = 0 /\ nClient = 0 /\ nXfer = 0 /\ nEnv = 0

Receive(m) ==
  \/ LDeliver(m) \/ LHeartbeatIn(m) \/ LHandleAER(m) \/ LHandleHbR(m)
  \/ DeliverAE(m) \/ HandleAER(m) \/ HandleLW(m) \/ HandleLWR(m)
  \/ HandlePV(m) \/ HandlePVR(m) \/ HandleV(m) \/ HandleVR(m)
  \/ HandleTN(m) \/ HandleTNR(m)

Next ==
  \/ \E s \in Servers :
       \/ LEngine(s) \/ LReplyFlushed(s) \/ \E r \in wbuf[s] : LGap(s, r)
       \/ LCtrReset(s)
       \/ ProcessBatch(s) \/ FlushReply(s) \/ GrantSlot(s)
       \/ EnvRequest(s) \/ EnvGrant(s) \/ EnvDone(s)
       \/ Flush(s) \/ ClientAppend(s) \/ AdvanceCommit(s) \/ XferTimeout(s)
       \/ Expire(s) \/ Timeout(s) \/ Crash(s)
       \/ \E Q \in Quorums : AtomicElection(s, Q)
       \/ \E f \in Servers :
            \/ LSend(s, f) \/ LHeartbeat(s, f) \/ LStreamTimeout(s, f)
            \/ SendAppend(s, f) \/ SendHeartbeat(s, f) \/ RecoverySend(s, f)
            \/ RecoveryDone(s, f) \/ RecoveryTimeout(s, f) \/ StartTransfer(s, f)
            \/ ConnFail(s, f)
  \/ \E m \in net : Receive(m) \/ Drop(m)

Spec == Init /\ [][Next]_vars

MsgTypes == {"ae", "aer", "lw", "lwr", "pv", "pvr", "v", "vr", "tn", "tnr"}
\* a message's fairness class: heartbeats (no entries) and their replies
\* apart from appends, so constant heartbeats cannot starve an append
Class(m) == << m.ty, IF m.ty = "ae" THEN m.ents = << >> ELSE IF m.ty = "aer" THEN m.he ELSE FALSE >>
\* delivery without duplication, per (source, destination, class)
RecvOK(a, b, ty, e) ==
  \E m \in net : m.src = a /\ m.dst = b /\ Class(m) = << ty, e >> /\ Receive(m) /\ nDup' = nDup

\* weak fairness on every protocol step and on timers that fire because
\* something is really lost; none on client requests, transfers, other
\* groups' requests, and failures (crashes, drops, connection failures,
\* duplicates, early expiry or gap timeouts, split votes), which are
\* therefore finite in every behaviour
Fairness ==
  /\ \A s \in Servers :
       /\ WF_vars(LEngine(s)) /\ WF_vars(LReplyFlushed(s))
       /\ WF_vars(\E r \in wbuf[s] : GapReal(s, r) /\ LGap(s, r))
       /\ WF_vars(ProcessBatch(s)) /\ WF_vars(FlushReply(s)) /\ WF_vars(GrantSlot(s))
       /\ WF_vars(EnvGrant(s)) /\ WF_vars(EnvDone(s))
       /\ WF_vars(Flush(s)) /\ WF_vars(AdvanceCommit(s)) /\ WF_vars(XferTimeout(s))
       /\ WF_vars(ExpireOK(s)) /\ WF_vars(Timeout(s) /\ nFault' = nFault)
       /\ \A f \in Servers :
            /\ WF_vars(LSend(s, f)) /\ WF_vars(LHeartbeat(s, f)) /\ WF_vars(LStreamTimeout(s, f))
            /\ WF_vars(SendAppend(s, f)) /\ WF_vars(SendHeartbeat(s, f))
            /\ WF_vars(RecoverySend(s, f)) /\ WF_vars(RecoveryDone(s, f))
            /\ WF_vars(RecoveryTimeout(s, f))
  /\ \A a, b \in Servers : \A ty \in MsgTypes : \A e \in BOOLEAN : WF_vars(RecvOK(a, b, ty, e))

\* AtomicElect liveness configs: some election happens while the newest term
\* has no leader
ElectFair == WF_vars((NoLeader \/ LeaseGone) /\ \E c \in Servers : \E Q \in Quorums : AtomicElection(c, Q))

LiveSpec == Spec /\ Fairness /\ ElectFair

SentBound == Budget = 0 \/ nSent <= Budget

(*************************** safety ***************************************)
TypeOK ==
  /\ role \in [Servers -> {"F", "PC", "C", "L"}]
  /\ \A s \in Servers : flushed[s] <= Len(log[s]) /\ term[s] <= MaxTerm

\* at most one leader per term
ElectionSafety == \A x, y \in elected : x[1] = y[1] => x[2] = y[2]

\* two logs with an entry of the same index and term agree up to it
LogMatching ==
  \A s, t \in Servers : \A i \in 1..Min(Len(log[s]), Len(log[t])) :
    log[s][i][1] = log[t][i][1] => SubSeq(log[s], 1, i) = SubSeq(log[t], 1, i)

\* every replica's commit prefix is the committed log
CommitAgree ==
  \A s \in Servers :
    /\ commitIndex[s] <= Len(log[s])
    /\ commitIndex[s] <= Len(clog)
    /\ SubSeq(log[s], 1, commitIndex[s]) = SubSeq(clog, 1, commitIndex[s])

\* a leader holds every entry committed in an earlier or its own term
LeaderCompleteness ==
  \A l \in Servers : role[l] = "L" =>
    \A i \in 1..Len(clog) : clogT[i] <= term[l] => i <= Len(log[l]) /\ log[l][i] = clog[i]

\* a replica applies its commit prefix, so commit prefixes never diverge
StateMachineSafety ==
  \A s, t \in Servers :
    LET a == SubSeq(log[s], 1, Min(commitIndex[s], Len(log[s])))
        b == SubSeq(log[t], 1, Min(commitIndex[t], Len(log[t])))
    IN IsPrefix(a, b) \/ IsPrefix(b, a)

\* committed entries are durable on a majority, counting flushed entries only
DurableCommit ==
  \A i \in 1..Len(clog) : \E Q \in Quorums : \A q \in Q :
    flushed[q] >= i /\ log[q][i] = clog[i]

\* a replica's commit index never exceeds its flushed index
CommitFlushed == \A s \in Servers : commitIndex[s] <= flushed[s]

\* what a leader counts for a follower in its term is durably on that
\* follower and matches the leader's log: no reordered, duplicated, stale,
\* overtaking or trailing reply (pipelined or heartbeat) moves it past the
\* truth
AckSound ==
  \A l, f \in Servers :
    (role[l] = "L" /\ f # l /\ term[f] = term[l]) =>
      LET n == View(l, f) IN
      n <= flushed[f] /\ n <= Len(log[l]) /\ SubSeq(log[f], 1, n) = SubSeq(log[l], 1, n)

\* landed: the stream's own match is always a true prefix of the follower
MatchSound ==
  \A l, f \in Servers :
    (Landed /\ role[l] = "L" /\ f # l /\ term[f] = term[l]) =>
      LET n == fs[l][f].match IN
      n <= Len(log[f]) /\ n <= Len(log[l]) /\ SubSeq(log[f], 1, n) = SubSeq(log[l], 1, n)

\* landed: a leader's live session is rejected as stale only by a follower
\* already in a newer term (session numbers only grow within a term)
LiveSessionNotStale ==
  \A m \in net :
    (Landed /\ m.ty = "aer" /\ m.kind = "stale" /\ role[m.dst] = "L" /\ m.qt = term[m.dst]
       /\ m.sess = fs[m.dst][m.src].sc) => m.fst > m.qt

\* landed: session ids. Within one term, a session id from a later leader
\* incarnation (a restart) sorts above the one the follower holds from an
\* earlier incarnation, and no id is reused across incarnations. (Across
\* terms the term's high bits already order them.)
SessionFresh ==
  \A m \in net :
    (Landed /\ m.ty = "ae" /\ ~m.hb /\ fsess[m.dst] # NoSess /\ m.term = fsess[m.dst][1]) =>
      /\ m.gi > fgi[m.dst] => Older(fsess[m.dst], SessOf(m))
      /\ m.sess = fsess[m.dst][2] => m.gi = fgi[m.dst]

\* redpanda: a lightweight heartbeat carries no term, so a follower accepts
\* one only from the leader of its own term: a success never hides a higher
\* term from a deposed leader, and stickiness only pins a node to the
\* leader of its own term
LwSound ==
  /\ \A m \in net : m.ty = "lwr" /\ m.ok => << m.ft, m.dst >> \in elected /\ m.ft <= term[m.dst]
  /\ \A s \in Servers : leaderId[s] \notin {Nil, s} => << term[s], leaderId[s] >> \in elected

Consensus ==
  /\ TypeOK /\ ElectionSafety /\ LogMatching /\ CommitAgree /\ LeaderCompleteness
  /\ StateMachineSafety /\ DurableCommit
Safety == Consensus /\ CommitFlushed /\ AckSound /\ MatchSound /\ LwSound /\ LiveSessionNotStale
          /\ SessionFresh

(*************************** liveness *************************************)
StableLeader == \E l \in Servers : role[l] = "L" /\ \A s \in Servers : term[s] = term[l]
\* a leader is eventually elected, and stays once failures stop
LeaderElected == <>[]StableLeader
\* no follower is left behind: every log converges to the leader's
NoStarvation == <>[](\E l \in Servers : role[l] = "L" /\ \A s \in Servers : log[s] = log[l])
\* everything is eventually committed and known committed (so applied)
\* everywhere: a replica applies its commit prefix in a local loop
AllApplied == <>[](\E l \in Servers : role[l] = "L" /\ commitIndex[l] = Len(log[l])
                     /\ \A s \in Servers : commitIndex[s] = Len(log[l]))
\* the group makes progress: eventually a leader has committed its whole log
GroupCommits == <>[](\E l \in Servers : role[l] = "L" /\ commitIndex[l] = Len(log[l]))
\* other groups' recoveries on a node are admitted
EnvServed == \A s \in Servers : envWant[s] ~> envActive[s]
\* a transfer election wins in its own term
TransferWins ==
  \A s \in Servers : \A k \in 1..MaxTerm :
    (role[s] = "C" /\ xfer[s] /\ term[s] = k) ~> (role[s] = "L" /\ term[s] = k)
\* landed: a stream with work outstanding always ends in progress
StreamProgress ==
  \A l, f \in Servers :
    (role[l] = "L" /\ f # l /\ fs[l][f].match < Len(log[l]))
      ~> (role[l] # "L" \/ fs[l][f].match = Len(log[l]))

Sym == Permutations(Servers)

(*************************** reachability (sanity) ************************)
(* Each is EXPECTED TO FAIL: the scenario it negates is reachable.         *)
NeverCommit == Len(clog) = 0
NeverCommitTerm2 == ~\E i \in 1..Len(clog) : clog[i][1] = 2
NeverTruncate == \A s \in Servers : \A i \in 1..Len(log[s]) :
                   i <= Len(clog) => log[s][i] = clog[i]
\* landed
NeverTwoStreams ==        \* two leaders' requests in flight to one follower
  ~\E m1, m2 \in net : m1.ty = "ae" /\ m2.ty = "ae" /\ ~m1.hb /\ ~m2.hb
     /\ m1.dst = m2.dst /\ m1.src # m2.src
NeverStaleSessionRejected ==
  ~\E m \in net : m.ty = "aer" /\ m.kind = "stale" /\ m.qt < m.term
NeverRestartInTerm ==     \* a leader's second session in one term
  ~\E l, f \in Servers : role[l] = "L" /\ fs[l][f].sc > 1
NeverGapReject == ~\E m \in net : m.ty = "aer" /\ m.kind = "gap"
NeverDupReject == ~\E m \in net : m.ty = "aer" /\ m.kind = "dup"
NeverWaiting == \A s \in Servers : Cardinality(wbuf[s]) <= 0
NeverHbOvertakes ==       \* a heartbeat reply taken while results are outstanding
  ~\E m \in net : m.ty = "aer" /\ m.hb /\ m.ok /\ role[m.dst] = "L" /\ m.qt = term[m.dst]
     /\ fs[m.dst][m.src].rnext < fs[m.dst][m.src].nseq
NeverCommitAfterCrash == ~(nCrash > 0 /\ Len(clog) >= 2)
\* redpanda
NeverRecovery == \A l, f \in Servers : ~fs[l][f].recov
NeverLw == ~\E m \in net : m.ty = "lwr" /\ m.ok
NeverGroupFlush == \A s \in Servers : Len(pend[s].reps) <= 1
NeverPendingSlot == \A s \in Servers : frs[s] # "pending"
NeverReorderedAccepted ==
  ~\E l, f \in Servers : role[l] = "L" /\ fs[l][f].recv > 0 /\
     \E m \in net : m.ty = "aer" /\ m.dst = l /\ m.src = f /\ m.qt = term[l] /\ m.seq < fs[l][f].recv
\* elections
NeverStickyRefusal == ~\E m \in net : m.ty = "vr" /\ ~m.ok
NeverPrevoteWin == \A s \in Servers : role[s] # "C" \/ xfer[s]
=============================================================================
