--------------------------- MODULE ControllerCreate ---------------------------
(***************************************************************************)
(* Shep design §9.6 (the WAL controller): creating log sets, as the        *)
(* implementation does it, checked before moves and splits.                *)
(*                                                                         *)
(* The controller manages log sets: a named list of partitions with        *)
(* LogSetRules {max_partitions, names}; the rules are data, identical on   *)
(* every node. (Shep reads a log set as a universe: partitions[0] is the  *)
(* meta partition, the rest data partitions; max_partitions = 1 + 1024,    *)
(* domain names. The model scales max_partitions down.)                    *)
(*                                                                         *)
(* The controller is an embedder of the shep-raft Machine contract:        *)
(*  - admit, on the leader, checks CREATE_LOG_SET {name, partitions,       *)
(*    replication} against the leader's APPLIED state (which may lag the   *)
(*    log): the rules, and that the name is free; it does placement there *)
(*    and embeds the partition ids, taken from next_partition, and the     *)
(*    replica nodes in the command;                                        *)
(*  - the command is appended in the leader's current term, which may     *)
(*    differ from the term it was admitted in: then it is VOID (VoidRule); *)
(*  - apply re-checks deterministically (ReCheck): rules, name free, and  *)
(*    the embedded ids start at next_partition; else it is rejected.       *)
(* The applied state is a fold of the log.                                 *)
(*                                                                         *)
(* The create-only reconciler: on every controller revision it applies, a *)
(* node creates the replicas it is assigned and doesn't have, and the      *)
(* lowest assigned node initializes the group (AlreadyInitialized is       *)
(* tolerated). A crash loses the node's applied revision (it replays the   *)
(* log on restart) but not its replicas on disk.                           *)
(*                                                                         *)
(* The environment can create and initialize a partition by hand           *)
(* (CreatePartition, InitializePartition) and write to it: only on         *)
(* seedless, standalone nodes (the fix: ControllerManaged elsewhere);      *)
(* ManualAnywhere drops the fix.                                           *)
(*                                                                         *)
(* Nodes are numbers, so "lowest" is the least.                            *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  Nodes,          \* WAL nodes (numbers)
  Managed,        \* nodes with seeds: controller-managed
  Names,          \* log set names a client may ask for
  DomainNames,    \* the names LogSetRules.names (Domain) allows
  MaxPartitions,  \* LogSetRules.max_partitions
  Counts,         \* partition counts a client may ask for
  MaxId,          \* partition ids 1..MaxId
  Writes,         \* write ids
  MaxTerm,        \* controller terms 1..MaxTerm
  VoidRule,       \* TRUE: admitted in one term, appended in another: VOID
  ReCheck,        \* TRUE: apply re-checks the command against the state
  ManualAnywhere, \* TRUE: manual create is allowed on managed nodes too
  Crashes,        \* bound on node crashes
  MaxLog          \* bound on the controller log's length (a leader whose
                  \*   applied state lags can admit the same name again and
                  \*   again; each append is rejected at apply, but without a
                  \*   bound the log, and the state space, are unbounded)

ASSUME Managed \subseteq Nodes

Ids == 1..MaxId
NoRep == [origin |-> "none", set |-> "-"]
Min(S) == CHOOSE n \in S : \A m \in S : n <= m
Range(s) == {s[i] : i \in DOMAIN s}

VARIABLES
  term,     \* the controller leader's term
  lai,      \* the leader's applied index (its applied state)
  pend,     \* admitted commands not yet appended
  clog,     \* the controller log: [name, k, ids, nodes, aterm, pterm]
  nrev,     \* node -> applied revision
  rep,      \* node -> id -> [origin: "ctl" | "manual" | "none", set]
  initBy,   \* id -> nodes that initialized its group
  dat,      \* node -> id -> writes held
  wtag,     \* write -> what it was written to: a log set, "manual", or "-"
  crashes

vars == <<term, lai, pend, clog, nrev, rep, initBy, dat, wtag, crashes>>

(*************************** the applied state: a fold of the log **********)

RulesOK(name, k) == k <= MaxPartitions /\ k >= 1 /\ name \in DomainNames
\* sets: name -> the entries that created it (empty: the name is free)
St0 == [sets |-> [n \in Names |-> {}], next |-> 1, eff |-> {}]
Void(e) == VoidRule /\ e.aterm # e.pterm
ApplyE(st, e, i) ==
  LET ok == /\ ~ Void(e)
            /\ ReCheck => /\ RulesOK(e.name, e.k)
                          /\ st.sets[e.name] = {}
                          /\ e.ids[1] = st.next
  IN IF ok
     THEN [sets |-> [st.sets EXCEPT ![e.name] = @ \cup {i}],
           next |-> e.ids[e.k] + 1,
           eff  |-> st.eff \cup {i}]
     ELSE st
RECURSIVE StateAt(_)
StateAt(r) == IF r = 0 THEN St0 ELSE ApplyE(StateAt(r - 1), clog[r], r)
Cur == Len(clog)

(*************************** actions ***************************************)

Init ==
  /\ term = 1
  /\ lai = 0
  /\ pend = {}
  /\ clog = << >>
  /\ nrev = [n \in Nodes |-> 0]
  /\ rep = [n \in Nodes |-> [p \in Ids |-> NoRep]]
  /\ initBy = [p \in Ids |-> {}]
  /\ dat = [n \in Nodes |-> [p \in Ids |-> {}]]
  /\ wtag = [w \in Writes |-> "-"]
  /\ crashes = 0

\* admit CREATE_LOG_SET against the leader's applied state; placement and
\* the partition ids are fixed here
Admit(name, k, S) ==
  LET st == StateAt(lai) IN
  /\ RulesOK(name, k) /\ st.sets[name] = {}
  /\ S # {} /\ st.next + k - 1 <= MaxId
  /\ ~ \E c \in pend : c.name = name /\ c.aterm = term
  /\ pend' = pend \cup {[name |-> name, k |-> k, ids |-> [i \in 1..k |-> st.next + i - 1],
                          nodes |-> S, aterm |-> term]}
  /\ UNCHANGED <<term, lai, clog, nrev, rep, initBy, dat, wtag, crashes>>

