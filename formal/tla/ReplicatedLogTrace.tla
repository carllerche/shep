-------------------------- MODULE ReplicatedLogTrace --------------------------
(***************************************************************************)
(* Scripted behaviours of ReplicatedLog.tla, for counterexamples too deep  *)
(* for breadth-first search over the whole model. Each step is one action *)
(* of ReplicatedLog's Next (the same definitions, with their own guards),  *)
(* with its parameters fixed; TLC still explores the nondeterminism inside *)
(* the actions (a client's read version, a rebuild's floor). The cfgs pair *)
(* each script with the commit-time seeding of the code before the fix     *)
(* (Seed = "tail": expected to violate the invariant) and with the landed  *)
(* fix (Seed = "fix": the script runs to its end, TraceIncomplete is then  *)
(* the invariant reported violated, and the checked property held).        *)
(***************************************************************************)
EXTENDS ReplicatedLog

CONSTANTS s1, s2, s3,
          Script      \* "idem" | "void"

VARIABLE step
tvars == <<vars, step>>

Proc(s, q, rv) == \E p \in net : p.q = q /\ p.rv >= rv /\ Process(s, p)
SendAt(q, rv) == Send(q) /\ \E p \in net' \ net : p.rv = rv

(* "idem": tail-only seeding re-decides a key inside its window.          *)
(* s1 (clock 0) leads term 1 and decides q1 at commit time 0; its clock   *)
(* reaches 2 and it decides q2 at 2 (floor 1), which makes s2 forget q1   *)
(* when it applies q2. s2 (clock 0) leads term 2; the rebuild walk (one   *)
(* entry, WinEntries = 1) stops at q2, so the replayed tail holds no       *)
(* command, and a retry of q1 is decided again at commit time 0.           *)
Idem ==
  CASE step = 0  -> BecomeLeader(s1, {s1, s2})
    [] step = 1  -> Rebuild(s1)
    [] step = 2  -> SendAt("q1", 0)
    [] step = 3  -> Proc(s1, "q1", 0)
    [] step = 4  -> Receipt(s1)
    [] step = 5  -> AE(s1, s2)
    [] step = 6  -> AdvanceCommit(s1)
    [] step = 7  -> Apply(s1)
    [] step = 8  -> Apply(s1)
    [] step = 9  -> Tick(s1)
    [] step = 10 -> Tick(s1)
    [] step = 11 -> SendAt("q2", 2)
    [] step = 12 -> Proc(s1, "q2", 2)
    [] step = 13 -> Receipt(s1)
    [] step = 14 -> AE(s1, s2)
    [] step = 15 -> AdvanceCommit(s1)
    [] step = 16 -> AE(s1, s2)
    [] step = 17 -> Apply(s2)
    [] step = 18 -> Apply(s2)
    [] step = 19 -> Apply(s2)
    [] step = 20 -> BecomeLeader(s2, {s2, s3})
    [] step = 21 -> Rebuild(s2)
    [] step = 22 -> SendAt("q1", 3)
    [] step = 23 -> Proc(s2, "q1", 3)
    [] step = 24 -> Receipt(s2)
    [] step = 25 -> AE(s2, s3)
    [] step = 26 -> AdvanceCommit(s2)
IdemLen == 27

(* "void": with a large WinEntries and no snapshots (today's config),     *)
(* tail-only seeding still goes backwards when the walk stops at an old   *)
(* VOID entry. Clocks: s1 0, s2 2, s3 1. s1 admits q1 in term 1 (commit   *)
(* time 0) and Raft has not received it; s2 leads term 2 and appends q2   *)
(* at 2; s1 leads term 3 and Raft appends q1 under term 3: void, commit   *)
(* time 0, after q2. s3 applies everything and leads term 4: its walk     *)
(* stops at the void entry (0 is before its clock 1 minus AW = 0), the    *)
(* tail holds only its no-op, and it issues commit time 1 after q2's 2.   *)
Void4 ==
  CASE step = 0  -> BecomeLeader(s1, {s1, s2})
    [] step = 1  -> AE(s1, s2)
    [] step = 2  -> AE(s1, s3)
    [] step = 3  -> AdvanceCommit(s1)
    [] step = 4  -> Rebuild(s1)
    [] step = 5  -> SendAt("q1", 0)
    [] step = 6  -> Proc(s1, "q1", 0)
    [] step = 7  -> BecomeLeader(s2, {s2, s3})
    [] step = 8  -> Rebuild(s2)
    [] step = 9  -> SendAt("q2", 1)
    [] step = 10 -> Proc(s2, "q2", 1)
    [] step = 11 -> Receipt(s2)
    [] step = 12 -> AE(s2, s1)
    [] step = 13 -> AE(s2, s3)
    [] step = 14 -> AdvanceCommit(s2)
    [] step = 15 -> BecomeLeader(s1, {s1, s3})
    [] step = 16 -> Receipt(s1)
    [] step = 17 -> AE(s1, s3)
    [] step = 18 -> AdvanceCommit(s1)
    [] step = 19 -> AE(s1, s3)
    [] step \in 20..24 -> Apply(s3)
    [] step = 25 -> BecomeLeader(s3, {s2, s3})
    [] step = 26 -> Rebuild(s3)
    [] step = 27 -> SendAt("q1", 5)
    [] step = 28 -> Proc(s3, "q1", 5)
    [] step = 29 -> Receipt(s3)
VoidLen == 30

Len0 == IF Script = "idem" THEN IdemLen ELSE VoidLen
Clocks0 == IF Script = "idem" THEN [s \in Servers |-> 0]
           ELSE (s1 :> 0 @@ s2 :> 2 @@ s3 :> 1)

TInit == Init /\ clk = Clocks0 /\ step = 0
TNext == /\ step < Len0
         /\ IF Script = "idem" THEN Idem ELSE Void4
         /\ step' = step + 1
TSpec == TInit /\ [][TNext]_tvars

\* violated exactly when the script ran to its end
TraceIncomplete == step < Len0
=============================================================================
