---------------------------- MODULE BootstrapFound ----------------------------
(***************************************************************************)
(* The landed implementation of founding (§9.6 "Founding"), as built, and  *)
(* candidate fixes. Bootstrap.tla checks the design-level protocol (joins, *)
(* growth, re-images, fleets) with founding as one atomic step; this       *)
(* module opens that step up, at the level of Raft messages.               *)
(*                                                                         *)
(* Nodes 1..N; node ids order them. Each node's configured seed list       *)
(* cfg[n] may change when it restarts (an operator fixing a typo).         *)
(*                                                                         *)
(* The protocol as built:                                                  *)
(*  - HELLO. The lowest-id seed s of its own list asks every other seed t; *)
(*    t answers "empty" (no controller log, no cluster id) with its seed   *)
(*    list, or "full". The answer is remembered (volatile) and may go      *)
(*    stale: time of check to time of use.                                 *)
(*  - INITIALIZE. Once every other seed answered empty (Guard "seedset":   *)
(*    with exactly s's list), s writes the membership entry (all its seeds *)
(*    voters) as its local log's first entry, as openraft's initialize     *)
(*    does. Other seeds learn it only through Raft replication.            *)
(*  - Raft on the one well-known controller group: terms, votes, the       *)
(*    up-to-date rule, AppendEntries carrying the leader's whole log,      *)
(*    commit by a quorum of the leader's membership in its own term.       *)
(*    Uninitialized nodes take part (vote, accept entries), as in openraft.*)
(*  - FOUND. A leader whose committed log has no Found appends             *)
(*    Found{cluster id}; the first committed Found applies.                *)
(*  - Restart reads the local log; a wipe clears it.                       *)
(*                                                                         *)
(* IdMode (what keeps two groups' traffic apart):                          *)
(*  "none"    as built: no cluster id on the wire before Found.            *)
(*  "msg"     the id is generated at INITIALIZE and persisted with the     *)
(*            membership entry; every controller message carries it; a     *)
(*            node with an id drops messages with another; a node with     *)
(*            none accepts any.                                            *)
(*  "promise" as "msg", plus a HELLO-time reservation: the initializer     *)
(*            picks the id at HELLO; a seed that answers empty persists a  *)
(*            pledge to it, answers "busy" to any other initializer,       *)
(*            doesn't initialize its own group, and, with no id, accepts   *)
(*            controller messages only for its pledged id. PledgeExpiry:   *)
(*            the pledge is released on a timeout.                         *)
(*  "conn"    the id goes in the shep-rpc connection hello, not in each    *)
(*            message. A connection is refused if both ends have ids and   *)
(*            they differ; a node without an id accepts it. Messages on an *)
(*            open connection are all accepted.                            *)
(*  "connstrict" as "conn", plus: a node closes all its connections when   *)
(*            its id changes (adoption), and a node without an id accepts  *)
(*            connections from peers of at most one id at a time.          *)
(*  "connpromise" as "connstrict", with the promise: a node without an id  *)
(*            accepts a connection from a peer with an id only if pledged  *)
(*            to that id.                                                  *)
(*  "claim"   B1's object-store claim. Founding is a conditional create of  *)
(*            one fleet-wide object {id, membership}: the initializer       *)
(*            (after the HELLO guard) creates it, and fails if it exists.   *)
(*            Every member seed with an empty log, the initializer          *)
(*            included, initializes from the claim (the identical first     *)
(*            entry, as openraft allows on every node) while the claim is   *)
(*            not marked founded. The first node that applies Found marks   *)
(*            the claim founded; the leader proposes nothing but Found      *)
(*            before that. A node with no id refuses all controller         *)
(*            traffic; after the mark, an empty node stays out until join.  *)
(* Cluster traffic after founding (Data, standing in for partition groups) *)
(* rides the same connections.                                             *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS Scenario, Guard, IdMode, PledgeExpiry, MaxTerm, MaxCrashes, MaxWipes,
          MaxReconf, MaxMsgs, DataMsgs

N == IF Scenario = "typo" THEN 4 ELSE 3
Nodes == 1..N
\* initial seed lists, and the lists a restart may switch to
Cfg0 == CASE Scenario = "overlap" -> [n \in Nodes |-> IF n = 1 THEN {1, 2, 3} ELSE {2, 3}]
          [] Scenario = "typo"    -> [n \in Nodes |-> IF n = 4 THEN {3, 4} ELSE {1, 2, 3}]
          [] OTHER                -> [n \in Nodes |-> {1, 2, 3}]
Recfg(n) == IF Scenario = "typo" /\ n = 3 THEN {{3, 4}} ELSE {}

Promise == IdMode \in {"promise", "connpromise"}
Claim == IdMode = "claim"
Conn == IdMode \in {"conn", "connstrict", "connpromise"}
Strict == IdMode \in {"connstrict", "connpromise"}
Min(S) == CHOOSE x \in S : \A y \in S : x <= y
Pairs == {<<a, b>> \in Nodes \X Nodes : a < b}
Key(a, b) == IF a < b THEN <<a, b>> ELSE <<b, a>>
NoAns == [k |-> "none", l |-> {}]
Ans(k, l) == [k |-> k, l |-> l]

VARIABLES
  up, cfg,
  log, term, voted, pledge,          \* persistent
  role, votes, hello, prop, match, commit,   \* volatile
  msgs, conn,
  nextId, crashes, wipes, reconfs,
  applied,    \* ghost: cluster ids some node applied (ever)
  seen,       \* ghost: seen[n] = membership entries (groups) n's log held since its last wipe
  cross,      \* ghost: a node with a group accepted a message from another group
  initWiped,  \* ghost: an initializer was wiped after another seed got its membership
              \*   entry, before any Found applied
  initCrashed,\* ghost: ... crashed
  reinit,     \* ghost: a group was initialized after a Found applied
  wipedAfter, \* ghost: nodes wiped after a Found applied (claim: after the mark)
  store       \* object store: the founding claim [x, m, c, f] (claim mode)

vars == <<up, cfg, log, term, voted, pledge, role, votes, hello, prop, match, commit,
          msgs, conn, nextId, crashes, wipes, reconfs, applied, seen, cross,
          initWiped, initCrashed, reinit, wipedAfter, store>>
\* the ghosts, and the object store (unchanged where the ghosts are)
ghosts == <<applied, seen, cross, initWiped, initCrashed, reinit, wipedAfter, store>>

\* the membership entry: [t, k = "init", g (group tag), m (members), x (id
\* on the wire), c (creator)]; Found: [t, k = "found", g, x (cluster id)]
Gid(n) == IF log[n] = << >> THEN 0 ELSE log[n][1].g     \* ghost group tag
Id(n)  == IF log[n] = << >> \/ IdMode = "none" THEN 0 ELSE log[n][1].x
Mem(n) == IF log[n] = << >> THEN {} ELSE log[n][1].m
LastTerm(l) == IF l = << >> THEN 0 ELSE l[Len(l)].t
UpToDate(lt, ll, r) ==
  lt > LastTerm(log[r]) \/ (lt = LastTerm(log[r]) /\ ll >= Len(log[r]))
IsPrefix(a, b) == Len(a) <= Len(b) /\ SubSeq(b, 1, Len(a)) = a
FirstFound(l, k) ==
  LET F == {i \in 1..k : l[i].k = "found"} IN
  IF F = {} THEN 0 ELSE l[Min(F)].x
Founded(n) == FirstFound(log[n], commit[n]) /= 0

Init ==
  /\ up = [n \in Nodes |-> TRUE]
  /\ cfg = Cfg0
  /\ log = [n \in Nodes |-> << >>]
  /\ term = [n \in Nodes |-> 0]
  /\ voted = [n \in Nodes |-> 0]
  /\ pledge = [n \in Nodes |-> 0]
  /\ role = [n \in Nodes |-> "follower"]
  /\ votes = [n \in Nodes |-> {}]
  /\ hello = [n \in Nodes |-> [t \in Nodes |-> NoAns]]
  /\ prop = [n \in Nodes |-> 0]
  /\ match = [n \in Nodes |-> [t \in Nodes |-> 0]]
  /\ commit = [n \in Nodes |-> 0]
  /\ msgs = {}
  /\ conn = [k \in Pairs |-> [open |-> FALSE, ep |-> 0, ids |-> <<0, 0>>]]
  /\ nextId = 1
  /\ crashes = 0 /\ wipes = 0 /\ reconfs = 0
  /\ applied = {} /\ seen = [n \in Nodes |-> {}] /\ cross = FALSE
  /\ initWiped = FALSE /\ initCrashed = FALSE /\ reinit = FALSE /\ wipedAfter = {}
  /\ store = [x |-> 0, m |-> {}, c |-> 0, f |-> FALSE]

(*************************** connections ***********************************)

\* the ids each side presented when the connection opened
Peer(k, n) == IF k[1] = n THEN k[2] ELSE k[1]
PeerIdAt(k, n) == IF k[1] = n THEN conn[k].ids[2] ELSE conn[k].ids[1]
OpenPeers(n) == {k \in Pairs : n \in {k[1], k[2]} /\ conn[k].open}

\* may n accept a connection from peer p (presenting id x)?
AcceptsConn(n, x) ==
  /\ (Id(n) /= 0 /\ x /= 0) => Id(n) = x
  /\ (Strict /\ Id(n) = 0 /\ x /= 0) =>
        \A k \in OpenPeers(n) : PeerIdAt(k, n) \in {0, x}
  /\ (IdMode = "connpromise" /\ Id(n) = 0 /\ x /= 0) => pledge[n] = x

\* close every connection of n, and drop what is in flight on them
CloseAll(n) ==
  /\ conn' = [k \in Pairs |-> IF n \in {k[1], k[2]} THEN [conn[k] EXCEPT !.open = FALSE]
                              ELSE conn[k]]
  /\ msgs' = {m \in msgs : n \notin {m.from, m.to}}

\* sending: in the conn modes on an open connection, tagged with its epoch;
\* a sender with no open connection opens one first (the hello), in the
\* same step (connections open lazily, which keeps the state space small)
IsOpen(s, r) == conn[Key(s, r)].open
CanOpen(s, r) == /\ ~IsOpen(s, r) /\ conn[Key(s, r)].ep < 3
                 /\ AcceptsConn(s, Id(r)) /\ AcceptsConn(r, Id(s))
CanSend(s, r) == ~Conn \/ IsOpen(s, r) \/ CanOpen(s, r)
CanReply(s, r) == ~Conn \/ IsOpen(s, r)
Ep(s, r) == IF ~Conn THEN 0 ELSE IF IsOpen(s, r) THEN conn[Key(s, r)].ep ELSE conn[Key(s, r)].ep + 1
ConnAfter(s, r) ==
  IF Conn /\ ~IsOpen(s, r)
  THEN LET k == Key(s, r) IN
       [conn EXCEPT ![k] = [open |-> TRUE, ep |-> @.ep + 1, ids |-> <<Id(k[1]), Id(k[2])>>]]
  ELSE conn
\* accepting a delivered message at r
Accepts(r, m) ==
  IF Conn THEN conn[Key(m.from, r)].open /\ conn[Key(m.from, r)].ep = m.ep
  ELSE CASE IdMode = "none" -> TRUE
         [] IdMode = "msg"  -> Id(r) = 0 \/ Id(r) = m.x
         [] IdMode = "claim" -> Id(r) /= 0 /\ Id(r) = m.x
         [] OTHER           -> IF Id(r) /= 0 THEN Id(r) = m.x ELSE pledge[r] = m.x
\* ghost: r, in group g, takes traffic of another group
Crossed(r, m) == cross \/ (Gid(r) /= 0 /\ m.g /= 0 /\ Gid(r) /= m.g)

Msg(ty, s, r, rest) ==
  [ty |-> ty, from |-> s, to |-> r, x |-> Id(s), g |-> Gid(s), ep |-> Ep(s, r)] @@ rest

(*************************** founding **************************************)

Hello(s, t) ==
  LET X == IF prop[s] /= 0 THEN prop[s] ELSE nextId
      ans == IF log[t] /= << >> THEN Ans("full", {})
             ELSE IF Promise /\ pledge[t] \notin {0, X} THEN Ans("busy", {})
             ELSE Ans("empty", cfg[t])
  IN /\ up[s] /\ up[t] /\ log[s] = << >> /\ s = Min(cfg[s]) /\ t \in cfg[s] \ {s}
     /\ Promise => pledge[s] \in {0, X}
     /\ hello[s][t] /= ans
     /\ hello' = [hello EXCEPT ![s][t] = ans]
     /\ IF Promise
        THEN /\ prop' = [prop EXCEPT ![s] = X]
             /\ nextId' = IF prop[s] = 0 THEN nextId + 1 ELSE nextId
             /\ pledge' = [pledge EXCEPT ![s] = X,
                                         ![t] = IF ans.k = "empty" THEN X ELSE @]
        ELSE UNCHANGED <<prop, nextId, pledge>>
     /\ UNCHANGED <<up, cfg, log, term, voted, role, votes, match, commit, msgs, conn,
                    crashes, wipes, reconfs, ghosts>>

Initialize(s) ==
  LET others == cfg[s] \ {s}
      x == IF Promise THEN prop[s] ELSE nextId
  IN /\ up[s] /\ log[s] = << >> /\ s = Min(cfg[s])
     /\ \A t \in others : hello[s][t].k = "empty"
     /\ Guard = "seedset" => \A t \in others : hello[s][t].l = cfg[s]
     /\ Promise => (prop[s] /= 0 /\ pledge[s] = prop[s])
     /\ Claim => store.x = 0
     /\ store' = IF Claim THEN [x |-> nextId, m |-> cfg[s], c |-> s, f |-> FALSE] ELSE store
     /\ log' = [log EXCEPT ![s] = << [t |-> 0, k |-> "init", g |-> nextId, m |-> cfg[s],
                                       x |-> x, c |-> s] >>]
     /\ nextId' = nextId + 1
     /\ seen' = [seen EXCEPT ![s] = @ \cup {nextId}]
     /\ reinit' = (reinit \/ applied /= {})
     /\ IF Strict THEN CloseAll(s) ELSE UNCHANGED <<conn, msgs>>
     /\ UNCHANGED <<up, cfg, term, voted, pledge, role, votes, hello, prop, match, commit,
                    crashes, wipes, reconfs, applied, cross, initWiped, initCrashed, wipedAfter>>

\* claim: a member seed with an empty log initializes from the unmarked claim
Adopt(n) ==
  /\ Claim /\ up[n] /\ log[n] = << >> /\ store.x /= 0 /\ ~ store.f /\ n \in store.m
  /\ log' = [log EXCEPT ![n] = << [t |-> 0, k |-> "init", g |-> store.x, m |-> store.m,
                                    x |-> store.x, c |-> store.c] >>]
  /\ seen' = [seen EXCEPT ![n] = @ \cup {store.x}]
  /\ UNCHANGED <<up, cfg, term, voted, pledge, role, votes, hello, prop, match, commit, msgs,
                 conn, nextId, crashes, wipes, reconfs, applied, cross, initWiped, initCrashed,
                 reinit, wipedAfter, store>>

\* claim: a node that applied Found marks the claim founded
MarkFounded(n) ==
  /\ Claim /\ up[n] /\ Founded(n) /\ ~ store.f
  /\ store' = [store EXCEPT !.f = TRUE]
  /\ UNCHANGED <<up, cfg, log, term, voted, pledge, role, votes, hello, prop, match, commit,
                 msgs, conn, nextId, crashes, wipes, reconfs, applied, seen, cross, initWiped,
                 initCrashed, reinit, wipedAfter>>

(*************************** Raft ******************************************)

Campaign(n) ==
  /\ up[n] /\ n \in Mem(n) /\ role[n] /= "leader" /\ term[n] < MaxTerm
  /\ term' = [term EXCEPT ![n] = @ + 1]
  /\ voted' = [voted EXCEPT ![n] = n]
  /\ role' = [role EXCEPT ![n] = "candidate"]
  /\ votes' = [votes EXCEPT ![n] = {n}]
  /\ UNCHANGED <<up, cfg, log, pledge, hello, prop, match, commit, msgs, conn, nextId,
                 crashes, wipes, reconfs, ghosts>>

SendRV(n, r) ==
  LET m == Msg("rv", n, r, [term |-> term[n], lt |-> LastTerm(log[n]), ll |-> Len(log[n])]) IN
  /\ up[n] /\ role[n] = "candidate" /\ r \in Mem(n) \ {n} /\ r \notin votes[n]
  /\ CanSend(n, r) /\ m \notin msgs
  /\ msgs' = msgs \cup {m}
  /\ conn' = ConnAfter(n, r)
  /\ UNCHANGED <<up, cfg, log, term, voted, pledge, role, votes, hello, prop, match, commit,
                 nextId, crashes, wipes, reconfs, ghosts>>

SendAE(n, r) ==
  LET m == Msg("ae", n, r, [term |-> term[n], lg |-> log[n], cm |-> commit[n]]) IN
  /\ up[n] /\ role[n] = "leader" /\ r \in Mem(n) \ {n}
  /\ CanSend(n, r) /\ m \notin msgs
  /\ msgs' = msgs \cup {m}
  /\ conn' = ConnAfter(n, r)
  /\ UNCHANGED <<up, cfg, log, term, voted, pledge, role, votes, hello, prop, match, commit,
                 nextId, crashes, wipes, reconfs, ghosts>>

\* a higher term seen: step down
StepTo(r, t) ==
  /\ term' = [term EXCEPT ![r] = IF t > @ THEN t ELSE @]
  /\ role' = [role EXCEPT ![r] = IF t > term[r] THEN "follower" ELSE @]

Discard(m) ==
  /\ msgs' = msgs \ {m}
  /\ UNCHANGED <<up, cfg, log, term, voted, pledge, role, votes, hello, prop, match, commit,
                 conn, nextId, crashes, wipes, reconfs, ghosts>>

RecvRV(m) ==
  LET r == m.to
      t1 == IF m.term > term[r] THEN m.term ELSE term[r]
      v1 == IF m.term > term[r] THEN 0 ELSE voted[r]
      grant == m.term = t1 /\ v1 \in {0, m.from} /\ UpToDate(m.lt, m.ll, r)
      reply == [ty |-> "rvr", from |-> r, to |-> m.from, x |-> m.x, g |-> Gid(r),
                ep |-> Ep(r, m.from), term |-> t1, ok |-> grant]
  IN /\ StepTo(r, m.term)
     /\ voted' = [voted EXCEPT ![r] = IF grant THEN m.from ELSE v1]
     /\ msgs' = (msgs \ {m}) \cup (IF CanReply(r, m.from) THEN {reply} ELSE {})
     /\ cross' = Crossed(r, m)
     /\ UNCHANGED <<up, cfg, log, pledge, votes, hello, prop, match, commit, conn, nextId,
                    crashes, wipes, reconfs, applied, seen, initWiped, initCrashed, reinit,
                    wipedAfter, store>>

Quorum(S, M) == 2 * Cardinality(S \cap M) > Cardinality(M)

RecvRVR(m) ==
  LET n == m.to
      vs == votes[n] \cup (IF m.ok THEN {m.from} ELSE {})
      win == role[n] = "candidate" /\ m.term = term[n] /\ Quorum(vs, Mem(n))
  IN /\ msgs' = msgs \ {m}
     /\ IF m.term > term[n]
        THEN StepTo(n, m.term) /\ voted' = [voted EXCEPT ![n] = 0] /\ UNCHANGED votes
        ELSE /\ votes' = [votes EXCEPT ![n] = IF role[n] = "candidate" /\ m.term = term[n]
                                               THEN vs ELSE @]
             /\ role' = [role EXCEPT ![n] = IF win THEN "leader" ELSE @]
             /\ UNCHANGED <<term, voted>>
     /\ match' = IF m.term <= term[n] /\ win
                 THEN [match EXCEPT ![n] = [t \in Nodes |-> IF t = n THEN Len(log[n]) ELSE 0]]
                 ELSE match
     /\ cross' = Crossed(n, m)
     /\ UNCHANGED <<up, cfg, log, pledge, hello, prop, commit, conn, nextId, crashes, wipes,
                    reconfs, applied, seen, initWiped, initCrashed, reinit, wipedAfter, store>>

\* commit index c at node n: record the first committed Found
Apply(n, l, c) == applied' = applied \cup (IF FirstFound(l, c) /= 0 THEN {FirstFound(l, c)} ELSE {})

RecvAE(m) ==
  LET r == m.to
      nl == IF IsPrefix(m.lg, log[r]) THEN log[r] ELSE m.lg   \* stale: keep; else leader's
      nc == IF m.cm < Len(m.lg) THEN m.cm ELSE Len(m.lg)
      c1 == IF nl /= log[r] \/ nc > commit[r] THEN nc ELSE commit[r]
      reply == [ty |-> "aer", from |-> r, to |-> m.from, x |-> m.x, g |-> m.g,
                ep |-> Ep(r, m.from), term |-> m.term, mi |-> Len(m.lg)]
  IN IF m.term < term[r]
     THEN Discard(m)
     ELSE
     /\ term' = [term EXCEPT ![r] = m.term]
     /\ voted' = [voted EXCEPT ![r] = IF m.term > term[r] THEN 0 ELSE @]
     /\ role' = [role EXCEPT ![r] = "follower"]
     /\ log' = [log EXCEPT ![r] = nl]
     /\ commit' = [commit EXCEPT ![r] = c1]
     /\ Apply(r, nl, c1)
     /\ seen' = [seen EXCEPT ![r] = @ \cup (IF nl = << >> THEN {} ELSE {nl[1].g})]
     /\ cross' = Crossed(r, m)
     /\ LET out == (msgs \ {m}) \cup (IF CanReply(r, m.from) THEN {reply} ELSE {}) IN
        IF Strict /\ Gid(r) /= (IF nl = << >> THEN 0 ELSE nl[1].g)
        THEN \* the id changed: close every connection (the reply goes on a new one)
             /\ conn' = [k \in Pairs |-> IF r \in {k[1], k[2]} THEN [conn[k] EXCEPT !.open = FALSE]
                                         ELSE conn[k]]
             /\ msgs' = {x \in msgs \ {m} : r \notin {x.from, x.to}}
        ELSE msgs' = out /\ UNCHANGED conn
     /\ UNCHANGED <<up, cfg, pledge, votes, hello, prop, match, nextId, crashes, wipes,
                    reconfs, initWiped, initCrashed, reinit, wipedAfter, store>>

RecvAER(m) ==
  LET n == m.to
      mt == [match[n] EXCEPT ![m.from] = IF m.mi > @ THEN m.mi ELSE @]
      ok == {k \in 1..Len(log[n]) :
               log[n][k].t = term[n] /\ Quorum({t \in Nodes : mt[t] >= k}, Mem(n))}
      c1 == IF ok = {} THEN commit[n] ELSE LET k == CHOOSE k \in ok : \A j \in ok : j <= k IN
                                          IF k > commit[n] THEN k ELSE commit[n]
  IN /\ msgs' = msgs \ {m}
     /\ IF m.term > term[n]
        THEN /\ StepTo(n, m.term) /\ voted' = [voted EXCEPT ![n] = 0]
             /\ UNCHANGED <<match, commit, applied>>
        ELSE IF role[n] = "leader" /\ m.term = term[n]
        THEN /\ match' = [match EXCEPT ![n] = mt]
             /\ commit' = [commit EXCEPT ![n] = c1]
             /\ Apply(n, log[n], c1)
             /\ UNCHANGED <<term, voted, role>>
        ELSE UNCHANGED <<term, voted, role, match, commit, applied>>
     /\ cross' = Crossed(n, m)
     /\ UNCHANGED <<up, cfg, log, pledge, votes, hello, prop, conn, nextId, crashes, wipes,
                    reconfs, seen, initWiped, initCrashed, reinit, wipedAfter, store>>

\* a message that is not accepted (another id, a closed or newer connection,
\* a stale term) is dropped
Drop(m) == ~ Accepts(m.to, m) /\ Discard(m)

Recv(m) ==
  /\ up[m.to] /\ Accepts(m.to, m)
  /\ CASE m.ty = "rv"  -> RecvRV(m)
       [] m.ty = "rvr" -> RecvRVR(m)
       [] m.ty = "ae"  -> RecvAE(m)
       [] m.ty = "aer" -> RecvAER(m)
       [] m.ty = "data" -> /\ cross' = Crossed(m.to, m) /\ msgs' = msgs \ {m}
                           /\ UNCHANGED <<up, cfg, log, term, voted, pledge, role, votes, hello,
                                          prop, match, commit, conn, nextId, crashes, wipes,
                                          reconfs, applied, seen, initWiped, initCrashed,
                                          reinit, wipedAfter, store>>

\* the loop: a leader whose committed log has no Found appends one
AppendFound(n) ==
  /\ up[n] /\ role[n] = "leader" /\ ~ Founded(n) /\ Len(log[n]) < 3
  /\ ~ \E i \in 1..Len(log[n]) : log[n][i].k = "found" /\ log[n][i].t = term[n]
  /\ LET x == IF IdMode = "none" THEN nextId ELSE Id(n) IN
     /\ log' = [log EXCEPT ![n] = Append(@, [t |-> term[n], k |-> "found", g |-> Gid(n), x |-> x])]
     /\ nextId' = IF IdMode = "none" THEN nextId + 1 ELSE nextId
  /\ match' = [match EXCEPT ![n][n] = Len(log[n]) + 1]
  /\ UNCHANGED <<up, cfg, term, voted, pledge, role, votes, hello, prop, commit, msgs, conn,
                 crashes, wipes, reconfs, ghosts>>

\* partition-group and other cluster traffic between founded members
SendData(n, r) ==
  LET m == Msg("data", n, r, [term |-> 0]) IN
  /\ DataMsgs /\ up[n] /\ Founded(n) /\ r /= n /\ CanSend(n, r) /\ m \notin msgs
  /\ msgs' = msgs \cup {m}
  /\ conn' = ConnAfter(n, r)
  /\ UNCHANGED <<up, cfg, log, term, voted, pledge, role, votes, hello, prop, match, commit,
                 nextId, crashes, wipes, reconfs, ghosts>>

(*************************** failures **************************************)

\* n initialized its group and some other seed already holds the membership
Holders(n) == log[n] /= << >> /\ log[n][1].c = n /\ \E t \in Nodes \ {n} : Gid(t) = Gid(n)

Volatile(n) ==
  /\ role' = [role EXCEPT ![n] = "follower"]
  /\ votes' = [votes EXCEPT ![n] = {}]
  /\ hello' = [hello EXCEPT ![n] = [t \in Nodes |-> NoAns]]
  /\ prop' = [prop EXCEPT ![n] = 0]
  /\ match' = [match EXCEPT ![n] = [t \in Nodes |-> 0]]
  /\ commit' = [commit EXCEPT ![n] = 0]

\* crash and restart (one step), maybe with a new seed list
Crash(n, L) ==
  /\ crashes < MaxCrashes
  /\ L = cfg[n] \/ (L \in Recfg(n) /\ reconfs < MaxReconf)
  /\ crashes' = crashes + 1
  /\ reconfs' = IF L /= cfg[n] THEN reconfs + 1 ELSE reconfs
  /\ cfg' = [cfg EXCEPT ![n] = L]
  /\ Volatile(n)
  /\ IF Conn THEN CloseAll(n) ELSE UNCHANGED <<conn, msgs>>
  /\ initCrashed' = (initCrashed \/ (Holders(n) /\ applied = {}))
  /\ UNCHANGED <<up, log, term, voted, pledge, nextId, wipes, applied, seen, cross,
                 initWiped, reinit, wipedAfter, store>>

\* re-image: the disk is wiped and the node restarts
Wipe(n) ==
  /\ wipes < MaxWipes
  /\ wipes' = wipes + 1
  /\ log' = [log EXCEPT ![n] = << >>]
  /\ term' = [term EXCEPT ![n] = 0]
  /\ voted' = [voted EXCEPT ![n] = 0]
  /\ pledge' = [pledge EXCEPT ![n] = 0]
  /\ Volatile(n)
  /\ IF Conn THEN CloseAll(n) ELSE UNCHANGED <<conn, msgs>>
  /\ seen' = [seen EXCEPT ![n] = {}]
  /\ initWiped' = (initWiped \/ (Holders(n) /\ applied = {}))
  /\ wipedAfter' = IF (IF Claim THEN store.f ELSE applied /= {}) THEN wipedAfter \cup {n}
                  ELSE wipedAfter
  /\ UNCHANGED <<up, cfg, nextId, crashes, reconfs, applied, cross, initCrashed, reinit, store>>

\* the pledge times out (PledgeExpiry)
Expire(n) ==
  /\ Promise /\ PledgeExpiry /\ pledge[n] /= 0 /\ log[n] = << >>
  /\ pledge' = [pledge EXCEPT ![n] = 0]
  /\ UNCHANGED <<up, cfg, log, term, voted, role, votes, hello, prop, match, commit, msgs,
                 conn, nextId, crashes, wipes, reconfs, ghosts>>

Next ==
  \/ \E s, t \in Nodes : Hello(s, t) \/ SendRV(s, t) \/ SendAE(s, t)
                         \/ SendData(s, t)
  \/ \E n \in Nodes : Initialize(n) \/ Campaign(n) \/ AppendFound(n) \/ Wipe(n) \/ Expire(n)
                      \/ Adopt(n) \/ MarkFounded(n)
  \/ \E n \in Nodes : \E L \in {cfg[n]} \cup Recfg(n) : Crash(n, L)
  \/ \E m \in msgs : Recv(m) \/ Drop(m)

Spec == Init /\ [][Next]_vars

\* state constraint: at most MaxMsgs messages in flight
Bound == Cardinality(msgs) <= MaxMsgs

(*************************** properties ************************************)

\* 1. Never two clusters from overlapping seed lists: at most one cluster id
\*    is ever applied (every scenario's lists overlap).
OneCluster == Cardinality(applied) <= 1
\* No node's controller log ever mixes two groups (since its last wipe).
NoMix == \A n \in Nodes : Cardinality(seen[n]) <= 1
\* No node with a group ever accepts a message from another group's member.
NoCross == ~ cross
\* 3. No group is initialized after a cluster was founded.
NoReinit == ~ reinit
\* 3'. A seed wiped after founding stays out (until join exists).
WipedStaysOut == \A n \in wipedAfter : log[n] = << >>

\* 2. witnesses (expected to be violated: the paths are reachable)
NoFoundAfterInitWipe == ~ (initWiped /\ applied /= {})
NoFoundAfterInitCrash == ~ (initCrashed /\ applied /= {})
NeverFounded == applied = {}
=============================================================================