\* the leader appends an admitted command in its current term
AppendCmd(c) ==
  /\ c \in pend /\ Len(clog) < MaxLog
  /\ pend' = pend \ {c}
  /\ clog' = Append(clog, [name |-> c.name, k |-> c.k, ids |-> c.ids, nodes |-> c.nodes,
                           aterm |-> c.aterm, pterm |-> term])
  /\ UNCHANGED <<term, lai, nrev, rep, initBy, dat, wtag, crashes>>

\* a new leader, in a new term; its applied state may lag
NewTerm ==
  /\ term < MaxTerm
  /\ term' = term + 1
  /\ \E a \in 0..lai : lai' = a
  /\ UNCHANGED <<pend, clog, nrev, rep, initBy, dat, wtag, crashes>>

LeaderApply ==
  /\ lai < Cur /\ lai' = lai + 1
  /\ UNCHANGED <<term, pend, clog, nrev, rep, initBy, dat, wtag, crashes>>

NApply(n) ==
  /\ n \in Managed /\ nrev[n] < Cur
  /\ nrev' = [nrev EXCEPT ![n] = @ + 1]
  /\ UNCHANGED <<term, lai, pend, clog, rep, initBy, dat, wtag, crashes>>

\* the create-only reconciler: replica p of the log set created by entry i
Assigned(n, i, p) ==
  /\ i \in StateAt(nrev[n]).eff
  /\ n \in clog[i].nodes /\ p \in Range(clog[i].ids)

CreateReplica(n, i, p) ==
  /\ i <= nrev[n] /\ Assigned(n, i, p) /\ rep[n][p].origin = "none"
  /\ rep' = [rep EXCEPT ![n][p] = [origin |-> "ctl", set |-> clog[i].name]]
  /\ UNCHANGED <<term, lai, pend, clog, nrev, initBy, dat, wtag, crashes>>

\* the lowest assigned node initializes the group; a repeat after a crash
\* answers AlreadyInitialized and changes nothing
InitGroup(n, i, p) ==
  /\ i <= nrev[n] /\ Assigned(n, i, p) /\ n = Min(clog[i].nodes)
  /\ rep[n][p].origin # "none" /\ initBy[p] = {}
  /\ initBy' = [initBy EXCEPT ![p] = {n}]
  /\ UNCHANGED <<term, lai, pend, clog, nrev, rep, dat, wtag, crashes>>

