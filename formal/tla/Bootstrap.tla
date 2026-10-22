------------------------------- MODULE Bootstrap -------------------------------
(***************************************************************************)
(* Shep design v49, §9.6 "Protocol: bootstrap", "The WAL controller"       *)
(* (cluster identity), and "Growing" / "The controller is a group of one"  *)
(* (single node, RF 1). Supersedes BootstrapSysPart.tla (the old system    *)
(* partition on seed nodes).                                               *)
(*                                                                         *)
(* Every WAL node hosts a replica of the controller Raft group. A node's   *)
(* static configuration is its seed list and its fleet (prod or np). Its   *)
(* disk holds the cluster UUID and its controller log (a prefix of the     *)
(* cluster's log). A re-imaged node loses its disk and restarts, possibly  *)
(* with a different configuration (repurposed to the other fleet, or       *)
(* misconfigured: np fleet with production's seed list).                   *)
(*                                                                         *)
(* Founding (§9.6): a seed whose seeds are all reachable and empty, and    *)
(* agree on the list, founds the cluster with the seed list as the initial *)
(* configuration. Founding is atomic here: the handshake among seeds and   *)
(* the first election, whose leader writes the cluster UUID as the first   *)
(* controller command, are one step. Every founder persists the UUID.      *)
(*                                                                         *)
(* Joining: an empty node asks a contact (its seeds) that is part of a     *)
(* cluster; the controller's leader appends a membership change and the   *)
(* node adopts the UUID. Membership changes go through joint consensus:   *)
(* C(old,new), then C(new). One voter growing: the second node joins as a  *)
(* learner, and the third's join promotes both in one joint change (1->3). *)
(*                                                                         *)
(* The controller log is abstracted: clog[c] is the cluster's log (as held *)
(* by its current leader); each member holds a prefix len[n]. Entries are  *)
(* configurations (the configuration in effect at that entry) or plain     *)
(* commands. Commit needs a quorum (both majorities in a joint config) of  *)
(* members holding the entry; election needs a quorum of voters whose logs *)
(* are no longer than the candidate's (the up-to-date rule), and the new   *)
(* leader's log replaces the cluster's (followers are truncated). Terms    *)
(* are not modelled; only a leader that is down or wiped is replaced.     *)
(*                                                                         *)
(* Knobs (design v49 values first; this model's proposed fixes marked +): *)
(*  AllEmpty   TRUE: founding needs every seed reachable and empty.        *)
(*             FALSE: founds with the reachable empty seeds.               *)
(*  Guard      "seeds": founding consults only the seeds. "claim"+: also   *)
(*             a conditional create of a cluster claim in the object store *)
(*             (survives a node's disk), and a wiped node may contact the  *)
(*             claimed cluster's members.                                  *)
(*  UuidCheck  TRUE: every message between nodes carries the cluster UUID; *)
(*             a mismatch (or a node with none) is rejected.               *)
(*  EnvCheck   FALSE (as written). TRUE+: the fleet name is configured,   *)
(*             recorded at founding, and checked when a node joins.        *)
(*  Rejoin     how a re-imaged node that is still a member rejoins:        *)
(*             "same": identity is the address, so it is the old member    *)
(*             with an empty log. "add" (as written, a join is a join): a  *)
(*             new identity (node UUID), added beside the dead old one.    *)
(*             "replace"+: one joint change swaps the old identity for the *)
(*             new one.                                                    *)
(*  SecondAs   "learner" (design): the second node joins a one-voter       *)
(*             controller as a learner. "voter": as a voter.               *)
(*  PromoteCheck FALSE (as written): the third node's join promotes the    *)
(*             learner. TRUE+: only a live learner holding the committed   *)
(*             log; otherwise the newcomer joins as another learner.       *)
(*  AddGuard   FALSE (as written). TRUE+: a join that adds a voter waits    *)
(*             while another voter's identity is dead (re-imaged, not yet  *)
(*             replaced); the leader sees it unresponsive.                 *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS Nodes, ProdSeeds, NpSeeds, NpNodes,
          AllEmpty, Guard, UuidCheck, EnvCheck, Rejoin, SecondAs, PromoteCheck, AddGuard,
          Repurpose,
          MaxWipes, MaxCrashes, MaxLog, MaxClusters

Clusters  == 1..MaxClusters
SeedLists == {ProdSeeds} \cup (IF NpSeeds = {} THEN {} ELSE {NpSeeds})
None == "none"

VARIABLES
  up,        \* up[n]
  inc,       \* inc[n]: incarnation; a wipe makes a new node identity
  env,       \* env[n]: configured fleet ("prod" / "np")
  seeds,     \* seeds[n]: configured seed list
  uuid,      \* uuid[n]: persisted cluster UUID, 0 if none
  len,       \* len[n]: how much of clog[uuid[n]] n's disk holds
  held,      \* held[n]: clusters whose controller entries n accepted since its last wipe
  clog,      \* clog[c]: cluster c's controller log
  commit,    \* commit[c]: highest index ever committed
  leader,    \* leader[c]: node or None
  cenv,      \* cenv[c]: the fleet of c's founders
  claim,     \* claim[S]: cluster claimed for seed list S in the object store (Guard = "claim")
  nextC,     \* next cluster UUID
  wipes, crashes,
  growthBad  \* history: some membership change made a node newly critical

vars == <<up, inc, env, seeds, uuid, len, held, clog, commit, leader, cenv,
          claim, nextC, wipes, crashes, growthBad>>

Init ==
  /\ up = [n \in Nodes |-> FALSE]
  /\ inc = [n \in Nodes |-> 0]
  /\ env = [n \in Nodes |-> IF n \in NpNodes THEN "np" ELSE "prod"]
  /\ seeds = [n \in Nodes |-> IF n \in NpNodes THEN NpSeeds ELSE ProdSeeds]
  /\ uuid = [n \in Nodes |-> 0]
  /\ len = [n \in Nodes |-> 0]
  /\ held = [n \in Nodes |-> {}]
  /\ clog = [c \in Clusters |-> << >>]
  /\ commit = [c \in Clusters |-> 0]
  /\ leader = [c \in Clusters |-> None]
  /\ cenv = [c \in Clusters |-> None]
  /\ claim = [s \in SeedLists |-> 0]
  /\ nextC = 1
  /\ wipes = 0
  /\ crashes = 0
  /\ growthBad = FALSE

(*************************** identities and configurations *****************)

\* a controller member's identity
Id(n) == IF Rejoin = "same" THEN [a |-> n, i |-> 0] ELSE [a |-> n, i |-> inc[n]]
Ids(S) == {Id(n) : n \in S}
\* the identity is still embodied by its node (not wiped since)
Live(id) == Id(id.a) = id

Voters(cfg)  == cfg.old \cup cfg.new
Members(cfg) == cfg.old \cup cfg.new \cup cfg.lrn
Maj(S, V) == 2 * Cardinality(S \cap V) > Cardinality(V)
Quorum(cfg, S) == Maj(S, cfg.old) /\ Maj(S, cfg.new)
\* voters whose single failure loses the quorum, with every live identity up
Crit(cfg) ==
  LET L == {id \in Voters(cfg) : Live(id)} IN
  {id \in L : ~Quorum(cfg, L \ {id})}

Simple(V, L) == [old |-> V, new |-> V, lrn |-> L]
LastCfg(c) == clog[c][Len(clog[c])].cfg
CfgAt(c, k) == clog[c][k].cfg

LeaderOK(c) ==
  /\ leader[c] /= None
  /\ up[leader[c]] /\ uuid[leader[c]] = c

ClearLeader(n) == [c \in Clusters |-> IF leader[c] = n THEN None ELSE leader[c]]

\* message acceptance at node m for cluster c's controller, sent to identity id
Accepts(m, c, id) ==
  /\ up[m]
  /\ Id(m) = id
  /\ UuidCheck => uuid[m] = c

(*************************** founding and joining **************************)

Found(s) ==
  LET S == seeds[s]
      Ready(t) == up[t] /\ uuid[t] = 0 /\ seeds[t] = S
      F == IF AllEmpty THEN S ELSE {t \in S : Ready(t)}
      c == nextC
  IN /\ up[s] /\ uuid[s] = 0 /\ s \in S /\ nextC <= MaxClusters
     /\ AllEmpty => \A t \in S : Ready(t)
     /\ Guard = "claim" => claim[S] = 0
     /\ EnvCheck => \A t \in F : env[t] = env[s]
     /\ nextC' = nextC + 1
     /\ uuid' = [t \in Nodes |-> IF t \in F THEN c ELSE uuid[t]]
     /\ len' = [t \in Nodes |-> IF t \in F THEN 1 ELSE len[t]]
     /\ held' = [t \in Nodes |-> IF t \in F THEN {c} ELSE held[t]]
     /\ clog' = [clog EXCEPT ![c] = << [cfg |-> Simple(Ids(F), {}), k |-> "boot"] >>]
     /\ commit' = [commit EXCEPT ![c] = 1]
     /\ leader' = [leader EXCEPT ![c] = s]
     /\ cenv' = [cenv EXCEPT ![c] = env[s]]
     /\ claim' = IF Guard = "claim" THEN [claim EXCEPT ![S] = c] ELSE claim
     /\ UNCHANGED <<up, inc, env, seeds, wipes, crashes, growthBad>>

\* who an empty node can ask: its seeds, and (claim) the claimed cluster's members
Contacts(n) ==
  (seeds[n] \ {n})
  \cup (IF Guard = "claim" /\ claim[seeds[n]] /= 0
        THEN {m \in Nodes : uuid[m] = claim[seeds[n]]} ELSE {})

\* learners the third node's join promotes (PromoteCheck: only live ones
\* that hold the committed log)
Promotable(c, cur) ==
  IF PromoteCheck
  THEN {l \in cur.lrn : Live(l) /\ uuid[l.a] = c /\ len[l.a] >= commit[c]}
  ELSE cur.lrn

\* the membership change the leader appends for n's join to cluster c
JoinCfg(c, cur, n) ==
  LET nid == Id(n)
      old == {id \in Members(cur) : id.a = n}
      V == cur.new
      R == Promotable(c, cur)
  IN IF Rejoin = "replace" /\ old /= {}
     THEN IF old \subseteq cur.lrn
          THEN Simple(V, (cur.lrn \ old) \cup {nid})
          ELSE [old |-> V, new |-> (V \ old) \cup {nid}, lrn |-> cur.lrn]
     ELSE IF Cardinality(V) = 1 /\ SecondAs = "learner" /\ R = {}
     THEN Simple(V, cur.lrn \cup {nid})
     ELSE IF Cardinality(V) = 1 /\ SecondAs = "learner"
     THEN [old |-> V, new |-> V \cup R \cup {nid}, lrn |-> cur.lrn \ R]
     ELSE [old |-> V, new |-> V \cup {nid}, lrn |-> cur.lrn]

Join(n, m) ==
  LET c == uuid[m]
      L == leader[c]
      cur == LastCfg(c)
      same == Rejoin = "same" /\ \E id \in Members(cur) : id.a = n
      ncfg == JoinCfg(c, cur, n)
  IN /\ up[n] /\ uuid[n] = 0 /\ m \in Contacts(n) /\ up[m] /\ c /= 0
     /\ LeaderOK(c)
     /\ EnvCheck => env[n] = cenv[c]
     /\ uuid' = [uuid EXCEPT ![n] = c]
     /\ held' = [held EXCEPT ![n] = {c}]
     /\ IF same
        THEN \* the old member, back with an empty log
             /\ len' = [len EXCEPT ![n] = 0]
             /\ UNCHANGED <<clog, growthBad>>
        ELSE /\ commit[c] = Len(clog[c]) /\ cur.old = cur.new
             /\ AddGuard => \A id \in cur.new : id.a /= n => Live(id)
             /\ Len(clog[c]) < MaxLog
             /\ clog' = [clog EXCEPT ![c] = Append(@, [cfg |-> ncfg, k |-> "cfg"])]
             /\ len' = [len EXCEPT ![n] = 0, ![L] = Len(clog[c]) + 1]
             /\ growthBad' = (growthBad \/ ~(Crit(ncfg) \subseteq Crit(cur)))
     /\ UNCHANGED <<up, inc, env, seeds, commit, leader, cenv, claim, nextC,
                    wipes, crashes>>

FinishJoint(c) ==
  LET cur == LastCfg(c)
      ncfg == Simple(cur.new, cur.lrn)
  IN /\ LeaderOK(c) /\ cur.old /= cur.new
     /\ commit[c] = Len(clog[c]) /\ Len(clog[c]) < MaxLog
     /\ clog' = [clog EXCEPT ![c] = Append(@, [cfg |-> ncfg, k |-> "cfg"])]
     /\ len' = [len EXCEPT ![leader[c]] = Len(clog[c]) + 1]
     /\ growthBad' = (growthBad \/ ~(Crit(ncfg) \subseteq Crit(cur)))
     /\ UNCHANGED <<up, inc, env, seeds, uuid, held, commit, leader, cenv,
                    claim, nextC, wipes, crashes>>

\* any other controller command (a universe create, a split, ...)
Command(c) ==
  /\ LeaderOK(c) /\ Len(clog[c]) < MaxLog
  /\ clog' = [clog EXCEPT ![c] = Append(@, [cfg |-> LastCfg(c), k |-> "cmd"])]
  /\ len' = [len EXCEPT ![leader[c]] = Len(clog[c]) + 1]
  /\ UNCHANGED <<up, inc, env, seeds, uuid, held, commit, leader, cenv,
                 claim, nextC, wipes, crashes, growthBad>>

(*************************** replication, commit, election *****************)

Replicate(c, id) ==
  LET m == id.a IN
  /\ LeaderOK(c) /\ id \in Members(LastCfg(c)) /\ m /= leader[c]
  /\ Accepts(m, c, id)
  /\ ~(uuid[m] = c /\ len[m] = Len(clog[c]))
  /\ uuid' = [uuid EXCEPT ![m] = c]
  /\ len' = [len EXCEPT ![m] = Len(clog[c])]
  /\ held' = [held EXCEPT ![m] = @ \cup {c}]
  /\ UNCHANGED <<up, inc, env, seeds, clog, commit, leader, cenv, claim,
                 nextC, wipes, crashes, growthBad>>

Acks(c, k) == {Id(m) : m \in {x \in Nodes : uuid[x] = c /\ len[x] >= k}}

AdvanceCommit(c) ==
  /\ LeaderOK(c)
  /\ \E k \in (commit[c] + 1)..Len(clog[c]) :
       /\ Quorum(LastCfg(c), Acks(c, k))
       /\ \A k2 \in (k + 1)..Len(clog[c]) : ~Quorum(LastCfg(c), Acks(c, k2))
       /\ commit' = [commit EXCEPT ![c] = k]
  /\ UNCHANGED <<up, inc, env, seeds, uuid, len, held, clog, leader, cenv,
                 claim, nextC, wipes, crashes, growthBad>>

Elect(c, n) ==
  LET cfg == CfgAt(c, len[n])
      Grants == {id \in Voters(cfg) :
                   LET m == id.a IN
                   /\ up[m] /\ Id(m) = id
                   /\ IF UuidCheck THEN uuid[m] = c ELSE TRUE
                   /\ uuid[m] = c => len[m] <= len[n]}
  IN /\ ~LeaderOK(c)
     /\ up[n] /\ uuid[n] = c /\ len[n] >= 1
     /\ Id(n) \in Voters(cfg)
     /\ Quorum(cfg, Grants)
     /\ leader' = [leader EXCEPT ![c] = n]
     /\ clog' = [clog EXCEPT ![c] = SubSeq(@, 1, len[n])]
     /\ len' = [m \in Nodes |-> IF uuid[m] = c /\ len[m] > len[n] THEN len[n] ELSE len[m]]
     /\ UNCHANGED <<up, inc, env, seeds, uuid, held, commit, cenv, claim,
                    nextC, wipes, crashes, growthBad>>

(*************************** failures **************************************)

Start(n) ==
  /\ ~up[n]
  /\ up' = [up EXCEPT ![n] = TRUE]
  /\ UNCHANGED <<inc, env, seeds, uuid, len, held, clog, commit, leader, cenv,
                 claim, nextC, wipes, crashes, growthBad>>

Crash(n) ==
  /\ up[n] /\ crashes < MaxCrashes
  /\ up' = [up EXCEPT ![n] = FALSE]
  /\ leader' = ClearLeader(n)
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<inc, env, seeds, uuid, len, held, clog, commit, cenv, claim,
                 nextC, wipes, growthBad>>

\* configurations a re-imaged node may come back with
Reconfigs(n) ==
  {[e |-> env[n], s |-> seeds[n]]}
  \cup (IF Repurpose
        THEN {[e |-> "prod", s |-> ProdSeeds], [e |-> "np", s |-> ProdSeeds]}
             \cup (IF NpSeeds = {} THEN {} ELSE {[e |-> "np", s |-> NpSeeds]})
        ELSE {})

\* re-image: the disk is wiped and the node restarts
Wipe(n, r) ==
  /\ wipes < MaxWipes
  /\ r \in Reconfigs(n)
  /\ wipes' = wipes + 1
  /\ up' = [up EXCEPT ![n] = TRUE]
  /\ inc' = [inc EXCEPT ![n] = @ + 1]
  /\ env' = [env EXCEPT ![n] = r.e]
  /\ seeds' = [seeds EXCEPT ![n] = r.s]
  /\ uuid' = [uuid EXCEPT ![n] = 0]
  /\ len' = [len EXCEPT ![n] = 0]
  /\ held' = [held EXCEPT ![n] = {}]
  /\ leader' = ClearLeader(n)
  /\ UNCHANGED <<clog, commit, cenv, claim, nextC, crashes, growthBad>>

(*************************** spec ******************************************)

Next ==
  \/ \E n \in Nodes : Start(n) \/ Crash(n) \/ Found(n)
  \/ \E n \in Nodes : \E r \in Reconfigs(n) : Wipe(n, r)
  \/ \E n, m \in Nodes : Join(n, m)
  \/ \E c \in 1..(nextC - 1) :
       \/ FinishJoint(c) \/ Command(c) \/ AdvanceCommit(c)
       \/ \E id \in Members(LastCfg(c)) : Replicate(c, id)
       \/ \E n \in Nodes : Elect(c, n)

Spec == Init /\ [][Next]_vars

(*************************** properties ************************************)

\* Nodes that share a seed are never in two different clusters. Only nodes
\* configured consistently (a fleet with its own seed list) count; a node
\* misconfigured with the other fleet's seeds is ProdNpNeverMix's business.
Consistent(n) == (env[n] = "prod" /\ seeds[n] = ProdSeeds) \/ (env[n] = "np" /\ seeds[n] = NpSeeds)
OneClusterPerSeeds ==
  \A n, m \in Nodes :
    (uuid[n] /= 0 /\ uuid[m] /= 0 /\ Consistent(n) /\ Consistent(m)
     /\ seeds[n] \cap seeds[m] /= {})
      => uuid[n] = uuid[m]

\* A node never holds two clusters' controller entries (since its last wipe).
NoNodeInTwoClusters == \A n \in Nodes : Cardinality(held[n]) <= 1

\* Production and non-production never mix: a node only ever holds entries
\* of a cluster founded by its own fleet.
ProdNpNeverMix == \A n \in Nodes : \A c \in held[n] : cenv[c] = env[n]

\* No growth step makes a node critical that wasn't (the controller never
\* depends on a new node alone for its quorum).
GrowthKeepsQuorum == ~growthBad

\* A committed controller entry is never lost.
CommittedKept == \A c \in Clusters : commit[c] <= Len(clog[c])

\* witnesses (expected to be violated: the paths are reachable)
NoGrowth == \A c \in Clusters : Len(clog[c]) > 0 => Cardinality(Voters(LastCfg(c))) < 3
NoRejoin == \A n \in Nodes : inc[n] > 0 => uuid[n] = 0
=============================================================================
