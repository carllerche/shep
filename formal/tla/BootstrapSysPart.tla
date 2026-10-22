------------------------------ MODULE BootstrapSysPart ------------------------------
(***************************************************************************)
(* SUPERSEDED (design v49). This is the old bootstrap: a system partition  *)
(* on seed nodes. §9.6 replaced it with the WAL controller group on every  *)
(* WAL node, founded from the static seed list; see Bootstrap.tla. Kept    *)
(* as history: its _selfmember counterexample motivated the static seed    *)
(* list, and _forward / _local are the counterexamples behind §14.1's      *)
(* "names are registered in production first" (production's registry now  *)
(* plays the role sys_prod plays here). Run with                           *)
(*   ./run.sh BootstrapSysPart BootstrapSysPart_<x>.cfg                    *)
(*                                                                         *)
(* Shep §9.6 (Protocol: bootstrap) and §14.1 (where global state lives).   *)
(*                                                                         *)
(* Two fleets, prod and np. Each fleet has a system partition sys_f on its *)
(* seed node s_f, a WAL node w_f that hosts one universe's meta and data   *)
(* partitions (prod: meta_p, data_p; np: meta_d, data_d for a dev          *)
(* universe), and a front end e_f that serves that universe. np's front    *)
(* end also runs the follower that copies production's app and schema-    *)
(* name records into sys_np. One replica per Raft group keeps the model    *)
(* small; a group is available when its replica is up and has joined it.  *)
(*                                                                         *)
(* Discovery (§9.6): a node that starts knows its static configuration     *)
(* (the seed list) and whatever its replicas persisted. Reaching a group   *)
(* needs its replica set. sys_f's WAL membership names the hosts of        *)
(* meta_u and data_u, and which universes the fleet hosts (so meta_u's     *)
(* location); meta_u's partition map names data_u. A replica can join its  *)
(* group once it knows the group's replica set, and persists it (Raft      *)
(* keeps its configuration in its own log). Crashes lose everything but    *)
(* what was persisted.                                                     *)
(*                                                                         *)
(* App and schema-name records: production's sys partition is the one      *)
(* authority (§14.1); np's copy follows it.                                *)
(*                                                                         *)
(* Knobs:                                                                  *)
(*  SysInSelf  FALSE (design): sys_f's replica set is static configuration *)
(*             (the seed list). TRUE: it is stored only in sys_f itself;   *)
(*             a node learns it from a seed that has already joined sys_f. *)
(*  NpRegister "none" (design, read-only copy): np never registers apps or *)
(*             schema names. "forward": np's registrations are sent to     *)
(*             production's sys partition. "local": np records them in its *)
(*             own sys partition, beside the copy.                         *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS SysInSelf, NpRegister, MaxCrashes, MaxRegs

Fleets == {"prod", "np"}
Nodes  == {"s_prod", "w_prod", "e_prod", "s_np", "w_np", "e_np"}
Parts  == {"sys_prod", "meta_p", "data_p", "sys_np", "meta_d", "data_d"}
Names  == {"n1"}
Owners == {"t1", "t2"}

FleetOf(n) == IF n \in {"s_prod", "w_prod", "e_prod"} THEN "prod" ELSE "np"
PFleet(p)  == IF p \in {"sys_prod", "meta_p", "data_p"} THEN "prod" ELSE "np"
Sys(f)  == IF f = "prod" THEN "sys_prod" ELSE "sys_np"
Meta(f) == IF f = "prod" THEN "meta_p" ELSE "meta_d"
Data(f) == IF f = "prod" THEN "data_p" ELSE "data_d"
Seed(f) == IF f = "prod" THEN "s_prod" ELSE "s_np"
Wal(f)  == IF f = "prod" THEN "w_prod" ELSE "w_np"

\* the actual replica placement (the WAL membership)
Host(p) == IF p = Sys(PFleet(p)) THEN Seed(PFleet(p)) ELSE Wal(PFleet(p))

\* which group stores the record of p's replica set
MembershipIn(p) == IF p = Sys(PFleet(p)) /\ ~SysInSelf THEN "static"
                   ELSE Sys(PFleet(p))

\* static configuration: the seed list gives sys_f's replica set, unless it
\* lives in sys_f; np's follower is also configured with prod's seeds
Static(n) == (IF SysInSelf THEN {} ELSE {Sys(FleetOf(n))})
             \cup (IF n = "e_np" /\ ~SysInSelf THEN {"sys_prod"} ELSE {})
\* the contact seeds a node may ask, when sys_f's membership is in sys_f
Contacts(n) == {Seed(FleetOf(n))} \cup (IF n = "e_np" THEN {"s_prod"} ELSE {})

\* the partitions a node must reach to do its job
Serves(n) ==
  LET f == FleetOf(n) IN
  CASE n = Seed(f) -> {Sys(f)}
    [] n = Wal(f)  -> {Sys(f), Meta(f), Data(f)}
    [] OTHER       -> {Sys(f), Meta(f), Data(f)}
                      \cup (IF n = "e_np" THEN {"sys_prod"} ELSE {})

VARIABLES
  up,        \* up[n]
  know,      \* partitions whose replica set n knows
  joined,    \* partitions n's replicas have joined (persisted, Raft config)
  reached,   \* partitions n has connected to
  crashes,
  prodApps,  \* sys_prod's app/schema-name records: [name, owner, origin]
  npCopied,  \* how many of them sys_np has copied
  npLocal,   \* records sys_np wrote itself ("local")
  wrote      \* wrote[p]: the fleets whose processes wrote to partition p

vars == <<up, know, joined, reached, crashes, prodApps, npCopied, npLocal, wrote>>
appVars == <<prodApps, npCopied, npLocal>>

Init ==
  /\ up = [n \in Nodes |-> FALSE]
  /\ know = [n \in Nodes |-> {}]
  /\ joined = [n \in Nodes |-> {}]
  /\ reached = [n \in Nodes |-> {}]
  /\ crashes = 0
  /\ prodApps = << >>
  /\ npCopied = 0
  /\ npLocal = {}
  /\ wrote = [p \in Parts |-> {}]

Available(p) == up[Host(p)] /\ p \in joined[Host(p)]

(*************************** discovery *************************************)

Start(n) ==
  /\ ~up[n]
  /\ up' = [up EXCEPT ![n] = TRUE]
  /\ know' = [know EXCEPT ![n] = Static(n) \cup joined[n]]
  /\ UNCHANGED <<joined, reached, crashes, appVars, wrote>>

Crash(n) ==
  /\ up[n] /\ crashes < MaxCrashes
  /\ up' = [up EXCEPT ![n] = FALSE]
  /\ know' = [know EXCEPT ![n] = {}]
  /\ reached' = [reached EXCEPT ![n] = {}]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<joined, appVars, wrote>>

\* SysInSelf: ask a contact seed for sys_f's replica set; it can answer
\* only once it has joined sys_f
AskSeed(n, s) ==
  /\ SysInSelf /\ up[n] /\ s \in Contacts(n) /\ up[s]
  /\ \E p \in joined[s] : p = Sys(FleetOf(s)) /\ p \notin know[n]
       /\ know' = [know EXCEPT ![n] = @ \cup {p}]
  /\ UNCHANGED <<up, joined, reached, crashes, appVars, wrote>>

\* a replica joins its group once it knows the group's replica set
Join(n, p) ==
  /\ up[n] /\ Host(p) = n /\ p \in know[n] /\ p \notin joined[n]
  /\ joined' = [joined EXCEPT ![n] = @ \cup {p}]
  /\ reached' = [reached EXCEPT ![n] = @ \cup {p}]
  /\ wrote' = [wrote EXCEPT ![p] = @ \cup {FleetOf(n)}]   \* its Raft log
  /\ UNCHANGED <<up, know, crashes, appVars>>

Reach(n, p) ==
  /\ up[n] /\ p \in know[n] /\ p \notin reached[n] /\ Available(p)
  /\ reached' = [reached EXCEPT ![n] = @ \cup {p}]
  /\ UNCHANGED <<up, know, joined, crashes, appVars, wrote>>

\* reading the fleet's sys partition: the WAL membership (who hosts what)
\* and the fleet's universes (where their meta partitions are)
ReadSys(n) ==
  LET f == FleetOf(n)
      learn == {Meta(f)} \cup {p \in Parts : Host(p) = n /\ PFleet(p) = f}
  IN /\ up[n] /\ Sys(f) \in reached[n] /\ Available(Sys(f))
     /\ ~(learn \subseteq know[n])
     /\ know' = [know EXCEPT ![n] = @ \cup learn]
     /\ UNCHANGED <<up, joined, reached, crashes, appVars, wrote>>

\* reading the meta partition's partition map (data partitions; their hosts
\* come from the WAL membership already read)
ReadMeta(n) ==
  LET f == FleetOf(n) IN
  /\ up[n] /\ Meta(f) \in reached[n] /\ Available(Meta(f))
  /\ Sys(f) \in reached[n]
  /\ Data(f) \notin know[n]
  /\ know' = [know EXCEPT ![n] = @ \cup {Data(f)}]
  /\ UNCHANGED <<up, joined, reached, crashes, appVars, wrote>>

(*************************** apps and schema names *************************)

Taken(name) == \E i \in 1..Len(prodApps) : prodApps[i].name = name

\* a production admin registers an app / reserves a schema name
RegisterProd(name, o) ==
  /\ Len(prodApps) + Cardinality(npLocal) < MaxRegs
  /\ up["e_prod"] /\ "sys_prod" \in reached["e_prod"] /\ Available("sys_prod")
  /\ ~Taken(name)
  /\ prodApps' = Append(prodApps, [name |-> name, owner |-> o, origin |-> "prod"])
  /\ wrote' = [wrote EXCEPT !["sys_prod"] = @ \cup {"prod"}]
  /\ UNCHANGED <<up, know, joined, reached, crashes, npCopied, npLocal>>

\* a dev session or test run in np registers one
RegisterNp(name, o) ==
  /\ Len(prodApps) + Cardinality(npLocal) < MaxRegs
  /\ up["e_np"]
  /\ CASE NpRegister = "forward" ->
            /\ "sys_prod" \in reached["e_np"] /\ Available("sys_prod")
            /\ ~Taken(name)
            /\ prodApps' = Append(prodApps, [name |-> name, owner |-> o, origin |-> "np"])
            /\ wrote' = [wrote EXCEPT !["sys_prod"] = @ \cup {"np"}]
            /\ UNCHANGED npLocal
       [] NpRegister = "local" ->
            /\ "sys_np" \in reached["e_np"] /\ Available("sys_np")
            /\ ~\E r \in npLocal : r.name = name
            /\ ~\E i \in 1..npCopied : prodApps[i].name = name
            /\ npLocal' = npLocal \cup {[name |-> name, owner |-> o, origin |-> "np"]}
            /\ wrote' = [wrote EXCEPT !["sys_np"] = @ \cup {"np"}]
            /\ UNCHANGED prodApps
       [] OTHER -> FALSE
  /\ UNCHANGED <<up, know, joined, reached, crashes, npCopied>>

\* np's follower copies the next production record (read-only on prod)
Follow ==
  /\ up["e_np"]
  /\ {"sys_prod", "sys_np"} \subseteq reached["e_np"]
  /\ Available("sys_prod") /\ Available("sys_np")
  /\ npCopied < Len(prodApps)
  /\ npCopied' = npCopied + 1
  /\ wrote' = [wrote EXCEPT !["sys_np"] = @ \cup {"np"}]
  /\ UNCHANGED <<up, know, joined, reached, crashes, prodApps, npLocal>>

(*************************** spec ******************************************)

Discover ==
  \/ \E n \in Nodes : Start(n) \/ ReadSys(n) \/ ReadMeta(n)
  \/ \E n \in Nodes, p \in Parts : Join(n, p) \/ Reach(n, p)
  \/ \E n, s \in Nodes : AskSeed(n, s)

Next ==
  \/ Discover
  \/ \E n \in Nodes : Crash(n)
  \/ \E name \in Names, o \in Owners : RegisterProd(name, o) \/ RegisterNp(name, o)
  \/ Follow

Fairness ==
  /\ \A n \in Nodes : WF_vars(Start(n)) /\ WF_vars(ReadSys(n)) /\ WF_vars(ReadMeta(n))
  /\ \A n \in Nodes, p \in Parts : WF_vars(Join(n, p)) /\ WF_vars(Reach(n, p))
  /\ \A n, s \in Nodes : WF_vars(AskSeed(n, s))
  /\ WF_vars(Follow)

Spec == Init /\ [][Next]_vars /\ Fairness

(*************************** properties ************************************)

\* The system partition never stores its own membership.
SysNotSelf == \A f \in Fleets : MembershipIn(Sys(f)) /= Sys(f)
\* The fleets share no writable partition.
NoSharedWritable == \A p \in Parts : wrote[p] \subseteq {PFleet(p)}
\* Non-production never writes production's app records.
NpNeverWritesProd == \A i \in 1..Len(prodApps) : prodApps[i].origin = "prod"
\* An app or schema name means the same thing in every fleet.
NamesAgree ==
  \A r \in npLocal \cup {prodApps[i] : i \in 1..npCopied} :
    \A i \in 1..Len(prodApps) : prodApps[i].name = r.name => prodApps[i].owner = r.owner
\* Every node eventually reaches each partition it serves (and stays so).
AllReached == \A n \in Nodes : up[n] /\ Serves(n) \subseteq reached[n]
EventuallyReached == <>[]AllReached
\* np's copy eventually catches up with production's records.
EventuallyCopied == <>[](npCopied = Len(prodApps))
=============================================================================