Crash(n) ==
  /\ crashes < Crashes /\ n \in Managed /\ nrev[n] > 0
  /\ nrev' = [nrev EXCEPT ![n] = 0] /\ crashes' = crashes + 1
  /\ UNCHANGED <<term, lai, pend, clog, rep, initBy, dat, wtag>>

ManualCreate(n, p) ==
  /\ n \notin Managed \/ ManualAnywhere
  /\ rep[n][p].origin = "none"
  /\ rep' = [rep EXCEPT ![n][p] = [origin |-> "manual", set |-> "-"]]
  \* a standalone node's partition is its own; on a managed node, the hand
  \* initialization is of the cluster's group with that id
  /\ initBy' = IF n \in Managed THEN [initBy EXCEPT ![p] = @ \cup {n}] ELSE initBy
  /\ UNCHANGED <<term, lai, pend, clog, nrev, dat, wtag, crashes>>

Write(n, p, w) ==
  /\ wtag[w] = "-" /\ rep[n][p].origin # "none"
  /\ rep[n][p].origin = "manual" \/ initBy[p] # {}
  /\ dat' = [dat EXCEPT ![n][p] = @ \cup {w}]
  /\ wtag' = [wtag EXCEPT ![w] = IF rep[n][p].origin = "ctl" THEN rep[n][p].set ELSE "manual"]
  /\ UNCHANGED <<term, lai, pend, clog, nrev, rep, initBy, crashes>>

Next ==
  \/ \E name \in Names, k \in Counts, S \in SUBSET Managed : Admit(name, k, S)
  \/ \E c \in pend : AppendCmd(c)
  \/ NewTerm \/ LeaderApply
  \/ \E n \in Nodes : NApply(n) \/ Crash(n)
  \/ \E n \in Nodes, i \in 1..Cur, p \in Ids : CreateReplica(n, i, p) \/ InitGroup(n, i, p)
  \/ \E n \in Nodes, p \in Ids : ManualCreate(n, p)
  \/ \E n \in Nodes, p \in Ids, w \in Writes : Write(n, p, w)

Spec == Init /\ [][Next]_vars

Fair ==
  /\ \A n \in Nodes : WF_vars(NApply(n))
  /\ \A n \in Nodes : WF_vars(\E i \in 1..Cur, p \in Ids : CreateReplica(n, i, p) \/ InitGroup(n, i, p))
LiveSpec == Spec /\ Fair

(*************************** properties ************************************)

Final == StateAt(Cur)
Eff == Final.eff

\* A command admitted in one term and appended in another has no effect.
VoidNoEffect == \A i \in 1..Cur : clog[i].aterm # clog[i].pterm => i \notin Eff

\* Every log set satisfies its rules.
RulesHold == \A i \in Eff : RulesOK(clog[i].name, clog[i].k)

\* Distinct log sets have disjoint partitions, all allocated by
\* next_partition, and one name names one log set.
Disjoint ==
  /\ \A i, j \in Eff : i # j => Range(clog[i].ids) \cap Range(clog[j].ids) = {}
  /\ \A i, j \in Eff : i # j => clog[i].name # clog[j].name
  /\ \A i \in Eff : \A p \in Range(clog[i].ids) : p < Final.next

\* Every replica on a managed node was created by reconciling a controller
\* command that took effect, and its id was allocated by next_partition.
CtlOnly ==
  \A n \in Managed, p \in Ids :
    rep[n][p].origin # "none" =>
      /\ rep[n][p].origin = "ctl"
      /\ \E i \in Eff : n \in clog[i].nodes /\ p \in Range(clog[i].ids) /\ rep[n][p].set = clog[i].name

\* No log set's partition holds data from before its CREATE_LOG_SET: every
\* write on a managed replica of its partitions was written to it.
NoInheritedData ==
  \A i \in Eff, n \in Managed : \A p \in Range(clog[i].ids) :
    \A w \in dat[n][p] : wtag[w] = clog[i].name

\* A group is initialized once, by its lowest assigned node.
OneInit == \A i \in Eff : \A p \in Range(clog[i].ids) : initBy[p] \subseteq {Min(clog[i].nodes)}

\* Convergence: every assigned replica exists and every group is initialized.
Converged ==
  \A i \in Eff : \A p \in Range(clog[i].ids) :
    /\ \A n \in clog[i].nodes : rep[n][p].origin = "ctl"
    /\ initBy[p] # {}
Convergence == <>[]Converged

TypeOK == crashes <= Crashes /\ term <= MaxTerm
=============================================================================
