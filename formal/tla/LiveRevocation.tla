---------------------------- MODULE LiveRevocation ----------------------------
(***************************************************************************)
(* Shep §13.2 phase 2, §6.3 actors, viewers and act-for keys, §6.5, §16.4. *)
(*                                                                         *)
(* One topic: the live query in_channel(c), on one PG node, so a version   *)
(* is a log prefix length. Message.read = delegate(node.channel), and      *)
(* Channel.read = viewer in members. Subscribers: users u1, u2 (each its   *)
(* own actor and viewer, from an SSO token) and agent g1, whose            *)
(* subscription runs as the pair (actor g1, viewer u1) fixed by its        *)
(* act-for key (the credential model, §6.3). A deleted actor or viewer, or *)
(* a revoked key, is denied before any policy runs, but those checks sit   *)
(* outside the policy: can_op itself still admits a deleted user whose     *)
(* membership edge is in place (deletion is soft, edges stay).             *)
(*                                                                         *)
(* Log entries: post a message; add/remove a member; delete/restore an     *)
(* actor; revoke g1's act-for key (in sys; folded into the one log, as     *)
(* front ends tail sys); deploy a policy (P1 = the rules above; P2 adds    *)
(* `deny if actor is agents.Agent`).                                       *)
(*                                                                         *)
(* The topic follows §13.2. A step goes from V_topic to a snapshot S       *)
(* (possibly several entries at once). Phase 1: Δcore = messages added in  *)
(* (V_topic, S]. Phase 2 keeps the anchor's audience intersected with the  *)
(* subscribers (`aud`); a new message is pushed to the subscribers in it   *)
(* (its residual is empty). Permission facts are routed: a subscriber is   *)
(* re-evaluated at S only when a routed fact concerns it:                  *)
(*   - a membership edge of c (c is in the reference set) whose member is  *)
(*     the subscriber's viewer (the batched lookup over viewers);          *)
(*   - @deleted_at on its own actor node or its viewer node (RouteDeleted);*)
(*     then, if that node is deleted at S, the stream closes with `denied` *)
(*     (for the viewer node, if CloseOnMapped);                            *)
(*   - the revocation of the key it was opened with (RouteRevoke); the     *)
(*     stream closes with `denied`;                                        *)
(*   - a policy deploy (every subscriber).                                 *)
(* A re-evaluated subscriber whose access to the anchor changed gets a     *)
(* resync (to the empty view, or to the full view). Every update carries   *)
(* S. A subscription joins through the topic's loop, at V_topic, when the  *)
(* topic has caught up with the log. Every message pushed mints a link     *)
(* for that subscriber; a link is checked when used.                       *)
(*                                                                         *)
(* Knobs (design value first):                                             *)
(*  DenyDeletedViewer TRUE: a request whose viewer is deleted is `denied`  *)
(*      (§6.3). FALSE: the superseded mapping model, where it had no       *)
(*      viewer and was judged by the policy.                               *)
(*  RouteDeleted TRUE: route @deleted_at of the subscriber's actor and     *)
(*      viewer nodes.                                                      *)
(*  RouteRevoke TRUE: route the revocation of the subscription's key.      *)
(*  CloseOnMapped TRUE: deleting the subscriber's viewer closes the        *)
(*      stream with `denied`; FALSE: it is a Δcan_read like any other.     *)
(*  LinkDeletedCheck TRUE: link use checks that the minting key is valid   *)
(*      and that neither the actor nor the viewer is deleted, as well as   *)
(*      can_op.                                                            *)
(*                                                                         *)
(* Superseded: the mapping model (§6.3 before the credential change). g1's *)
(* viewer was its `acts_for` target, which a "remap" entry re-points. Only *)
(* the superseded configs put "remap" in Ops; with it,                     *)
(*  RouteActsFor TRUE routes changes to g1's acts_for edge, and            *)
(*  MapRouteAt "step" takes the viewer node from the step's snapshot S,    *)
(*      "subscribe" from subscribe time.                                   *)
(* Without "remap", g1's viewer is the constant u1 and these are inert.    *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS MaxEntries, MaxMsgs, Ops,
          RouteDeleted, RouteActsFor, MapRouteAt, CloseOnMapped, LinkDeletedCheck,
          DenyDeletedViewer, RouteRevoke

Users  == {"u1", "u2"}
Agents == {"g1"}
Actors == Users \cup Agents
MsgIds == {"m0", "m1", "m2"}
NoV == "nobody"

\* the graph and registry state after a log prefix
\* owner: g1's viewer (constant unless the superseded "remap" is in Ops);
\* rev: actors whose act-for key is revoked
St0 == [mem |-> {"u1"}, del |-> {}, owner |-> "u1", pol |-> "P1", msgs |-> {"m0"},
        rev |-> {}]

VARIABLES
  log,      \* sequence of entries [k, x]
  hist,     \* hist[v + 1] = the state at version v (hist[1] = St0)
  Vt,       \* the topic's version
  sub,      \* sub[a] \in {"none", "open", "closed"}
  mapAt,    \* g1's viewer when it subscribed (superseded MapRouteAt)
  aud,      \* phase 2's cached anchor audience, intersected with subscribers
  cv,       \* cv[a]: the subscriber's client view
  links,    \* minted links <<a, m>>
  pushOK,   \* history: no push at S reveals a message unreadable at S, and
            \*   none goes to an actor deleted at S                   (inv 1)
  exactOK,  \* history: every push leaves the client = fresh eval at S (inv 3)
  closeOK,  \* history: a stream closes with denied only when a fresh
            \*   request at that version would be denied
  linkOK    \* history: a link works only for a request that isn't denied
            \*   and can read (inv 2)

vars == <<log, hist, Vt, sub, mapAt, aud, cv, links, pushOK, exactOK, closeOK, linkOK>>

Cur == hist[Len(log) + 1]
At(v) == hist[v + 1]

Viewer(a, s) == IF a \in Users THEN a
                ELSE IF s.owner \in s.del THEN NoV ELSE s.owner
\* can_op(read, a, m) for a message of c: the compiled policy over the
\* viewer; the pre-checks (deleted actor or viewer, revoked key) are not
\* part of it
CanOp(a, s) == /\ Viewer(a, s) /= NoV
               /\ Viewer(a, s) \in s.mem
               /\ (s.pol = "P2" => a \notin Agents)
\* a fresh evaluation of the query as a, at state s, is `denied` when a is
\* deleted, its viewer is deleted, or its key is revoked (Denied), and
\* otherwise returns FreshView
Denied(a, s) == \/ a \in s.del
                \/ DenyDeletedViewer /\ a \in Agents /\ s.owner \in s.del
                \/ a \in s.rev
FreshView(a, s) == IF CanOp(a, s) THEN s.msgs ELSE {}

Init ==
  /\ log = << >>
  /\ hist = << St0 >>
  /\ Vt = 0
  /\ sub = [a \in Actors |-> "none"]
  /\ mapAt = "u1"
  /\ aud = {}
  /\ cv = [a \in Actors |-> {}]
  /\ links = {}
  /\ pushOK = TRUE /\ exactOK = TRUE /\ closeOK = TRUE /\ linkOK = TRUE

(*************************** writes **************************************)

Apply(s, e) ==
  CASE e.k = "post"    -> [s EXCEPT !.msgs = @ \cup {e.x}]
    [] e.k = "add"     -> [s EXCEPT !.mem = @ \cup {e.x}]
    [] e.k = "rm"      -> [s EXCEPT !.mem = @ \ {e.x}]
    [] e.k = "del"     -> [s EXCEPT !.del = @ \cup {e.x}]
    [] e.k = "restore" -> [s EXCEPT !.del = @ \ {e.x}]
    [] e.k = "remap"   -> [s EXCEPT !.owner = e.x]
    [] e.k = "deploy"  -> [s EXCEPT !.pol = e.x]
    [] e.k = "revoke"  -> [s EXCEPT !.rev = @ \cup {e.x}]

Possible(s) ==
     {[k |-> "post", x |-> m] : m \in {m \in MsgIds : m \notin s.msgs /\ Cardinality(s.msgs) <= MaxMsgs}}
  \cup {[k |-> "add", x |-> u] : u \in Users \ s.mem}
  \cup {[k |-> "rm", x |-> u] : u \in s.mem}
  \cup {[k |-> "del", x |-> a] : a \in Actors \ s.del}
  \cup {[k |-> "restore", x |-> a] : a \in s.del}
  \cup {[k |-> "remap", x |-> u] : u \in Users \ {s.owner}}
  \cup {[k |-> "deploy", x |-> p] : p \in {"P1", "P2"} \ {s.pol}}
  \cup {[k |-> "revoke", x |-> g] : g \in Agents \ s.rev}

\* the next message id is fixed, so posts don't branch on names
NextMsg(s) == IF "m1" \in s.msgs THEN "m2" ELSE "m1"

Write(e) ==
  /\ Len(log) < MaxEntries
  /\ e.k \in Ops
  /\ e \in Possible(Cur)
  /\ e.k = "post" => e.x = NextMsg(Cur)
  /\ log' = Append(log, e)
  /\ hist' = Append(hist, Apply(Cur, e))
  /\ UNCHANGED <<Vt, sub, mapAt, aud, cv, links, pushOK, exactOK, closeOK, linkOK>>

(*************************** subscribe ************************************)

\* joins through the topic's loop, once it has caught up; the request is
\* denied if the actor or viewer is deleted or the key revoked
Subscribe(a) ==
  /\ sub[a] /= "open"
  /\ Vt = Len(log)
  /\ ~Denied(a, Cur)
  /\ sub' = [sub EXCEPT ![a] = "open"]
  /\ cv' = [cv EXCEPT ![a] = FreshView(a, Cur)]
  /\ aud' = IF CanOp(a, Cur) THEN aud \cup {a} ELSE aud \ {a}
  /\ mapAt' = IF a \in Agents THEN Cur.owner ELSE mapAt
  /\ links' = links \cup {<<a, m>> : m \in FreshView(a, Cur)}
  /\ UNCHANGED <<log, hist, Vt, pushOK, exactOK, closeOK, linkOK>>

(*************************** a topic step to S ****************************)

Entries(lo, hi) == {log[i] : i \in (lo + 1)..hi}

\* a's viewer node, when it isn't a itself
Mapped(a, s) == IF a \notin Agents THEN {}
                ELSE IF MapRouteAt = "step" THEN {s.owner} ELSE {mapAt}

Step(S) ==
  LET old == At(Vt)
      new == At(S)
      E == Entries(Vt, S)
      open == {a \in Actors : sub[a] = "open"}
      routedDel(a) == RouteDeleted /\
                        \E e \in E : e.k \in {"del", "restore"} /\ e.x \in {a} \cup Mapped(a, new)
      routedRemap(a) == RouteActsFor /\ a \in Agents /\ \E e \in E : e.k = "remap"
      routedRev(a) == RouteRevoke /\ \E e \in E : e.k = "revoke" /\ e.x = a
      routedMem(a) == \E e \in E : e.k \in {"add", "rm"} /\ e.x = Viewer(a, new)
      routedDeploy == \E e \in E : e.k = "deploy"
      affected(a) == routedDel(a) \/ routedRemap(a) \/ routedMem(a) \/ routedDeploy
      closes(a) == \/ routedDel(a) /\
                       (a \in new.del \/ (CloseOnMapped /\ Mapped(a, new) \cap new.del /= {}))
                   \/ routedRev(a) /\ a \in new.rev
      closing == {a \in open : closes(a)}
      stay == open \ closing
      inAud(a) == IF affected(a) THEN CanOp(a, new) ELSE a \in aud
      resync == {a \in stay : affected(a) /\ inAud(a) /= (a \in aud)}
      dcore == new.msgs \ old.msgs
      newCv(a) == IF a \in resync THEN (IF inAud(a) THEN new.msgs ELSE {})
                  ELSE IF inAud(a) THEN cv[a] \cup dcore ELSE cv[a]
      pushed(a) == IF a \in resync THEN newCv(a) ELSE IF inAud(a) THEN dcore ELSE {}
      rcpt == {a \in stay : a \in resync \/ pushed(a) /= {}}
  IN
  /\ Vt < S /\ S <= Len(log)
  /\ Vt' = S
  /\ sub' = [a \in Actors |-> IF a \in closing THEN "closed" ELSE sub[a]]
  /\ aud' = {a \in stay : inAud(a)}
  /\ cv' = [a \in Actors |-> IF a \in closing THEN {}
                             ELSE IF a \in stay THEN newCv(a) ELSE cv[a]]
  /\ links' = links \cup UNION {{<<a, m>> : m \in pushed(a)} : a \in rcpt}
  /\ pushOK' = (pushOK /\ \A a \in rcpt :
                   ~Denied(a, new) /\ (newCv(a) /= {} => CanOp(a, new)))
  /\ exactOK' = (exactOK /\ \A a \in rcpt : ~ Denied(a, new) /\ newCv(a) = FreshView(a, new))
  /\ closeOK' = (closeOK /\ \A a \in closing : Denied(a, new))
  /\ UNCHANGED <<log, hist, mapAt, linkOK>>

(*************************** links *****************************************)

\* using a link: verified, then checked against the current state (the
\* minting key, the actor's and viewer's deletion, then can_op)
UseLink(a, m) ==
  /\ <<a, m>> \in links
  /\ LinkDeletedCheck => ~Denied(a, Cur)
  /\ CanOp(a, Cur) /\ m \in Cur.msgs
  /\ linkOK' = (linkOK /\ ~Denied(a, Cur) /\ CanOp(a, Cur))
  /\ UNCHANGED <<log, hist, Vt, sub, mapAt, aud, cv, links, pushOK, exactOK, closeOK>>

WriteAny == \E e \in Possible(Cur) : Write(e)
StepAny == \E S \in (Vt + 1)..Len(log) : Step(S)

Next ==
  \/ WriteAny
  \/ \E a \in Actors : Subscribe(a)
  \/ StepAny
  \/ \E a \in Actors, m \in MsgIds : UseLink(a, m)

Spec == Init /\ [][Next]_vars

(*************************** properties ***********************************)

\* 1. After a revocation commits at R, no update pushed at S >= R reveals a
\*    message the subscriber can't read at S, and a request that would be
\*    denied at S (deleted actor or viewer, revoked key) gets no push (its
\*    stream was closed first).
PushSafe == pushOK
\* 1'. The same for the view every open stream asserts at V_topic, whether
\*     or not this step pushed to it ("no update" means "no change").
ViewSafe == \A a \in Actors : sub[a] = "open" =>
              /\ ~Denied(a, At(Vt))
              /\ (cv[a] /= {} => CanOp(a, At(Vt)))
\* 2. A link used after revocation (or deletion of its actor or viewer) is
\*    refused.
LinkSafe == linkOK
\* 3. Differential (milestone 9): at every pushed version, and at every
\*    version the topic reaches, an open subscriber's view equals a fresh
\*    evaluation of the query as that actor at that version.
PushExact == exactOK
ViewExact == \A a \in Actors : sub[a] = "open" =>
               ~ Denied(a, At(Vt)) /\ cv[a] = FreshView(a, At(Vt))
\* A stream closes with `denied` only when a request would be denied.
CloseOnlyIfDenied == closeOK

Inv == PushSafe /\ ViewSafe /\ LinkSafe /\ PushExact /\ ViewExact
=============================================================================
