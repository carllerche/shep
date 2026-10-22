------------------------- MODULE ControllerReconcile -------------------------
(***************************************************************************)
(* Shep design §9.6 (the WAL controller, its reconcilers, moves and        *)
(* splits as start/finish command pairs), after Redpanda's                 *)
(* controller_backend (cluster/controller_backend.cc, topic_table.cc,      *)
(* controller_utils.cc: log_revision_on_node, shard_placement_table.cc:    *)
(* get_reconciliation_action).                                             *)
(*                                                                         *)
(* The controller log holds commands; a command's offset is its revision.  *)
(* The partition table is a fold of the log. Per group it holds: whether   *)
(* it exists, its incarnation (the revision of the command that created    *)
(* it), its replica set and each replica's log revision (the revision at  *)
(* which that node joined), and an in-progress update (state in_progress   *)
(* or cancelled, the previous and target replica sets, its revision).      *)
(* Like Redpanda's topic_table: a move sets the replicas to the target; a  *)
(* cancel sets them back to the previous set; revert_cancel sets them to   *)
(* the target again; finish ends the update.                               *)
(*                                                                         *)
(* A node's expected log revision for a group (log_revision_on_node): its  *)
(* revision if it is in the original replicas (the previous set during an  *)
(* update), the update's revision if it is in the target, else none. Each  *)
(* node applies the log in order and reconciles each group at its applied  *)
(* revision (get_reconciliation_action): delete a local replica that isn't *)
(* expected, or whose log revision is older than expected (RevCheck);      *)
(* create a missing one.                                                   *)
(*                                                                         *)
(* Raft, per group: a configuration, simple or joint, and each replica's   *)
(* data (acknowledged writes). The leader (a live member) drives the      *)
(* update as reconcile_partition_reconfiguration does: enter the joint     *)
(* configuration, leave it for the target, then dispatch finish; on a      *)
(* cancel, leave the joint configuration back to the previous set; if raft *)
(* has already moved, Redpanda dispatches revert_cancel (CancelMode =      *)
(* "revert"), where "back" instead reconfigures to the previous set.       *)
(*                                                                         *)
(* Splits: start_split creates the child C; P's leader takes the split     *)
(* entry (a snapshot of P's data; P then refuses writes); C's first entry  *)
(* installs it; finish_split removes P. Moves of P interleave freely.     *)
(*                                                                         *)
(* Writes name the group and incarnation from the client's map (the      *)
(* table at the latest revision); a replica takes a write only for its     *)
(* incarnation (RevCheck), from a leader with a quorum of each half of its *)
(* configuration.                                                          *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  Nodes,           \* WAL nodes
  Writes,          \* write ids
  MaxCmds,         \* bound on admin commands (create, delete, move, cancel, split)
  WithSplit,       \* allow a split of P into C
  WithRecreate,    \* allow deleting and recreating P
  CancelMode,      \* "back" or "revert" (Redpanda's revert_cancel)
  DeleteOnMove,    \* TRUE: old replicas are deleted when the move command is
                   \*   applied, not at finish
  CancelAfterFinish, \* TRUE: a cancel applies after finish (to the last move)
  RevCheck,        \* TRUE: replicas are checked by log revision
  RaftRevCheck     \* TRUE (Redpanda): a raft configuration carries the
                   \*   revision of the command it applies; raft refuses an
                   \*   older one, and finish needs the configuration at the
                   \*   update's last command revision

Groups == IF WithSplit THEN {"P", "C"} ELSE {"P"}
NoUpd == "none"

GS0 == [ex |-> FALSE, inc |-> 0, reps |-> {}, rev |-> [n \in Nodes |-> 0],
        ust |-> NoUpd, prev |-> {}, tgt |-> {}, urev |-> 0,
        canc |-> FALSE, lastPrev |-> {}, lastRev |-> [n \in Nodes |-> 0], bad |-> FALSE, lcr |-> 0]

VARIABLES
  clog,    \* controller log: sequence of [k, g, set]
  admin,   \* admin commands issued
  nrev,    \* node -> applied revision
  loc,     \* node -> group -> log revision of the local replica (0: none)
  dat,     \* node -> group -> writes it holds
  cfg,     \* group -> raft configuration [inc, kind, old, new]
  acked,   \* write -> <<group, incarnation>> or <<"-", 0>>
  psnap,   \* P's split entry: its data, or {"none"}
  cstart   \* C's first entry committed

vars == <<clog, admin, nrev, loc, dat, cfg, acked, psnap, cstart>>

(*************************** the table: a fold of the log *******************)

Cmd(k, g, s) == [k |-> k, g |-> g, set |-> s]

ApplyCmd(T, c, r) ==
  LET g == c.g
      gs == T[g]
      new ==
        CASE c.k = "create" ->
               [GS0 EXCEPT !.ex = TRUE, !.inc = r, !.reps = c.set,
                           !.rev = [n \in Nodes |-> IF n \in c.set THEN r ELSE 0], !.bad = gs.bad]
          [] c.k = "delete" -> [gs EXCEPT !.ex = FALSE, !.ust = NoUpd]
          [] c.k = "move" ->
               [gs EXCEPT !.ust = "in_progress", !.prev = gs.reps, !.tgt = c.set,
                          !.urev = r, !.reps = c.set, !.canc = FALSE, !.lcr = r]
          [] c.k = "cancel" ->
               IF gs.ust = "in_progress"
               THEN [gs EXCEPT !.ust = "cancelled", !.reps = gs.prev, !.canc = TRUE, !.lcr = r]
               ELSE IF CancelAfterFinish /\ gs.ust = NoUpd /\ gs.lastPrev # {}
               THEN [gs EXCEPT !.reps = gs.lastPrev, !.rev = gs.lastRev]  \* back to a finished move's old set
               ELSE gs
          [] c.k = "revert_cancel" ->
               IF gs.ust = "cancelled" THEN [gs EXCEPT !.ust = "in_progress", !.reps = gs.tgt, !.lcr = r] ELSE gs
          [] c.k = "finish" ->
               CASE gs.ust = "in_progress" ->
                      [gs EXCEPT !.ust = NoUpd, !.reps = gs.tgt, !.lastPrev = gs.prev, !.lastRev = gs.rev,
                                 !.rev = [n \in Nodes |-> IF n \in gs.tgt \ gs.prev THEN gs.urev ELSE gs.rev[n]],
                                 !.bad = gs.bad \/ gs.canc]
                 [] gs.ust = "cancelled" -> [gs EXCEPT !.ust = NoUpd, !.reps = gs.prev]
                 [] OTHER -> gs
          [] c.k = "finish_split" -> [gs EXCEPT !.ex = FALSE, !.ust = NoUpd]
          [] OTHER -> gs
  IN IF c.k = "start_split"
     THEN [T EXCEPT !["C"] = [GS0 EXCEPT !.ex = TRUE, !.inc = r, !.reps = c.set,
                                         !.rev = [n \in Nodes |-> IF n \in c.set THEN r ELSE 0]]]
     ELSE [T EXCEPT ![g] = new]

RECURSIVE TabR(_)
TabR(r) == IF r = 0 THEN [g \in {"P", "C"} |-> GS0] ELSE ApplyCmd(TabR(r - 1), clog[r], r)
Cur == Len(clog)
SplitStarted(r) == \E j \in 1..r : clog[j].k = "start_split"

Orig(gs) == IF gs.ust # NoUpd THEN gs.prev ELSE gs.reps
\* log_revision_on_node; with DeleteOnMove, the previous set isn't kept
ELR(gs, n) ==
  IF ~ gs.ex THEN 0
  ELSE IF n \in (IF DeleteOnMove THEN gs.reps ELSE Orig(gs)) THEN
         (IF n \in Orig(gs) THEN gs.rev[n] ELSE gs.urev)
  ELSE IF gs.ust # NoUpd /\ n \in gs.tgt THEN gs.urev
  ELSE 0

(*************************** raft ******************************************)

\* A configuration names vnodes <<node, log revision>> (Redpanda's vnode):
\* a member is live only on a replica of exactly that revision (RevCheck),
\* else on any replica of the node.
Nd(S) == {v[1] : v \in S}
Members(c) == c.old \cup c.new
VLive(v, g) == IF RevCheck THEN loc[v[1]][g] = v[2] ELSE loc[v[1]][g] # 0
\* a replica that may serve or recover (member or learner)
Live(n, g) == loc[n][g] # 0 /\ (RevCheck => loc[n][g] >= cfg[g].inc)
Quorum(S, g) == S = {} \/ 2 * Cardinality({v \in S : VLive(v, g)}) > Cardinality(S)
HasQuorum(g) == cfg[g].inc > 0 /\ Quorum(cfg[g].old, g) /\ Quorum(cfg[g].new, g)
Leaders(g) == {v[1] : v \in {v \in Members(cfg[g]) : VLive(v, g)}}
LiveMembers(g) == {v[1] : v \in {v \in Members(cfg[g]) : VLive(v, g)}}
\* the vnodes of node set S in table entry T: kept nodes keep their
\* revision, nodes the update adds get the update's
VSet(T, S) == {<<n, IF n \in Orig(T) THEN T.rev[n] ELSE T.urev>> : n \in S}

(*************************** actions ***************************************)

Init ==
  /\ clog = << >>
  /\ admin = 0
  /\ nrev = [n \in Nodes |-> 0]
  /\ loc = [n \in Nodes |-> [g \in {"P", "C"} |-> 0]]
  /\ dat = [n \in Nodes |-> [g \in {"P", "C"} |-> {}]]
  /\ cfg = [g \in {"P", "C"} |-> [inc |-> 0, kind |-> "simple", old |-> {}, new |-> {}, crev |-> 0]]
  /\ acked = [w \in Writes |-> <<"-", 0>>]
  /\ psnap = {"none"}
  /\ cstart = FALSE

Admin(c) ==
  /\ admin < MaxCmds
  /\ clog' = Append(clog, c) /\ admin' = admin + 1
  /\ UNCHANGED <<nrev, loc, dat, cfg, acked, psnap, cstart>>

AdminCmds ==
  LET T == TabR(Cur) IN
  \/ ~ T["P"].ex /\ Cur = 0 /\ \E n \in Nodes : Admin(Cmd("create", "P", {n}))
  \/ WithRecreate /\ ~ T["P"].ex /\ Cur > 0 /\ ~ SplitStarted(Cur)
     /\ \E n \in Nodes : Admin(Cmd("create", "P", {n}))
  \/ WithRecreate /\ T["P"].ex /\ T["P"].ust = NoUpd /\ ~ SplitStarted(Cur) /\ Admin(Cmd("delete", "P", {}))
  \/ \E g \in Groups : T[g].ex /\ T[g].ust = NoUpd
       /\ \E n \in Nodes : {n} # T[g].reps /\ Admin(Cmd("move", g, {n}))
  \/ \E g \in Groups : (T[g].ust = "in_progress" \/ (CancelAfterFinish /\ T[g].ex /\ T[g].ust = NoUpd /\ T[g].lastPrev # {}))
       /\ Admin(Cmd("cancel", g, {}))
  \/ WithSplit /\ T["P"].ex /\ ~ SplitStarted(Cur)
     /\ \E n \in Nodes : Admin(Cmd("start_split", "P", {n}))

NApply(n) ==
  /\ nrev[n] < Cur
  /\ nrev' = [nrev EXCEPT ![n] = @ + 1]
  /\ UNCHANGED <<clog, admin, loc, dat, cfg, acked, psnap, cstart>>

\* one reconciliation step for group g on node n (get_reconciliation_action)
Reconcile(n, g) ==
  LET T == TabR(nrev[n])[g]
      e == ELR(T, n)
      l == loc[n][g]
  IN \/ /\ l # 0 /\ (e = 0 \/ (RevCheck /\ l < e))          \* remove_partition
        /\ loc' = [loc EXCEPT ![n][g] = 0]
        /\ dat' = [dat EXCEPT ![n][g] = {}]
        /\ UNCHANGED cfg
     \/ /\ l = 0 /\ e # 0                                    \* create
        /\ loc' = [loc EXCEPT ![n][g] = e]
        /\ dat' = [dat EXCEPT ![n][g] = {}]
        \* the first replica of an incarnation bootstraps its configuration
        \* with the original replicas (initial_replicas)
        /\ cfg' = IF cfg[g].inc < T.inc
                  THEN [cfg EXCEPT ![g] = [inc |-> T.inc, kind |-> "simple", old |-> VSet(T, Orig(T)), new |-> {}, crev |-> T.inc]]
                  ELSE cfg
  /\ UNCHANGED <<clog, admin, nrev, acked, psnap, cstart>>

\* recovery: a live replica (member or learner) that is behind copies the
\* leader's data
Recover(n, g) ==
  /\ Live(n, g) /\ HasQuorum(g)
  /\ \E l \in Leaders(g) : l # n /\ ~ (dat[l][g] \subseteq dat[n][g])
       /\ dat' = [dat EXCEPT ![n][g] = @ \cup dat[l][g]]
  /\ UNCHANGED <<clog, admin, nrev, loc, cfg, acked, psnap, cstart>>

SetCfg(g, kind, old, new, rv) ==
  /\ RaftRevCheck => rv >= cfg[g].crev           \* raft refuses an older revision
  /\ cfg' = [cfg EXCEPT ![g].kind = kind, ![g].old = old, ![g].new = new, ![g].crev = rv]

\* the leader drives the update, from its node's table
RaftStep(g) ==
  /\ HasQuorum(g)
  /\ \E l \in Leaders(g) :
     LET T == TabR(nrev[l])[g]
         c == cfg[g]
     IN /\ T.ex /\ T.inc = c.inc
        /\ \/ \* enter joint consensus for the target
              /\ T.ust = "in_progress" /\ c.kind = "simple" /\ Nd(c.old) = T.prev
              /\ \A v \in VSet(T, T.tgt) : VLive(v, g) /\ dat[l][g] \subseteq dat[v[1]][g]  \* learners caught up
              /\ SetCfg(g, "joint", c.old, VSet(T, T.tgt), T.lcr)
           \/ \* leave joint for the new set
              /\ c.kind = "joint" /\ Quorum(c.new, g)
              /\ \A v \in c.new : dat[l][g] \subseteq dat[v[1]][g]
              /\ SetCfg(g, "simple", c.new, {}, c.crev)
           \/ \* cancel while joint: back to the previous set
              /\ T.ust = "cancelled" /\ c.kind = "joint" /\ Nd(c.new) = T.tgt
              /\ SetCfg(g, "simple", c.old, {}, T.lcr)
           \/ \* cancel after raft moved: reconfigure back ("back")
              /\ T.ust = "cancelled" /\ CancelMode = "back"
              /\ c.kind = "simple" /\ Nd(c.old) = T.tgt /\ T.tgt # T.prev
              /\ \A v \in VSet(T, T.prev) : VLive(v, g) /\ dat[l][g] \subseteq dat[v[1]][g]
              /\ SetCfg(g, "joint", c.old, VSet(T, T.prev), T.lcr)
           \/ \* already at the target (e.g. after revert_cancel): only the
              \* configuration's revision
              /\ T.ust = "in_progress" /\ c.kind = "simple" /\ Nd(c.old) = T.tgt /\ c.crev < T.lcr
              /\ SetCfg(g, "simple", c.old, {}, T.lcr)
           \/ \* cancel before raft moved: only the configuration's revision
              /\ T.ust = "cancelled" /\ c.kind = "simple" /\ Nd(c.old) = T.prev /\ c.crev < T.lcr
              /\ SetCfg(g, "simple", c.old, {}, T.lcr)
  /\ UNCHANGED <<clog, admin, nrev, loc, dat, acked, psnap, cstart>>

\* the leader dispatches finish (or revert_cancel) to the controller; it
\* checks the latest table so it dispatches once per update
Dispatch(g) ==
  /\ HasQuorum(g) /\ cfg[g].kind = "simple"
  /\ LET T == TabR(Cur)[g] IN
     /\ T.ex /\ T.inc = cfg[g].inc /\ T.ust # NoUpd
     /\ \E l \in Leaders(g) : l \in Nd(cfg[g].old)
     \* finish needs the configuration at the update's last command revision
     \* (check_configuration_update); revert_cancel doesn't
     /\ \/ T.ust = "in_progress" /\ Nd(cfg[g].old) = T.tgt
           /\ (RaftRevCheck => cfg[g].crev >= T.lcr)
           /\ clog' = Append(clog, Cmd("finish", g, {}))
        \/ T.ust = "cancelled" /\ Nd(cfg[g].old) = T.prev
           /\ (RaftRevCheck => cfg[g].crev >= T.lcr)
           /\ clog' = Append(clog, Cmd("finish", g, {}))
        \/ T.ust = "cancelled" /\ CancelMode = "revert" /\ Nd(cfg[g].old) = T.tgt /\ T.tgt # T.prev
           /\ clog' = Append(clog, Cmd("revert_cancel", g, {}))
  /\ UNCHANGED <<admin, nrev, loc, dat, cfg, acked, psnap, cstart>>

\* --- the split ---
PSplitEntry ==
  /\ WithSplit /\ SplitStarted(Cur) /\ psnap = {"none"} /\ HasQuorum("P")
  /\ \E l \in Leaders("P") : psnap' = dat[l]["P"]
  /\ UNCHANGED <<clog, admin, nrev, loc, dat, cfg, acked, cstart>>

CFirst ==
  /\ psnap # {"none"} /\ ~ cstart /\ HasQuorum("C") /\ TabR(Cur)["C"].inc = cfg["C"].inc
  /\ dat' = [n \in Nodes |-> [dat[n] EXCEPT !["C"] = IF n \in LiveMembers("C") THEN @ \cup psnap ELSE @]]
  /\ cstart' = TRUE
  /\ UNCHANGED <<clog, admin, nrev, loc, cfg, acked, psnap>>

FinishSplit ==
  /\ cstart /\ TabR(Cur)["P"].ex
  /\ clog' = Append(clog, Cmd("finish_split", "P", {}))
  /\ UNCHANGED <<admin, nrev, loc, dat, cfg, acked, psnap, cstart>>

\* --- client writes ---
Write(w, g) ==
  /\ acked[w] = <<"-", 0>>
  /\ LET T == TabR(Cur)[g] IN
     /\ T.ex /\ HasQuorum(g) /\ (RevCheck => cfg[g].inc = T.inc)
     /\ g = "P" => psnap = {"none"}
     /\ g = "C" => cstart
     /\ dat' = [n \in Nodes |-> [dat[n] EXCEPT ![g] = IF n \in LiveMembers(g) THEN @ \cup {w} ELSE @]]
     /\ acked' = [acked EXCEPT ![w] = <<g, T.inc>>]
  /\ UNCHANGED <<clog, admin, nrev, loc, cfg, psnap, cstart>>

Next ==
  \/ AdminCmds
  \/ \E n \in Nodes : NApply(n) \/ \E g \in Groups : Reconcile(n, g) \/ Recover(n, g)
  \/ \E g \in Groups : RaftStep(g) \/ Dispatch(g)
  \/ PSplitEntry \/ CFirst \/ FinishSplit
  \/ \E w \in Writes, g \in Groups : Write(w, g)

Spec == Init /\ [][Next]_vars

Fair ==
  /\ \A n \in Nodes : WF_vars(NApply(n)) /\ \A g \in Groups : WF_vars(Reconcile(n, g)) /\ WF_vars(Recover(n, g))
  /\ \A g \in Groups : WF_vars(RaftStep(g)) /\ WF_vars(Dispatch(g))
  /\ WF_vars(PSplitEntry) /\ WF_vars(CFirst) /\ WF_vars(FinishSplit)
LiveSpec == Spec /\ Fair

CmdBound == Len(clog) <= MaxCmds + 6

(*************************** properties ************************************)

Tab == TabR(Cur)

(* A replica is deleted only when no longer needed: a group that exists at *)
(* the latest revision, once a write was acknowledged to it (or C's first  *)
(* entry is in), keeps a quorum of each half of its configuration.         *)
Bootstrapped(g) ==
  (\E w \in Writes : acked[w] = <<g, Tab[g].inc>>) \/ (g = "C" /\ cstart)
QuorumKept == \A g \in Groups : (Tab[g].ex /\ Bootstrapped(g)) => HasQuorum(g)

(* No acknowledged write is lost: every write acknowledged to a group      *)
(* incarnation that still exists is held by a quorum of each half of its   *)
(* configuration (for P after the split: by C).                            *)
Durable(w, g) ==
  LET hold(S) == S = {} \/ 2 * Cardinality({v \in S : VLive(v, g) /\ w \in dat[v[1]][g]}) > Cardinality(S)
  IN hold(cfg[g].old) /\ hold(cfg[g].new)
NoLostAck ==
  \A w \in Writes : acked[w] # <<"-", 0>> =>
    LET g == acked[w][1] IN
    \/ Tab[g].ex /\ Tab[g].inc = acked[w][2] /\ Durable(w, g)
    \/ g = "P" /\ cstart /\ Durable(w, "C")
    \/ ~ (Tab[g].ex /\ Tab[g].inc = acked[w][2]) /\ ~ (g = "P" /\ SplitStarted(Cur))

(* A cancel returns to the old replica set: an update that was cancelled  *)
(* never finishes at its target.                                          *)
CancelRestores == \A g \in Groups : ~ Tab[g].bad

(* A recreated group is never confused with the original: every live      *)
(* member of a group's configuration holds only writes acknowledged to    *)
(* that group's current incarnation (or, for C, carried from P).           *)
NoConfusion ==
  \A g \in Groups : \A n \in LiveMembers(g) :
      \A w \in dat[n][g] : acked[w] = <<g, cfg[g].inc>> \/ (g = "C" /\ acked[w][1] = "P")

\* Reconciliation converges: every node applies every command, every group's
\* replicas are exactly the expected ones, and no update is left in progress.
Converged ==
  /\ \A n \in Nodes : nrev[n] = Cur
  /\ \A n \in Nodes, g \in Groups : loc[n][g] = ELR(Tab[g], n)
  /\ \A g \in Groups : Tab[g].ust = NoUpd
  /\ WithSplit /\ SplitStarted(Cur) => ~ Tab["P"].ex
Convergence == <>[]Converged

TypeOK == admin <= MaxCmds
Sym == Permutations(Nodes)
=============================================================================
