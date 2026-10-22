------------------------------ MODULE CoreMove ------------------------------
(***************************************************************************)
(* Shep design §3 (the WAL is thread-per-core; each core owns its Raft     *)
(* groups and a per-core metadata store) and §9.6 (each node assigns its   *)
(* replicas to cores, as Redpanda's shard_balancer and                     *)
(* shard_placement_table do; entries move through receiving, hosted and    *)
(* obsolete).                                                              *)
(*                                                                         *)
(* One node, one Raft group, one replica of it on the node. The step order *)
(* follows Redpanda (src/v/cluster):                                       *)
(*   shard_placement_table::prepare_transfer   step 1: the destination's   *)
(*       current-state marker, is_complete = false (receiving), revision   *)
(*       = the destination's assignment (shard) revision                   *)
(*   controller_backend::transfer_partition    step 2: shutdown_partition  *)
(*       (the xshard_transfer_state carries only a leader-term hint);      *)
(*       step 3: copy_persistent_state, a set of separate kvstore puts     *)
(*   shard_placement_table::finish_transfer    step 4: the destination's   *)
(*       marker, is_complete = true (hosted); step 5: the source becomes   *)
(*       obsolete IN MEMORY ONLY (its on-disk marker still says hosted,    *)
(*       with the older revision)                                          *)
(*   remove_partition_kvstore_state            the obsolete core removes   *)
(*       its raft keys, then its marker (finish_delete)                    *)
(*   initialize_from_kvstore / ntp_init_data   recovery: the hosted marker *)
(*       with the highest revision is the state; a receiving marker with a *)
(*       higher revision on a surviving core resumes the move; every other *)
(*       marker is obsolete; the revision counter restarts above every     *)
(*       persisted revision, removed cores' stores included                *)
(*   shard_balancer::start + transfer_partition_from_extra_shard: after a  *)
(*       core-count decrease the removed cores' stores are opened as extra *)
(*       stores, a hosted state on one is moved to a surviving core by the *)
(*       same steps, and the extra stores are deleted afterwards. Nothing  *)
(*       else starts until then.                                           *)
(*                                                                         *)
(* Per core, the store holds the group's raft keys (vote, config, start    *)
(* offset; and the log itself if LogPerCore) and its placement marker. A   *)
(* missing key reads as the fresh value.                                   *)
(*                                                                         *)
(* The replica takes part in Raft whenever it is live on some core: it     *)
(* grants votes (persisting <<term, candidate>> before answering), appends,*)
(* applies configuration changes, and advances its start offset. Ghost     *)
(* variables record the highest of each the node has ever acknowledged.    *)
(* A crash can happen between any two steps; a restart may change the core *)
(* count.                                                                  *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
  MaxCores,      \* cores 0..MaxCores-1 can exist
  CoreCounts,    \* core counts a restart may choose
  InitCores,     \* core count at the start
  InitHomes,     \* cores the group may be created on
  RestartMode,   \* how a restart treats cores at or above its core count:
                 \* "refuse" (the implementation): if any of their directories
                 \*   holds group state, startup refuses (CoreRemoved); empty
                 \*   ones are ignored; nothing moves between cores at startup.
                 \* "skip" (fault: the old behaviour): their directories are
                 \*   silently not reopened.
                 \* "merge" (future, with core moves): Redpanda's merge, below.
  Cands,         \* candidates that ask for votes
  MaxTerm, MaxCfg, MaxLog,
  MaxCrashes, MaxMoves,
  LogPerCore,    \* FALSE (Redpanda): one log directory per group on the
                 \*   node, opened by whichever core hosts the replica.
                 \*   TRUE: each core has its own copy of the log
  CopyLog,       \* with LogPerCore: step 3 also copies the log
  \* ---- the faults the negative configs inject (all FALSE in the design) ----
  SkipShutdown,  \* step 2 does not stop the source
  CopyEarly,     \* the vote is copied before step 2
  CleanEarly,    \* the source's state is removed once copied, before step 4
  CleanupLog,    \* obsolete cleanup also deletes the (node-level) log
  MergeDropRecv, \* a restart that changes the core count drops receiving markers
  MergeRecvDone, \* a restart that changes the core count carries a receiving
                 \*   marker on a removed core into a surviving core as complete
  ResumeLead,    \* "none" (the implementation): every instance comes up a
                 \*   follower. "move" (fault): the destination, started with
                 \*   the source's leader-term hint equal to its persisted
                 \*   term, leads in that term without an election.
                 \*   "restart" (fault: leader restore): an instance whose
                 \*   persisted vote is for itself leads in that term.
  Fold           \* "none" (Redpanda): a removed core's store is read as an
                 \*   extra store and its hosted state moved by the same steps.
                 \* "rev": a restart that removes cores folds each removed
                 \*   core's entry into surviving core (r % n), in one atomic
                 \*   step; on a collision the higher marker revision wins.
                 \* "hosted": the same fold; a hosted marker beats a receiving
                 \*   one, then the higher revision wins.

Cores == 0..(MaxCores - 1)
Nil == -1
NoVote == <<0, "-">>
NoMk == [s |-> "none", rev |-> 0]
Mk(s, r) == [s |-> s, rev |-> r]
None == [src |-> Nil]
Self == "self"   \* the node's own replica, as a candidate

Empty == [vote |-> NoVote, cfg |-> Nil, start |-> Nil, log |-> Nil, mk |-> NoMk]
Fresh(r) == [vote |-> NoVote, cfg |-> 0, start |-> 0,
             log |-> IF LogPerCore THEN 0 ELSE Nil, mk |-> Mk("hosted", r)]
RaftKeys == {"vote", "cfg", "start"} \cup (IF LogPerCore THEN {"log"} ELSE {})
CopyKeys == IF LogPerCore /\ ~CopyLog THEN {"vote", "cfg", "start"} ELSE RaftKeys
Wipe(e) == [e EXCEPT !.vote = NoVote, !.cfg = Nil, !.start = Nil, !.log = Nil]

VARIABLES
  ncores,   \* current core count
  store,    \* core -> its kvstore (persisted); cores >= ncores are extra stores
  nlog,     \* length of the group's node-level log (persisted; LogPerCore = FALSE)
  st,       \* core -> in-memory placement: none, receiving, hosted,
            \*   obsolete, cleaning (keys removed, marker not yet), extra
  live,     \* cores running the replica (in memory)
  mv,       \* the transfer in progress (in memory): src, dst, rev, ph, copied
  nextRev,  \* the shard revision counter (in memory)
  crashes, moves,
  granted,  \* ghost: every <<term, candidate>> the node has granted
  hiVote, hiCfg, hiStart, acked,  \* ghost: highest acknowledged
  lead,     \* core -> the term its running instance leads in, 0 if follower
  inst,     \* core -> the id of its running instance (in memory)
  nextInst, \* instance ids (ghost)
  hint,     \* core -> the leader-term hint handed to it by a transfer (in memory)
  ledIn,    \* ghost: every <<term, instance, core>> in which an instance led
  down      \* the last startup refused; the node runs nothing until a restart

vars == <<ncores, store, nlog, st, live, mv, nextRev, crashes, moves,
          granted, hiVote, hiCfg, hiStart, acked, lead, inst, nextInst, hint, ledIn, down>>
ldr == <<lead, inst, nextInst, hint, ledIn, down>>
ghosts == <<granted, hiVote, hiCfg, hiStart, acked>>

LogOf(c) == IF LogPerCore THEN store[c].log ELSE nlog
Max(a, b) == IF a >= b THEN a ELSE b
RevOf(S, c) == S[c].mk.rev
ArgMaxRev(S, X) == CHOOSE c \in X : \A d \in X : RevOf(S, d) <= RevOf(S, c)
HostedIn(S) == {c \in Cores : S[c].mk.s = "hosted"}
\* the core whose state recovery would restore now
Auth == IF HostedIn(store) = {} THEN Nil ELSE ArgMaxRev(store, HostedIn(store))
MaxRevAll(S) == LET R == {RevOf(S, c) : c \in Cores} IN CHOOSE r \in R : \A q \in R : q <= r
\* (merge mode) no removed core's store is left to drain; other modes ignore
\* directories at or above the core count
NoExtra == RestartMode # "merge" \/ \A c \in Cores : c >= ncores => store[c] = Empty
Up == ~down
Hosted == {c \in Cores : st[c] = "hosted"}

TypeOK ==
  /\ ncores \in CoreCounts \cup {InitCores}
  /\ \A c \in Cores :
       /\ store[c].mk.s \in {"none", "receiving", "hosted"}
       /\ store[c].vote \in {NoVote} \cup ((1..MaxTerm) \X (Cands \cup {Self}))
       /\ lead[c] \in 0..MaxTerm /\ hint[c] \in 0..MaxTerm
       /\ store[c].cfg \in {Nil} \cup (0..MaxCfg)
       /\ store[c].start \in {Nil} \cup (0..MaxLog)
       /\ st[c] \in {"none", "receiving", "hosted", "obsolete", "cleaning", "extra"}
  /\ live \subseteq Cores

Init ==
  /\ ncores = InitCores
  /\ \E h \in InitHomes :
       /\ store = [c \in Cores |-> IF c = h THEN Fresh(1) ELSE Empty]
       /\ st = [c \in Cores |-> IF c = h THEN "hosted" ELSE "none"]
  /\ nlog = IF LogPerCore THEN Nil ELSE 0
  /\ live = {}
  /\ mv = None
  /\ nextRev = 2
  /\ crashes = 0 /\ moves = 0
  /\ granted = {} /\ hiVote = 0 /\ hiCfg = 0 /\ hiStart = 0 /\ acked = 0
  /\ lead = [c \in Cores |-> 0] /\ inst = [c \in Cores |-> 0] /\ nextInst = 1
  /\ hint = [c \in Cores |-> 0] /\ ledIn = {} /\ down = FALSE

(*************************** the replica's Raft activity ********************)

\* grant a vote: a term above the persisted one (re-granting the same vote
\* changes nothing). Persisted before the reply.
Vote(c, t, x) ==
  /\ c \in live /\ t > store[c].vote[1]
  /\ store' = [store EXCEPT ![c].vote = <<t, x>>]
  /\ granted' = granted \cup {<<t, x>>}
  /\ hiVote' = Max(hiVote, t)
  /\ lead' = [lead EXCEPT ![c] = 0]   \* a higher term: a leader steps down
  /\ UNCHANGED <<ncores, nlog, st, live, mv, nextRev, crashes, moves, hiCfg, hiStart, acked>>
  /\ UNCHANGED <<inst, nextInst, hint, ledIn, down>>

\* win an election: vote for itself in the next term (persisted), then lead
Elect(c) ==
  /\ c \in live /\ lead[c] = 0 /\ store[c].vote[1] < MaxTerm
  /\ LET t == store[c].vote[1] + 1 IN
       /\ store' = [store EXCEPT ![c].vote = <<t, Self>>]
       /\ granted' = granted \cup {<<t, Self>>}
       /\ hiVote' = Max(hiVote, t)
       /\ lead' = [lead EXCEPT ![c] = t]
       /\ ledIn' = ledIn \cup {<<t, inst[c], c>>}
  /\ UNCHANGED <<ncores, nlog, st, live, mv, nextRev, crashes, moves, hiCfg, hiStart, acked>>
  /\ UNCHANGED <<inst, nextInst, hint, down>>

\* append and acknowledge one entry
Append(c) ==
  /\ c \in live /\ LogOf(c) < MaxLog
  /\ IF LogPerCore
       THEN store' = [store EXCEPT ![c].log = @ + 1] /\ UNCHANGED nlog
       ELSE nlog' = nlog + 1 /\ UNCHANGED store
  /\ acked' = LogOf(c) + 1
  /\ UNCHANGED <<ncores, st, live, mv, nextRev, crashes, moves, granted, hiVote, hiCfg, hiStart, ldr>>

\* a configuration change applied and persisted
Reconfig(c) ==
  /\ c \in live /\ store[c].cfg < MaxCfg
  /\ store' = [store EXCEPT ![c].cfg = @ + 1]
  /\ hiCfg' = Max(hiCfg, store[c].cfg + 1)
  /\ UNCHANGED <<ncores, nlog, st, live, mv, nextRev, crashes, moves, granted, hiVote, hiStart, acked, ldr>>

\* prefix truncation: the start offset is persisted, then the prefix's
\* segments may go (so a replica with an older start offset reads a hole)
Truncate(c) ==
  /\ c \in live /\ store[c].start < LogOf(c)
  /\ store' = [store EXCEPT ![c].start = @ + 1]
  /\ hiStart' = Max(hiStart, store[c].start + 1)
  /\ UNCHANGED <<ncores, nlog, st, live, mv, nextRev, crashes, moves, granted, hiVote, hiCfg, acked, ldr>>

(*************************** placement **************************************)

\* reconciliation's create: start the replica on its hosted core. A hosted
\* core that is the source of a transfer only transfers (Redpanda: its
\* reconciliation action is transfer, never create).
StartReplica(c) ==
  /\ NoExtra /\ Up /\ c < ncores
  /\ st[c] = "hosted" /\ c \notin live
  /\ mv.src # c
  /\ live' = live \cup {c}
  \* a fresh instance, opened from the persisted state, comes up a follower;
  \* the hint only makes it start an election at once (Redpanda:
  \* dispatch_vote when leader_term equals the persisted term)
  /\ inst' = [inst EXCEPT ![c] = nextInst]
  /\ nextInst' = nextInst + 1
  /\ hint' = [hint EXCEPT ![c] = 0]
  /\ IF \/ ResumeLead = "move" /\ hint[c] > 0 /\ hint[c] = store[c].vote[1]
        \/ ResumeLead = "restart" /\ store[c].vote[2] = Self
       THEN /\ lead' = [lead EXCEPT ![c] = store[c].vote[1]]
            /\ ledIn' = ledIn \cup {<<store[c].vote[1], nextInst, c>>}
       ELSE /\ lead' = [lead EXCEPT ![c] = 0]
            /\ UNCHANGED ledIn
  /\ UNCHANGED <<ncores, store, nlog, st, mv, nextRev, crashes, moves, ghosts, down>>

\* the balancer picks a new core (set_target) and prepare_transfer commits the
\* destination. Step 1: the receiving marker. The destination must hold no
\* placement state (an obsolete one is cleaned first).
MoveStart(d) ==
  /\ NoExtra /\ Up /\ mv = None /\ moves < MaxMoves
  /\ d < ncores /\ st[d] = "none"
  /\ \E h \in Hosted :
       /\ mv' = [src |-> h, dst |-> d, rev |-> nextRev, ph |-> "prep", copied |-> {}, lt |-> 0]
  /\ store' = [store EXCEPT ![d].mk = Mk("receiving", nextRev)]
  /\ st' = [st EXCEPT ![d] = "receiving"]
  /\ nextRev' = nextRev + 1
  /\ moves' = moves + 1
  /\ UNCHANGED <<ncores, nlog, live, crashes, ghosts, ldr>>

\* after a restart that removed the source's core: clear the chosen surviving
\* destination's store and write its receiving marker
ExtraPrep ==
  /\ mv # None /\ mv.ph = "init"
  /\ store' = [store EXCEPT ![mv.dst] = [Empty EXCEPT !.mk = Mk("receiving", mv.rev)]]
  /\ st' = [st EXCEPT ![mv.dst] = "receiving"]
  /\ mv' = [mv EXCEPT !.ph = "down"]
  /\ UNCHANGED <<ncores, nlog, live, nextRev, crashes, moves, ghosts, ldr>>

\* step 2: shut the source down (it hands over a leader-term hint only)
Shutdown ==
  /\ mv # None /\ mv.ph = "prep"
  /\ live' = IF SkipShutdown THEN live ELSE live \ {mv.src}
  /\ lead' = IF SkipShutdown THEN lead ELSE [lead EXCEPT ![mv.src] = 0]
  /\ mv' = [mv EXCEPT !.ph = "down", !.lt = lead[mv.src]]
  /\ UNCHANGED <<ncores, store, nlog, st, nextRev, crashes, moves, ghosts>>
  /\ UNCHANGED <<inst, nextInst, hint, ledIn, down>>

\* step 3: copy one key (copy_persistent_state is several separate puts)
Copy(k) ==
  /\ mv # None /\ k \notin mv.copied
  /\ \/ mv.ph = "down"
     \/ CopyEarly /\ mv.ph = "prep" /\ k = "vote"
  /\ store' = [store EXCEPT ![mv.dst][k] = store[mv.src][k]]
  /\ mv' = [mv EXCEPT !.copied = @ \cup {k}]
  /\ UNCHANGED <<ncores, nlog, st, live, nextRev, crashes, moves, ghosts, ldr>>

\* steps 4 and 5: the destination's marker becomes complete; the source
\* becomes obsolete in memory
Complete ==
  /\ mv # None /\ mv.ph = "down" /\ mv.copied = CopyKeys
  /\ store' = [store EXCEPT ![mv.dst].mk = Mk("hosted", mv.rev)]
  /\ st' = [st EXCEPT ![mv.dst] = "hosted",
                      ![mv.src] = IF @ \in {"cleaning", "none"} THEN @
                                  ELSE IF mv.src < ncores THEN "obsolete" ELSE "extra"]
  /\ mv' = None
  /\ hint' = [hint EXCEPT ![mv.dst] = mv.lt]
  /\ UNCHANGED <<ncores, nlog, live, nextRev, crashes, moves, ghosts>>
  /\ UNCHANGED <<lead, inst, nextInst, ledIn, down>>

\* the source may be cleaned up before step 4 (fault)
EarlyClean(c) ==
  /\ CleanEarly /\ mv # None /\ c = mv.src /\ c < ncores
  /\ mv.ph = "down" /\ mv.copied = CopyKeys

\* step 5, later: remove the obsolete core's raft keys, then its marker
CleanKeys(c) ==
  /\ NoExtra /\ Up /\ c < ncores
  /\ st[c] = "obsolete" \/ (EarlyClean(c) /\ st[c] = "hosted")
  /\ store' = [store EXCEPT ![c] = Wipe(@)]
  /\ nlog' = IF CleanupLog /\ ~LogPerCore THEN 0 ELSE nlog
  /\ st' = [st EXCEPT ![c] = "cleaning"]
  /\ UNCHANGED <<ncores, live, mv, nextRev, crashes, moves, ghosts, ldr>>

CleanMarker(c) ==
  /\ c < ncores /\ st[c] = "cleaning"
  /\ store' = [store EXCEPT ![c].mk = NoMk]
  /\ st' = [st EXCEPT ![c] = "none"]
  /\ UNCHANGED <<ncores, nlog, live, mv, nextRev, crashes, moves, ghosts, ldr>>

\* the extra stores are deleted once no hosted state remains on them
DropExtra ==
  /\ ~NoExtra /\ Up
  /\ mv = None \/ mv.src < ncores
  /\ \A c \in Cores : c >= ncores => st[c] # "hosted"
  /\ store' = [c \in Cores |-> IF c >= ncores THEN Empty ELSE store[c]]
  /\ st' = [c \in Cores |-> IF c >= ncores THEN "none" ELSE st[c]]
  /\ UNCHANGED <<ncores, nlog, live, mv, nextRev, crashes, moves, ghosts, ldr>>

\* no placement state survived: reconciliation creates the replica afresh, as
\* for a new replica (Redpanda's initial create). Never enabled in the design.
Recreate(d) ==
  /\ NoExtra /\ Up /\ mv = None /\ d < ncores /\ st[d] = "none"
  /\ \A c \in Cores : st[c] \notin {"hosted", "receiving"}
  /\ store' = [store EXCEPT ![d] = Fresh(nextRev)]
  /\ st' = [st EXCEPT ![d] = "hosted"]
  /\ nextRev' = nextRev + 1
  /\ UNCHANGED <<ncores, nlog, live, mv, crashes, moves, ghosts, ldr>>

(*************************** crash and recovery ****************************)

\* a physical fold of the removed cores' stores into the surviving ones
Rank(e) == IF e.mk.s = "none" THEN 0
           ELSE IF Fold = "hosted" /\ e.mk.s = "hosted" THEN 1000 + e.mk.rev
           ELSE 1 + e.mk.rev
FoldSrc(s, n) == {s} \cup {r \in Cores : r >= n /\ r % n = s}
Winner(E) == CHOOSE e \in E : \A f \in E : Rank(f) <= Rank(e)
Folded(n) == [c \in Cores |-> IF c >= n THEN Empty ELSE Winner({store[r] : r \in FoldSrc(c, n)})]

\* the store after a core-count-changing restart's merge, with the faults
Merged(n) ==
  IF n = ncores THEN store
  ELSE IF Fold # "none" /\ n < ncores THEN Folded(n)
  ELSE [c \in Cores |->
         IF store[c].mk.s # "receiving" THEN store[c]
         ELSE IF MergeDropRecv THEN [store[c] EXCEPT !.mk = NoMk]
         ELSE IF MergeRecvDone /\ c >= n THEN [store[c] EXCEPT !.mk.s = "hosted"]
         ELSE store[c]]

\* what startup reads with n cores: everything (merge), or only the
\* directories below n (skip, and refuse once it has checked the others)
View(n) == IF RestartMode = "merge" THEN Merged(n)
           ELSE [c \in Cores |-> IF c >= n THEN Empty ELSE store[c]]
Refuses(n) == RestartMode = "refuse" /\ \E c \in Cores : c >= n /\ store[c] # Empty

\* A crash loses everything in memory. In refuse mode, a startup whose
\* removed directories hold group state refuses (CoreRemoved) and runs
\* nothing; a later restart may try another core count.
Refuse(n) ==
  /\ crashes < MaxCrashes /\ Refuses(n)
  /\ down' = TRUE
  /\ live' = {} /\ mv' = None
  /\ st' = [c \in Cores |-> "none"]
  /\ lead' = [c \in Cores |-> 0] /\ hint' = [c \in Cores |-> 0]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<ncores, store, nlog, nextRev, moves, ghosts, inst, nextInst, ledIn>>

\* Otherwise the restart (with n cores) rebuilds the placement from what it
\* reads (in merge mode every store, the removed cores' included). d is the
\* surviving core that receives a hosted state found on a removed core.
Restart(n, d) ==
  /\ crashes < MaxCrashes /\ ~Refuses(n)
  /\ LET S   == View(n)
         H   == HostedIn(S)
         hc  == IF H = {} THEN Nil ELSE ArgMaxRev(S, H)
         R   == {c \in Cores : S[c].mk.s = "receiving" /\ hc # Nil
                               /\ RevOf(S, c) > RevOf(S, hc)}
         rc  == IF R = {} THEN Nil ELSE ArgMaxRev(S, R)
         resume == hc # Nil /\ rc # Nil /\ rc < n
         fromExtra == hc # Nil /\ hc >= n /\ ~resume
         top == MaxRevAll(S)
     IN /\ d \in (IF fromExtra THEN 0..(n - 1) ELSE {0})
        /\ store' = IF RestartMode = "merge" THEN S ELSE store
        /\ st' = [c \in Cores |->
                    IF c = hc THEN "hosted"
                    ELSE IF resume /\ c = rc THEN "receiving"
                    ELSE IF S[c].mk.s = "none" THEN "none"
                    ELSE IF c < n THEN "obsolete" ELSE "extra"]
        /\ mv' = IF resume THEN [src |-> hc, dst |-> rc, rev |-> RevOf(S, rc),
                                 ph |-> "down", copied |-> {}, lt |-> 0]
                 ELSE IF fromExtra THEN [src |-> hc, dst |-> d, rev |-> top + 1,
                                         ph |-> "init", copied |-> {}, lt |-> 0]
                 ELSE None
        /\ nextRev' = IF fromExtra THEN top + 2 ELSE top + 1
  /\ ncores' = n
  /\ live' = {}
  /\ lead' = [c \in Cores |-> 0] /\ hint' = [c \in Cores |-> 0]
  /\ crashes' = crashes + 1
  /\ down' = FALSE
  /\ UNCHANGED <<nlog, moves, ghosts, inst, nextInst, ledIn>>

Next ==
  \/ \E c \in Cores, t \in 1..MaxTerm, x \in Cands : Vote(c, t, x)
  \/ \E c \in Cores : Append(c) \/ Reconfig(c) \/ Truncate(c) \/ Elect(c)
  \/ \E c \in Cores : StartReplica(c) \/ MoveStart(c) \/ CleanKeys(c) \/ CleanMarker(c) \/ Recreate(c)
  \/ ExtraPrep \/ Shutdown \/ Complete \/ DropExtra
  \/ \E k \in CopyKeys : Copy(k)
  \/ \E n \in CoreCounts, d \in Cores : Restart(n, d)
  \/ \E n \in CoreCounts : Refuse(n)

Spec == Init /\ [][Next]_vars

Fairness ==
  /\ WF_vars(ExtraPrep) /\ WF_vars(Shutdown) /\ WF_vars(Complete) /\ WF_vars(DropExtra)
  /\ \A k \in CopyKeys : WF_vars(Copy(k))
  /\ \A c \in Cores : WF_vars(StartReplica(c)) /\ WF_vars(CleanKeys(c)) /\ WF_vars(CleanMarker(c))
  /\ WF_vars(\E c \in Cores : Recreate(c))

LiveSpec == Spec /\ Fairness

(*************************** properties ************************************)

\* a node never runs two replicas of the group
NoTwoLive == Cardinality(live) <= 1

\* the replica runs only on the core whose placement is hosted: never on a
\* source the destination has replaced
LiveIsHosted == \A c \in live : st[c] = "hosted"

\* the node never votes for two candidates in one term
VoteSafety == \A v, w \in granted : v[1] = w[1] => v[2] = w[2]

\* the running replica, and the state a recovery would restore now, are never
\* behind what the node has acknowledged
Restorable == ({Auth} \cap Cores) \cup live
VoteNoRegress  == Auth # Nil /\ \A c \in Restorable : store[c].vote[1] >= hiVote
CfgNoRegress   == Auth # Nil /\ \A c \in Restorable : store[c].cfg >= hiCfg
StartNoRegress == Auth # Nil /\ \A c \in Restorable : store[c].start >= hiStart
LogNoRegress   == Auth # Nil /\ \A c \in Restorable : LogOf(c) >= acked

\* a fresh instance never leads in a term in which an earlier instance led:
\* the moved replica leads only by winning an election at a higher term (the
\* raft driver's session ids, term << 32 | per-core counter, rely on it)
LeaderTermFresh == \A a, b \in ledIn : a[1] = b[1] => a[2] = b[2]

\* no group is silently dropped: the group hosted before a restart is hosted
\* after it, or startup refused
NoGroupDropped == down \/ Hosted # {}

\* exactly one hosted placement, the one holding the state; or a transfer
\* from it to a destination marked receiving with a newer revision
PlacementUp ==
  /\ Cardinality(Hosted) = 1
  /\ Hosted = {Auth}
  /\ mv = None => \A c \in Cores : st[c] # "receiving"
  /\ mv # None =>
       /\ mv.src = Auth
       /\ mv.rev > RevOf(store, Auth)
       /\ mv.ph # "init" => /\ st[mv.dst] = "receiving"
                            /\ store[mv.dst].mk = Mk("receiving", mv.rev)
PlacementConsistent == down \/ PlacementUp

\* liveness: a transfer finishes (a crash may roll it back), and the node
\* settles with one running replica and no leftover state
MoveEnds == (mv # None) ~> (mv = None)
Settled == /\ mv = None /\ Cardinality(live) = 1 /\ NoExtra
           /\ \A c \in Cores : st[c] \in {"none", "hosted"}
Settles == <>[](down \/ Settled)

\* ---- reachability (sanity configs; each is expected to FAIL) ----
NeverMoved == \A c \in live : c = 0
NeverResumed == ~(mv # None /\ mv.ph = "down" /\ mv.copied = {} /\ live = {} /\ crashes > 0
                  /\ mv.dst < ncores)
NeverExtraMoved == ~(mv # None /\ mv.src >= ncores /\ mv.copied = CopyKeys)
NeverVotedAfterMove == ~(\E c \in live : c # 0 /\ store[c].vote[1] = MaxTerm
                         /\ \E x \in Cands : <<1, x>> \in granted)
NeverLedAfterMove == ~(\E c \in live : c # 0 /\ lead[c] > 0
                       /\ \E e \in ledIn : e[3] = 0 /\ e[1] < lead[c])
NeverRefused == ~down
NeverShrunk == ~(ncores < InitCores /\ live # {})
NeverCleaned == ~(/\ moves > 0 /\ mv = None /\ live # {} /\ ~(0 \in live)
                  /\ \A c \in Cores : st[c] \in {"none", "hosted"}
                  /\ Cardinality({e \in Cores : store[e] # Empty}) = 1)
=============================================================================
