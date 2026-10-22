------------------------------- MODULE SlackRT -------------------------------
(***************************************************************************)
(* Shep §13.2: the Slack clone's per-channel task.                         *)
(*                                                                         *)
(* One channel, on one PG node. Base data: the channel's member set        *)
(* (changed by an out-of-band admin path) and its messages. A message's    *)
(* read policy is "author may read; otherwise delegate(channel)", so its   *)
(* anchored audience is A_c with residual.add = {author}, remove = {}.     *)
(*                                                                         *)
(* A task holds A_c (a Z-set over users) and V_c. Posting: the task        *)
(* mutates with since: V_c and observe: audience(...).anchor(channel); the *)
(* response gives C and dA = A(C) - A(V_c) (computed exactly, as §11.6     *)
(* promises). The task then sets A_c += dA, V_c = C, drops listeners that  *)
(* left A_c, and pushes the message (with token C) to the listeners in     *)
(* A_c + residual. Between writes it polls changes(audience, since V_c).   *)
(* Startup/failover: read A_c with a plain query. Client resume: the task  *)
(* answers changes(messages, since: token) filtered to the user's current  *)
(* visibility; a client without a token cold-loads.                        *)
(*                                                                         *)
(* Knobs:                                                                  *)
(*  PollMode = "concurrent": a poll may be in flight while a post is (the  *)
(*     design only says the task issues one MUTATION at a time).           *)
(*  PollMode = "serialized": proposed fix: polls and posts on a channel    *)
(*     are serialized, so every response's since equals the current V_c.   *)
(*  ExclusiveOwner = FALSE: a new owner can start while the old one is     *)
(*     still running (consistent-hashing handover has no fencing).         *)
(*  PushAll = FALSE: the task pushes only the message it posted.           *)
(*  PushAll = TRUE : proposed fix: posts and polls also observe            *)
(*     changes(messages in channel) since V_c, and the task pushes every   *)
(*     message in that delta to the listeners that can read it at C.       *)
(*  SerialConnect = FALSE: a client connects by loading at a fresh         *)
(*     snapshot and attaching to the task, independently of V_c.           *)
(*  SerialConnect = TRUE: proposed fix: connects go through the task's     *)
(*     serialized loop; the task first advances (A_c, V_c) to the          *)
(*     connect's snapshot W, then attaches the listener at W.              *)
(*  TokenFromAnyResponse = TRUE: the client's "last token it received"     *)
(*     may come from any Shep response (its Db handle), not only from the  *)
(*     channel's pushes.                                                   *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS Users, Tasks, InitMem, MaxMsgs, MaxAdmin, MaxPolls,
          PollMode, ExclusiveOwner, PushAll, TokenFromAnyResponse, SerialConnect

MaxD == MaxMsgs + MaxAdmin
Dots == 1..MaxD
NoTask == "none"
NoUser == "nobody"

VARIABLES
  ctr, ent,            \* the log: ent[d] = [st, kind, u]
  nmsg, nadm, npoll,   \* bounds
  tst, A, V,           \* tasks: state, audience Z-set, version
  post,                \* in-flight post per task
  poll,                \* in-flight poll per task
  lis,                 \* lis[u]: task the user's client is connected to
  seen, ctok, hasTok,  \* client caches: messages, last token, has a token
  safeOK,              \* history: every push was readable at its version
  leakOK               \* history: every push was readable at its version
                       \*   or at least in the current state (no real leak)

vars == <<ctr, ent, nmsg, nadm, npoll, tst, A, V, post, poll, lis, seen, ctok, hasTok, safeOK, leakOK>>

NoPost == [st |-> "none", d |-> 0, since |-> {}, snap |-> {}, C |-> {}]
NoPoll == [st |-> "none", since |-> {}, Vp |-> {}]

Init ==
  /\ ctr = 0
  /\ ent = [d \in Dots |-> [st |-> "none", kind |-> "msg", u |-> NoUser]]
  /\ nmsg = 0 /\ nadm = 0 /\ npoll = 0
  /\ tst = [k \in Tasks |-> "off"]
  /\ A = [k \in Tasks |-> [u \in Users |-> 0]]
  /\ V = [k \in Tasks |-> {}]
  /\ post = [k \in Tasks |-> NoPost]
  /\ poll = [k \in Tasks |-> NoPoll]
  /\ lis = [u \in Users |-> NoTask]
  /\ seen = [u \in Users |-> {}]
  /\ ctok = [u \in Users |-> {}]
  /\ hasTok = [u \in Users |-> FALSE]
  /\ safeOK = TRUE
  /\ leakOK = TRUE

Committed == {d \in Dots : ent[d].st = "committed"}
Msgs(S) == {d \in S : ent[d].kind = "msg"}
Author(m) == ent[m].u

Count(S) == Cardinality(S)
\* membership weight of u in the state at version S
MemW(u, S) == (IF u \in InitMem THEN 1 ELSE 0)
              + Count({d \in S : ent[d].kind = "add" /\ ent[d].u = u})
              - Count({d \in S : ent[d].kind = "rm" /\ ent[d].u = u})
MemZ(S) == [u \in Users |-> MemW(u, S)]
Members(S) == {u \in Users : MemW(u, S) > 0}
Pos(Z) == {u \in Users : Z[u] > 0}
ZAdd(a, b) == [u \in Users |-> a[u] + b[u]]
ZSub(a, b) == [u \in Users |-> a[u] - b[u]]

\* the message read policy, evaluated at version S
CanRead(u, m, S) == u \in Members(S) \/ u = Author(m)

Listeners(k) == {u \in Users : lis[u] = k}

(*************************** out-of-band admin path ************************)

Admin(u, kind) ==
  /\ nadm < MaxAdmin /\ ctr < MaxD
  /\ IF kind = "add" THEN u \notin Members(Committed) ELSE u \in Members(Committed)
  /\ ctr' = ctr + 1
  /\ ent' = [ent EXCEPT ![ctr + 1] = [st |-> "committed", kind |-> kind, u |-> u]]
  /\ nadm' = nadm + 1
  /\ UNCHANGED <<nmsg, npoll, tst, A, V, post, poll, lis, seen, ctok, hasTok, safeOK, leakOK>>

(*************************** tasks *****************************************)

StartTask(k) ==
  /\ tst[k] = "off"
  /\ ExclusiveOwner => \A j \in Tasks : tst[j] /= "on"
  /\ tst' = [tst EXCEPT ![k] = "on"]
  /\ A' = [A EXCEPT ![k] = MemZ(Committed)]          \* plain query
  /\ V' = [V EXCEPT ![k] = Committed]                 \* its token
  /\ UNCHANGED <<ctr, ent, nmsg, nadm, npoll, post, poll, lis, seen, ctok, hasTok, safeOK, leakOK>>

KillTask(k) ==
  /\ tst[k] = "on"
  /\ tst' = [tst EXCEPT ![k] = "dead"]
  /\ lis' = [u \in Users |-> IF lis[u] = k THEN NoTask ELSE lis[u]]
  /\ post' = [post EXCEPT ![k] = NoPost]
  /\ poll' = [poll EXCEPT ![k] = NoPoll]
  /\ UNCHANGED <<ctr, ent, nmsg, nadm, npoll, A, V, seen, ctok, hasTok, safeOK, leakOK>>

Idle(k) == post[k].st = "none" /\ (PollMode = "serialized" => poll[k].st = "none")

\* a listener of k posts through k: snapshot taken, dot allocated, in flight
PostBegin(k, u) ==
  /\ tst[k] = "on" /\ lis[u] = k /\ Idle(k)
  /\ nmsg < MaxMsgs /\ ctr < MaxD
  /\ ctr' = ctr + 1
  /\ ent' = [ent EXCEPT ![ctr + 1] = [st |-> "pending", kind |-> "msg", u |-> u]]
  /\ nmsg' = nmsg + 1
  /\ post' = [post EXCEPT ![k] = [st |-> "inflight", d |-> ctr + 1, since |-> V[k],
                                  snap |-> Committed, C |-> {}]]
  /\ UNCHANGED <<nadm, npoll, tst, A, V, poll, lis, seen, ctok, hasTok, safeOK, leakOK>>

PostCommit(k) ==
  LET p == post[k] IN
  /\ p.st = "inflight"
  /\ ent' = [ent EXCEPT ![p.d].st = "committed"]
  /\ post' = [post EXCEPT ![k].st = "resp", ![k].C = p.snap \cup p.since \cup {p.d}]
  /\ UNCHANGED <<ctr, nmsg, nadm, npoll, tst, A, V, poll, lis, seen, ctok, hasTok, safeOK, leakOK>>

(* The task advances (A_c, V_c) from `since` to W using an exact delta,    *)
(* drops listeners that left, and pushes the messages in `toPush` to the   *)
(* listeners that may read them, with token W.                            *)
Advance(k, since, W, toPush) ==
  LET A2 == ZAdd(A[k], ZSub(MemZ(W), MemZ(since)))
      kept == {u \in Listeners(k) : u \in Pos(A2)}
      gets(u) == {m \in toPush : u \in Pos(A2) \/ u = Author(m)}
      rcpt == {u \in kept : gets(u) /= {}}
  IN /\ A' = [A EXCEPT ![k] = A2]
     /\ V' = [V EXCEPT ![k] = W]
     /\ lis' = [u \in Users |-> IF lis[u] = k /\ u \notin kept THEN NoTask ELSE lis[u]]
     /\ seen' = [u \in Users |-> IF u \in rcpt THEN seen[u] \cup gets(u) ELSE seen[u]]
     /\ ctok' = [u \in Users |-> IF u \in rcpt THEN W ELSE ctok[u]]
     /\ hasTok' = [u \in Users |-> IF u \in rcpt THEN TRUE ELSE hasTok[u]]
     /\ safeOK' = (safeOK /\ \A u \in rcpt : \A m \in gets(u) : CanRead(u, m, W))
     /\ leakOK' = (leakOK /\ \A u \in rcpt : \A m \in gets(u) :
                               CanRead(u, m, W) \/ CanRead(u, m, Committed))

TaskHandlePost(k) ==
  LET p == post[k] IN
  /\ tst[k] = "on" /\ p.st = "resp"
  /\ Advance(k, p.since, p.C,
             IF PushAll THEN Msgs(p.C) \ Msgs(p.since) ELSE {p.d})
  /\ post' = [post EXCEPT ![k] = NoPost]
  /\ UNCHANGED <<ctr, ent, nmsg, nadm, npoll, tst, poll>>

\* between writes: changes(audience, since V_c) (and, with PushAll, messages)
PollIssue(k) ==
  /\ tst[k] = "on" /\ poll[k].st = "none"
  /\ PollMode = "serialized" => post[k].st = "none"
  /\ npoll < MaxPolls
  /\ npoll' = npoll + 1
  /\ poll' = [poll EXCEPT ![k] = [st |-> "resp", since |-> V[k], Vp |-> Committed]]
  /\ UNCHANGED <<ctr, ent, nmsg, nadm, tst, A, V, post, lis, seen, ctok, hasTok, safeOK, leakOK>>

TaskHandlePoll(k) ==
  LET q == poll[k] IN
  /\ tst[k] = "on" /\ q.st = "resp"
  /\ Advance(k, q.since, q.Vp,
             IF PushAll THEN Msgs(q.Vp) \ Msgs(q.since) ELSE {})
  /\ poll' = [poll EXCEPT ![k] = NoPoll]
  /\ UNCHANGED <<ctr, ent, nmsg, nadm, npoll, tst, post>>

(*************************** clients ***************************************)

\* connect to a running task that has the user in its audience; cold load
\* or resume (changes(messages, since token) filtered to current visibility)
Connect(u, k) ==
  /\ ~ SerialConnect
  /\ lis[u] = NoTask /\ tst[k] = "on" /\ u \in Pos(A[k])
  /\ LET W == Committed
         new == {m \in Msgs(W) : CanRead(u, m, W)
                                 /\ (hasTok[u] => m \notin ctok[u])}
     IN /\ seen' = [seen EXCEPT ![u] = seen[u] \cup new]
        /\ ctok' = [ctok EXCEPT ![u] = W]
  /\ hasTok' = [hasTok EXCEPT ![u] = TRUE]
  /\ lis' = [lis EXCEPT ![u] = k]
  /\ UNCHANGED <<ctr, ent, nmsg, nadm, npoll, tst, A, V, post, poll, safeOK, leakOK>>

\* proposed fix: the connect is a step of the task's serialized loop
ConnectSerial(u, k) ==
  /\ SerialConnect
  /\ lis[u] = NoTask /\ tst[k] = "on"
  /\ post[k].st = "none" /\ poll[k].st = "none"
  /\ LET W == Committed
         A2 == ZAdd(A[k], ZSub(MemZ(W), MemZ(V[k])))
         toPush == IF PushAll THEN Msgs(W) \ Msgs(V[k]) ELSE {}
         kept == {x \in Listeners(k) : x \in Pos(A2)}
         gets(x) == {m \in toPush : x \in Pos(A2) \/ x = Author(m)}
         rcpt == {x \in kept : gets(x) /= {}}
         new == {m \in Msgs(W) : CanRead(u, m, W) /\ (hasTok[u] => m \notin ctok[u])}
     IN /\ u \in Pos(A2)
        /\ A' = [A EXCEPT ![k] = A2]
        /\ V' = [V EXCEPT ![k] = W]
        /\ lis' = [x \in Users |-> IF x = u THEN k
                                   ELSE IF lis[x] = k /\ x \notin kept THEN NoTask
                                   ELSE lis[x]]
        /\ seen' = [x \in Users |-> IF x = u THEN seen[u] \cup new
                                    ELSE IF x \in rcpt THEN seen[x] \cup gets(x)
                                    ELSE seen[x]]
        /\ ctok' = [x \in Users |-> IF x = u \/ x \in rcpt THEN W ELSE ctok[x]]
        /\ hasTok' = [x \in Users |-> IF x = u \/ x \in rcpt THEN TRUE ELSE hasTok[x]]
        /\ safeOK' = (safeOK /\ \A x \in rcpt : \A m \in gets(x) : CanRead(x, m, W))
        /\ leakOK' = (leakOK /\ \A x \in rcpt : \A m \in gets(x) : CanRead(x, m, W))
  /\ UNCHANGED <<ctr, ent, nmsg, nadm, npoll, tst, post, poll>>

Disconnect(u) ==
  /\ lis[u] /= NoTask
  /\ lis' = [lis EXCEPT ![u] = NoTask]
  /\ UNCHANGED <<ctr, ent, nmsg, nadm, npoll, tst, A, V, post, poll, seen, ctok, hasTok, safeOK, leakOK>>

\* the client's last token comes from some other Shep response
OtherResponse(u) ==
  /\ TokenFromAnyResponse
  /\ lis[u] = NoTask /\ hasTok[u]
  /\ ctok' = [ctok EXCEPT ![u] = Committed]
  /\ UNCHANGED <<ctr, ent, nmsg, nadm, npoll, tst, A, V, post, poll, lis, seen, hasTok, safeOK, leakOK>>

Next ==
  \/ \E u \in Users : Admin(u, "add") \/ Admin(u, "rm")
  \/ \E k \in Tasks : StartTask(k) \/ KillTask(k) \/ PostCommit(k)
                      \/ TaskHandlePost(k) \/ PollIssue(k) \/ TaskHandlePoll(k)
  \/ \E k \in Tasks, u \in Users : PostBegin(k, u) \/ Connect(u, k) \/ ConnectSerial(u, k)
  \/ \E u \in Users : Disconnect(u) \/ OtherResponse(u)

Spec == Init /\ [][Next]_vars

(*************************** properties ************************************)

\* §13.2's claim: every delivery was readable by its recipient at the
\* version it was delivered at (for the task's own message: at its commit)
PushSafe == safeOK

\* weaker: never push a message to someone who cannot read it at the push
\* version AND cannot read it now either (an outright leak)
NoLeak == leakOK

\* the task's audience equals the channel's audience at V_c (chaining)
AudienceExact == \A k \in Tasks : tst[k] = "on" => A[k] = MemZ(V[k])

\* a connected client holds every message its token says it has and that
\* it may read at that token: resuming from the token loses nothing
CacheComplete ==
  \A u \in Users : lis[u] /= NoTask =>
     {m \in Msgs(ctok[u]) : CanRead(u, m, ctok[u])} \subseteq seen[u]

UserSym == Permutations(Users)
TaskSym == Permutations(Tasks)
=============================================================================
