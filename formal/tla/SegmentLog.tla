----------------------------- MODULE SegmentLog -----------------------------
(***************************************************************************)
(* Shep design §3 (the WAL is thread-per-core; each core owns its Raft     *)
(* groups' logs and a per-core metadata store), §9.6, §17 (shep-wal: log   *)
(* storage; shep-raft: one fsync per burst, commit at flushed offsets).    *)
(* The storage layer is copied from Redpanda's src/v/storage: segment_set, *)
(* log_replayer, kvstore. This models ONE Raft group's log on ONE core,    *)
(* plus that core's kvstore, at the level of files and fsyncs, with a      *)
(* crash at every step, including between individual file operations.     *)
(*                                                                         *)
(* Files (a POSIX model).                                                  *)
(*   - An inode has a page-cache content c and a durable content d (each a *)
(*     sequence of records). write/ftruncate change c; fsync sets d := c.  *)
(*   - A directory maps names to inodes. create, unlink and rename change  *)
(*     the current directory dirC; only a directory fsync makes them       *)
(*     durable (dirD := dirC).                                             *)
(*   - A crash keeps, for each name independently, either its current or  *)
(*     its durable entry (POSIX orders nothing between directory           *)
(*     operations without a directory fsync), and gives each surviving     *)
(*     inode an image: if c extends d, any prefix of c at least d, with    *)
(*     possibly a torn record after it; if c is shorter (an unsynced       *)
(*     ftruncate), either size. The image is then durable.                 *)
(*   - Records: a log segment holds batches (entry ids; T is a torn batch, *)
(*     one whose header or payload CRC fails); kvstore files hold kv       *)
(*     records and snapshot records (<<"T">> is torn).                     *)
(*   - Appended vs written: Redpanda's appender buffers batches in memory  *)
(*     before writing them to the OS. A crash loses the buffer, which is   *)
(*     the image that keeps none of the unsynced page cache, so the model  *)
(*     writes appended batches straight to c. appended = written = the end *)
(*     of c; flushed = the end of d.                                       *)
(*                                                                         *)
(* The log. Segments are named <base>-<term>. A new segment starts on a    *)
(* size roll (SegSize batches) or a term change, after an fsync of the     *)
(* previous one (Redpanda's release_appender). One batch per offset.       *)
(*   - Flush: fsync the active segment; everything appended is flushed     *)
(*     (and may be acknowledged to the leader).                            *)
(*   - Suffix truncation at o (Raft log matching; the new leader's term is *)
(*     above every truncated entry's): clear the clean-shutdown marker,    *)
(*     unlink the segments at or above o (back to front), ftruncate the    *)
(*     one that holds the cut, fsync it.                                   *)
(*   - Prefix truncation to s (below the flushed end; Raft snapshots):     *)
(*     persist the start offset s, then unlink whole segments below s.     *)
(*   - Clean shutdown: fsync, then put the marker. As in Redpanda the      *)
(*     marker names the last segment (<base, term>) and its length, and    *)
(*     after a restart appends go to a NEW segment (recovered segments     *)
(*     have no appender), so the marker vouches for a file never written   *)
(*     again. ReopenAfterOpen = TRUE makes appends continue in it.         *)
(*                                                                         *)
(* The kvstore (one per core): vote (the term; Raft's vote record), start  *)
(* offset, clean-shutdown marker. A put appends a record to the current    *)
(* kvstore log segment (created on demand, named by its base offset) and   *)
(* fsyncs it before it is applied and acknowledged. A snapshot writes a    *)
(* temp file, fsyncs it, renames it over the snapshot, fsyncs the          *)
(* directory, then unlinks the kvstore log segments it covers. Startup     *)
(* loads the snapshot, replays the segments that follow it (a hole is      *)
(* fatal, as in Redpanda's kvstore::recover), takes a snapshot, and unlinks *)
(* the replayed segments.                                                  *)
(*                                                                         *)
(* Log recovery (after the kvstore): start = the persisted start offset.   *)
(* Sort the segments by base (then by length, descending: Redpanda's       *)
(* order). The LAST one is rescanned (CRC-validated, torn tail dropped)    *)
(* unless the marker names it, in which case its first <len> batches are   *)
(* trusted. The others are read as they are. Walk the sorted segments from *)
(* start: a segment wholly below start or empty is skipped; one that       *)
(* overlaps what is already kept is dropped (Redpanda's "have newer"); at  *)
(* a gap the walk stops (Gaps = "cut") or keeps going with a hole          *)
(* (Gaps = "keep": Redpanda's "use the segments as is"). Then repair:      *)
(* unlink every segment not kept, ftruncate the rescanned tail, fsync it.  *)
(*                                                                         *)
(* SegDirSync = TRUE adds a directory fsync after creating a segment (log  *)
(* or kvstore; before the first write to it), after the unlinks of a       *)
(* suffix truncation, and after recovery's unlinks. The design as written  *)
(* (and Redpanda) has none of them: SegmentLog_asis.cfg.                   *)
(*                                                                         *)
(* Readers (Readers # {}; the reader configs, without crashes). A reader  *)
(* reads the batches from offset o to the end of the segment that holds o, *)
(* in separate steps: the log's epoch and `truncating` flag (together),    *)
(* the segment's in-memory metadata (size, end, its open file), the file's *)
(* contents through that open file; then it re-checks the epoch. If the    *)
(* flag was set it waits for the log's lock (held by a truncation from its *)
(* first step to its last) and restarts; if the epoch changed it restarts; *)
(* if the file is shorter than the metadata says it reports "segment       *)
(* shorter than its records". A suffix truncation bumps the epoch (and,    *)
(* with TruncFlag = "atomic", sets the flag in the same step), shrinks the *)
(* file (ftruncate, fsync) BEFORE it updates the in-memory size and end,   *)
(* and clears the flag in its last step (a drop guard). It can fail or be  *)
(* cancelled before any of its steps (ErrorPath): the guard clears the     *)
(* flag ("clear"), clears it after resetting the metadata from the files   *)
(* ("reconcile"), or leaves it set ("keep"). A prefix truncation changes   *)
(* the metadata in one step before unlinking whole segments; readers hold  *)
(* segments open, so an unlinked file is still readable.                   *)
(*                                                                         *)
(* Invariants are checked when recovery reads the disk (stored in `rec`).  *)
(* At that point the ghost state is reset to what was recovered, which is  *)
(* durable, so a second crash during recovery must recover exactly the     *)
(* same log and vote: idempotence.                                         *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANTS
  MaxAppend,          \* bound on appended entries (ids 1..MaxAppend)
  MaxTerm,            \* bound on the vote (term)
  SegSize,            \* batches per segment before a size roll
  MaxCrash,           \* bound on crashes (power loss: any step, also when down)
  MaxStop,            \* bound on clean shutdowns
  MaxSnap,            \* bound on background kvstore snapshots
  MaxPrefix,          \* bound on prefix truncations
  SegDirSync,         \* see above (FALSE: the design as written)
  ClearMarker,        \* suffix truncation first clears the marker
  SyncAfterTrunc,     \* suffix truncation fsyncs the cut segment
  PersistStartFirst,  \* prefix truncation persists the start before unlinking
  SnapTmpSync,        \* kvstore snapshot fsyncs the temp file before rename
  SnapDirSync,        \* kvstore snapshot fsyncs the directory after rename
  Rescan,             \* recovery rescans the last segment (unless marked)
  RepairSync,         \* recovery fsyncs the segment whose torn tail it cut
  ReopenAfterOpen,    \* after recovery, appends continue in the last segment
  Gaps,               \* "cut" or "keep" (see above)
  Readers,            \* concurrent readers (model values; {} for crash configs)
  MaxReads,           \* reads each reader completes
  TruncFlag,          \* "atomic": the flag is set with the epoch bump (the fix)
                      \* "separate": set in the next step; "none": no flag
  ErrorPath,          \* "none": truncation never fails; "clear", "reconcile",
                      \* "keep": what the drop guard does on an error
  MaxErr              \* bound on failed or cancelled truncations

ASSUME Gaps \in {"cut", "keep"}
ASSUME TruncFlag \in {"atomic", "separate", "none"}
ASSUME ErrorPath \in {"none", "clear", "reconcile", "keep"}

VARIABLES
  fs,        \* inode -> [c, d]
  dirC,      \* name -> inode, current
  dirD,      \* name -> inode, durable
  pc,        \* program counter
  kret,      \* where a kvstore put / snapshot returns to
  kop,       \* the kvstore record being put
  db,        \* the kvstore's in-memory state [vote, start, mk]
  kvoff,     \* last kvstore record offset applied
  kvact,     \* the kvstore's current log segment (or NoName)
  act,       \* the log's active segment (or NoName)
  lstart,    \* the log's in-memory start offset
  tgt,       \* the offset of the truncation in progress
  rdel,      \* recovery: segments to unlink
  rtr,       \* recovery: <<segment, length>> to cut its torn tail to
  \* ghosts
  hlog,      \* offset -> id: the longest log a recovery may return (the
             \*   appended log, or the pre-truncation log while a suffix
             \*   truncation is in progress)
  dlog,      \* offset -> id: the flushed log (acknowledged)
  gone,      \* ids removed by a completed suffix truncation
  ackVote,   \* the highest acknowledged vote
  ackStart,  \* the highest persisted start offset
  rec,       \* the results of the last recovery
  rcrash,    \* the last crash happened during recovery
  \* bounds
  nextId, crashes, stops, snaps, prefixes,
  \* readers
  mseg,      \* the log's in-memory segments: name -> [ino, sz, g]
  epoch,     \* the log's epoch (bumped by a suffix truncation)
  tflag,     \* the `truncating` flag
  fgen,      \* ghost: inode -> number of times its file was cut
  rdr,       \* reader -> its state
  errs       \* failed truncations so far

fsv   == <<fs, dirC, dirD>>
kvv   == <<kret, kop, db, kvoff, kvact>>
logv  == <<act, lstart, tgt, rdel, rtr>>
ghost == <<hlog, dlog, gone, ackVote, ackStart, rec, rcrash>>
ctr   == <<nextId, crashes, stops, snaps, prefixes>>
rv    == <<mseg, epoch, tflag, fgen, rdr, errs>>
vars  == <<fsv, kvv, logv, ghost, ctr, pc, rv>>

(****************************** names, records ****************************)
T      == 0                    \* a torn batch in a log segment
KT     == <<"T">>              \* a torn record in a kvstore file
NoName == <<"none", 0, 0>>
NoMk   == <<0, 0, 0>>          \* marker <<base, term, len>>; none
SnapN  == <<"snap", 0, 0>>
TmpN   == <<"tmp", 0, 0>>
SegN(b, t) == <<"s", b, t>>
KvN(b)     == <<"k", b, 0>>
Base(n)  == n[2]
STerm(n) == n[3]
KRec(key, v1, v2, v3) == <<"R", key, v1, v2, v3>>
DB0 == [vote |-> 0, start |-> 1, mk |-> NoMk]
\* a reader between reads (k reads done, z the last result)
RIdle(k, z) == [st |-> "idle", o |-> 0, e |-> 0, n |-> NoName, ino |-> 0, sz |-> 0,
                mg |-> 0, fg |-> 0, c |-> <<>>, cnt |-> k, res |-> z, rs |-> FALSE]

Max(S) == CHOOSE x \in S : \A y \in S : y <= x
Restrict(f, S) == [x \in DOMAIN f \cap S |-> f[x]]
IsPrefix(a, b) == Len(a) <= Len(b) /\ SubSeq(b, 1, Len(a)) = a

(****************************** files ************************************)
Fresh == IF DOMAIN fs = {} THEN 1 ELSE Max(DOMAIN fs) + 1
Cur(n) == fs[dirC[n]].c
Segs  == {n \in DOMAIN dirC : n[1] = "s"}
KSegs == {n \in DOMAIN dirC : n[1] = "k"}
EndC(n) == Base(n) + Len(Cur(n)) - 1
LEnd == Max({lstart - 1} \cup {EndC(n) : n \in Segs})

Create(n) ==
  LET i == Fresh IN
  /\ fs' = [x \in DOMAIN fs \cup {i} |-> IF x = i THEN [c |-> <<>>, d |-> <<>>] ELSE fs[x]]
  /\ dirC' = [x \in DOMAIN dirC \cup {n} |-> IF x = n THEN i ELSE dirC[x]]
Unlink(n) == dirC' = [x \in DOMAIN dirC \ {n} |-> dirC[x]]
Rename(a, b) == dirC' = [x \in (DOMAIN dirC \ {a}) \cup {b} |-> IF x = b THEN dirC[a] ELSE dirC[x]]
WriteRec(n, r) == fs' = [fs EXCEPT ![dirC[n]].c = Append(@, r)]
Fsync(n) == fs' = [fs EXCEPT ![dirC[n]].d = fs[dirC[n]].c]
Ftrunc(n, k) == fs' = [fs EXCEPT ![dirC[n]].c = SubSeq(@, 1, k)]
DirSync == dirD' = dirC
FG(i) == IF i \in DOMAIN fgen THEN fgen[i] ELSE 0
\* the in-memory metadata the files imply
FilesMeta == [n \in Segs |-> [ino |-> dirC[n], sz |-> Len(Cur(n)), g |-> FG(dirC[n])]]
FilesLog == [o \in lstart..LEnd |->
               LET n == CHOOSE n \in Segs : Base(n) <= o /\ o <= EndC(n)
               IN Cur(n)[o - Base(n) + 1]]

\* crash images of an inode (t: its torn record)
Images(c, d, t) ==
  IF Len(c) >= Len(d)
  THEN {SubSeq(c, 1, k) : k \in Len(d)..Len(c)}
       \cup {SubSeq(c, 1, k) \o <<t>> : k \in Len(d)..(Len(c) - 1)}
  ELSE {c, d}

\* every function g on D with g[x] \in F[x]
RECURSIVE Prod(_, _)
Prod(D, F) ==
  IF D = {} THEN {[x \in {} |-> 0]}
  ELSE LET x == CHOOSE x \in D : TRUE
       IN {[y \in DOMAIN g \cup {x} |-> IF y = x THEN v ELSE g[y]] :
             g \in Prod(D \ {x}, F), v \in F[x]}

(****************************** reading ***********************************)
LogVLen(s) ==
  IF \E j \in 1..Len(s) : s[j] = T
  THEN (CHOOSE j \in 1..Len(s) : s[j] = T /\ \A k \in 1..(j - 1) : s[k] # T) - 1
  ELSE Len(s)
KvVLen(s) ==
  IF \E j \in 1..Len(s) : s[j][1] = "T"
  THEN (CHOOSE j \in 1..Len(s) : s[j][1] = "T" /\ \A k \in 1..(j - 1) : s[k][1] # "T") - 1
  ELSE Len(s)
\* read k batches; past the end of the file is garbage (T)
Take(s, k) == IF Len(s) >= k THEN SubSeq(s, 1, k)
              ELSE s \o [j \in 1..(k - Len(s)) |-> T]

Apply(d, r) ==
  CASE r[2] = "vote"  -> [d EXCEPT !.vote = r[3]]
    [] r[2] = "start" -> [d EXCEPT !.start = r[3]]
    [] r[2] = "mk"    -> [d EXCEPT !.mk = <<r[3], r[4], r[5]>>]
RECURSIVE ApplyAll(_, _, _)
ApplyAll(d, s, i) == IF i > Len(s) THEN d ELSE ApplyAll(Apply(d, s[i]), s, i + 1)

RECURSIVE SortK(_)
SortK(S) == IF S = {} THEN <<>>
            ELSE LET m == CHOOSE m \in S : \A x \in S : Base(m) <= Base(x)
                 IN <<m>> \o SortK(S \ {m})

\* Redpanda's order: base ascending, then dirty offset descending
SegBefore(a, b) ==
  \/ Base(a) < Base(b)
  \/ Base(a) = Base(b) /\ EndC(a) > EndC(b)
  \/ Base(a) = Base(b) /\ EndC(a) = EndC(b) /\ STerm(a) > STerm(b)
RECURSIVE SortSegs(_)
SortSegs(S) == IF S = {} THEN <<>>
               ELSE LET m == CHOOSE m \in S : \A x \in S \ {m} : SegBefore(m, x)
                    IN <<m>> \o SortSegs(S \ {m})

(* The kvstore as startup finds it: [ok, db, off]. *)
SnapOK == SnapN \notin DOMAIN dirC
          \/ (Len(Cur(SnapN)) = 1 /\ Cur(SnapN)[1][1] = "S")
SnapDB == IF SnapN \in DOMAIN dirC
          THEN LET r == Cur(SnapN)[1] IN
               [vote |-> r[2], start |-> r[3], mk |-> <<r[4], r[5], r[6]>>]
          ELSE DB0
SnapOff == IF SnapN \in DOMAIN dirC THEN Cur(SnapN)[1][7] ELSE 0
RECURSIVE Replay(_, _, _, _)
Replay(q, i, nxt, d) ==
  IF i > Len(q) THEN [ok |-> TRUE, db |-> d, off |-> nxt - 1]
  ELSE IF Base(q[i]) # nxt THEN [ok |-> FALSE, db |-> d, off |-> nxt - 1]
  ELSE LET s == Cur(q[i])
           v == SubSeq(s, 1, KvVLen(s))
       IN Replay(q, i + 1, nxt + Len(v), ApplyAll(d, v, 1))
KLoad ==
  IF ~ SnapOK THEN [ok |-> FALSE, db |-> DB0, off |-> 0]
  ELSE LET q == SortK({n \in KSegs : Base(n) > SnapOff}) IN
       IF q = <<>> THEN [ok |-> TRUE, db |-> SnapDB, off |-> SnapOff]
       ELSE Replay(q, 1, SnapOff + 1, SnapDB)

(* The log walk: [end, kept]. rd: what recovery reads from each segment. *)
RECURSIVE Walk(_, _, _, _, _, _)
Walk(q, i, e, kept, rd, st) ==
  IF i > Len(q) THEN [end |-> e, kept |-> kept]
  ELSE LET n  == q[i]
           re == Base(n) + Len(rd[n]) - 1
       IN IF Len(rd[n]) = 0 \/ re < st THEN Walk(q, i + 1, e, kept, rd, st)
          ELSE IF kept # {} /\ Base(n) <= e THEN Walk(q, i + 1, e, kept, rd, st)
          ELSE IF Base(n) > e + 1 /\ Gaps = "cut" THEN [end |-> e, kept |-> kept]
          ELSE Walk(q, i + 1, re, kept \cup {n}, rd, st)

(****************************** kvstore put *******************************)
\* begin a put of record r, then continue at ret
KPut(r, ret) ==
  /\ kop' = r
  /\ kret' = ret
  /\ pc' = IF kvact = NoName THEN "kp_create" ELSE "kp_write"

KpCreate ==
  /\ pc = "kp_create"
  /\ Create(KvN(kvoff + 1))
  /\ kvact' = KvN(kvoff + 1)
  /\ pc' = IF SegDirSync THEN "kp_dsync" ELSE "kp_write"
  /\ UNCHANGED <<dirD, kret, kop, db, kvoff, logv, ghost, ctr>>
KpDsync ==
  /\ pc = "kp_dsync"
  /\ DirSync
  /\ pc' = "kp_write"
  /\ UNCHANGED <<fs, dirC, kvv, logv, ghost, ctr>>
KpWrite ==
  /\ pc = "kp_write"
  /\ WriteRec(kvact, kop)
  /\ pc' = "kp_sync"
  /\ UNCHANGED <<dirC, dirD, kvv, logv, ghost, ctr>>
KpSync ==   \* fsync, apply, acknowledge
  /\ pc = "kp_sync"
  /\ Fsync(kvact)
  /\ db' = Apply(db, kop)
  /\ kvoff' = kvoff + 1
  /\ ackVote' = IF kop[2] = "vote" THEN kop[3] ELSE ackVote
  /\ ackStart' = IF kop[2] = "start" THEN kop[3] ELSE ackStart
  /\ pc' = kret
  /\ UNCHANGED <<dirC, dirD, kret, kop, kvact, logv, hlog, dlog, gone, rec, rcrash, ctr>>

(****************************** kvstore snapshot **************************)
KsCreate ==
  /\ pc = "ks_create"
  /\ Create(TmpN)
  /\ pc' = "ks_write"
  /\ UNCHANGED <<dirD, kvv, logv, ghost, ctr>>
KsWrite ==
  /\ pc = "ks_write"
  /\ WriteRec(TmpN, <<"S", db.vote, db.start, db.mk[1], db.mk[2], db.mk[3], kvoff>>)
  /\ pc' = IF SnapTmpSync THEN "ks_sync" ELSE "ks_rename"
  /\ UNCHANGED <<dirC, dirD, kvv, logv, ghost, ctr>>
KsSync ==
  /\ pc = "ks_sync"
  /\ Fsync(TmpN)
  /\ pc' = "ks_rename"
  /\ UNCHANGED <<dirC, dirD, kvv, logv, ghost, ctr>>
KsAfterDsync == IF KSegs = {} THEN kret ELSE "ks_del"
KsRename ==
  /\ pc = "ks_rename"
  /\ Rename(TmpN, SnapN)
  /\ pc' = IF SnapDirSync THEN "ks_dsync" ELSE KsAfterDsync
  /\ UNCHANGED <<fs, dirD, kvv, logv, ghost, ctr>>
KsDsync ==
  /\ pc = "ks_dsync"
  /\ DirSync
  /\ pc' = KsAfterDsync
  /\ UNCHANGED <<fs, dirC, kvv, logv, ghost, ctr>>
KsDel ==    \* unlink the kvstore log segments the snapshot covers
  /\ pc = "ks_del"
  /\ LET n == CHOOSE n \in KSegs : \A m \in KSegs : Base(n) <= Base(m) IN
     /\ Unlink(n)
     /\ pc' = IF KSegs = {n} THEN kret ELSE "ks_del"
  /\ UNCHANGED <<fs, dirD, kvv, logv, ghost, ctr>>

(****************************** recovery **********************************)
RecPCs == {"rc_log", "rc_rep", "rc_tr", "rc_sync", "rc_dsync"}
KsPCs  == {"ks_create", "ks_write", "ks_sync", "ks_rename", "ks_dsync", "ks_del"}
InRec  == pc \in RecPCs \/ (pc \in KsPCs /\ kret = "rc_log")

\* startup: load the kvstore, then snapshot it (continuing at rc_log)
Start ==
  /\ pc = "down"
  /\ LET L == KLoad IN
     IF ~ L.ok
     THEN /\ pc' = "broken"
          /\ rec' = [rec EXCEPT !.kvok = FALSE]
          /\ UNCHANGED <<fsv, kvv, logv, hlog, dlog, gone, ackVote, ackStart, rcrash, ctr>>
     ELSE /\ db' = L.db
          /\ kvoff' = L.off
          /\ kvact' = NoName
          /\ rec' = [rec EXCEPT !.vote = (L.db.vote >= ackVote)]
          /\ ackVote' = L.db.vote
          /\ kret' = "rc_log"
          /\ pc' = "ks_create"
          /\ UNCHANGED <<fsv, kop, logv, hlog, dlog, gone, ackStart, rcrash, ctr>>

RcAfterTr == IF SegDirSync THEN "rc_dsync" ELSE "up"
RcAfterDel(r) == IF r[1] # NoName THEN "rc_tr" ELSE RcAfterTr

RcLog ==
  /\ pc = "rc_log"
  /\ LET st   == db.start
         q    == SortSegs(Segs)
         last == IF q = <<>> THEN NoName ELSE q[Len(q)]
         skip == db.mk # NoMk /\ last = SegN(db.mk[1], db.mk[2])
         rd   == [n \in Segs |->
                   IF n = last /\ skip THEN Take(Cur(n), db.mk[3])
                   ELSE IF n = last /\ Rescan THEN SubSeq(Cur(n), 1, LogVLen(Cur(n)))
                   ELSE Cur(n)]
         W    == Walk(q, 1, st - 1, {}, rd, st)
         Has(n, o) == Base(n) <= o /\ o <= Base(n) + Len(rd[n]) - 1
         R    == [o \in st..W.end |->
                   IF \E n \in W.kept : Has(n, o)
                   THEN LET n == CHOOSE n \in W.kept : Has(n, o) IN rd[n][o - Base(n) + 1]
                   ELSE T]
         del  == Segs \ W.kept
         tr   == IF last \in W.kept /\ Len(Cur(last)) > Len(rd[last])
                 THEN <<last, Len(rd[last])>> ELSE <<NoName, 0>>
     IN
     /\ rec' = [rec EXCEPT
                  !.start   = (st >= ackStart),
                  !.intact  = \A o \in st..W.end : R[o] # T,
                  !.flushed = \A o \in DOMAIN dlog :
                                o >= st => o \in st..W.end /\ R[o] = dlog[o],
                  !.prefix  = \A o \in st..W.end : o \in DOMAIN hlog /\ R[o] = hlog[o],
                  !.gone    = \A o \in st..W.end : R[o] \notin gone,
                  !.skip    = skip,
                  !.torn    = tr[1] # NoName,
                  !.extra   = \E o \in st..W.end : o \notin DOMAIN dlog,
                  !.dropped = \E n \in del : Len(rd[n]) > 0 /\ Base(n) + Len(rd[n]) - 1 >= st,
                  !.again   = rcrash]
     \* recovered state is durable: it becomes the new baseline
     /\ hlog' = R
     /\ dlog' = R
     /\ ackStart' = st
     /\ lstart' = st
     /\ rdel' = del
     /\ rtr' = tr
     /\ act' = IF ReopenAfterOpen /\ W.kept # {}
               THEN CHOOSE n \in W.kept : \A m \in W.kept : Base(m) <= Base(n)
               ELSE NoName
     /\ rcrash' = FALSE
     /\ mseg' = [n \in W.kept |-> [ino |-> dirC[n], sz |-> Len(rd[n]), g |-> FG(dirC[n])]]
     /\ pc' = IF del # {} THEN "rc_rep" ELSE RcAfterDel(tr)
  /\ UNCHANGED <<fsv, kvv, tgt, gone, ackVote, ctr, epoch, tflag, fgen, rdr, errs>>

RcRep ==
  /\ pc = "rc_rep"
  /\ LET n == CHOOSE n \in rdel : TRUE IN
     /\ Unlink(n)
     /\ rdel' = rdel \ {n}
     /\ pc' = IF rdel = {n} THEN RcAfterDel(rtr) ELSE "rc_rep"
  /\ UNCHANGED <<fs, dirD, kvv, act, lstart, tgt, rtr, ghost, ctr>>
RcTr ==
  /\ pc = "rc_tr"
  /\ Ftrunc(rtr[1], rtr[2])
  /\ pc' = IF RepairSync THEN "rc_sync" ELSE RcAfterTr
  /\ UNCHANGED <<dirC, dirD, kvv, logv, ghost, ctr>>
RcSync ==
  /\ pc = "rc_sync"
  /\ Fsync(rtr[1])
  /\ pc' = RcAfterTr
  /\ UNCHANGED <<dirC, dirD, kvv, logv, ghost, ctr>>
RcDsync ==
  /\ pc = "rc_dsync"
  /\ DirSync
  /\ pc' = "up"
  /\ UNCHANGED <<fs, dirC, kvv, logv, ghost, ctr>>

(****************************** Raft-driven operations *******************)
Vote ==
  /\ pc = "up"
  /\ db.vote < MaxTerm
  /\ KPut(KRec("vote", db.vote + 1, 0, 0), "up")
  /\ UNCHANGED <<fsv, db, kvoff, kvact, logv, ghost, ctr>>

NeedRoll == act = NoName \/ STerm(act) # db.vote \/ Len(Cur(act)) >= SegSize
AppendEntry ==
  /\ pc = "up"
  /\ nextId <= MaxAppend
  /\ db.vote >= 1
  /\ IF NeedRoll
     THEN /\ pc' = IF act # NoName THEN "ap_rsync" ELSE "ap_create"
          /\ UNCHANGED <<fsv, hlog, nextId, mseg>>
     ELSE /\ WriteRec(act, nextId)
          /\ mseg' = [mseg EXCEPT ![act].sz = @ + 1]
          /\ hlog' = [o \in DOMAIN hlog \cup {LEnd + 1} |->
                        IF o = LEnd + 1 THEN nextId ELSE hlog[o]]
          /\ nextId' = nextId + 1
          /\ UNCHANGED <<pc, dirC, dirD>>
  /\ UNCHANGED <<kvv, logv, dlog, gone, ackVote, ackStart, rec, rcrash,
                 crashes, stops, snaps, prefixes, epoch, tflag, fgen, rdr, errs>>
ApRsync ==  \* roll: fsync the previous segment first
  /\ pc = "ap_rsync"
  /\ Fsync(act)
  /\ pc' = "ap_create"
  /\ UNCHANGED <<dirC, dirD, kvv, logv, ghost, ctr>>
ApCreate ==
  /\ pc = "ap_create"
  /\ Create(SegN(LEnd + 1, db.vote))
  /\ act' = SegN(LEnd + 1, db.vote)
  /\ mseg' = [x \in DOMAIN mseg \cup {SegN(LEnd + 1, db.vote)} |->
               IF x = SegN(LEnd + 1, db.vote) THEN [ino |-> Fresh, sz |-> 0, g |-> FG(Fresh)]
               ELSE mseg[x]]
  /\ pc' = IF SegDirSync THEN "ap_dsync" ELSE "ap_write"
  /\ UNCHANGED <<dirD, kvv, lstart, tgt, rdel, rtr, ghost, ctr, epoch, tflag, fgen, rdr, errs>>
ApDsync ==
  /\ pc = "ap_dsync"
  /\ DirSync
  /\ pc' = "ap_write"
  /\ UNCHANGED <<fs, dirC, kvv, logv, ghost, ctr>>
ApWrite ==
  /\ pc = "ap_write"
  /\ WriteRec(act, nextId)
  /\ hlog' = [o \in DOMAIN hlog \cup {LEnd + 1} |->
                IF o = LEnd + 1 THEN nextId ELSE hlog[o]]
  /\ nextId' = nextId + 1
  /\ mseg' = [mseg EXCEPT ![act].sz = @ + 1]
  /\ pc' = "up"
  /\ UNCHANGED <<dirC, dirD, kvv, logv, dlog, gone, ackVote, ackStart, rec, rcrash,
                 crashes, stops, snaps, prefixes, epoch, tflag, fgen, rdr, errs>>

Flush ==    \* fsync the active segment: everything appended is flushed
  /\ pc = "up"
  /\ dlog # hlog
  /\ IF act # NoName THEN Fsync(act) ELSE UNCHANGED fs
  /\ dlog' = hlog
  /\ UNCHANGED <<pc, dirC, dirD, kvv, logv, hlog, gone, ackVote, ackStart, rec, rcrash, ctr>>

(* Suffix truncation at o. *)
\* clear the marker (a kvstore put), then unlink
TrBody ==
  IF ClearMarker /\ db.mk # NoMk
  THEN KPut(KRec("mk", 0, 0, 0), "tr_del")
  ELSE /\ pc' = "tr_del"
       /\ UNCHANGED <<kret, kop>>
\* takes the log's lock, bumps the epoch (and sets the flag)
Truncate ==
  /\ pc = "up"
  /\ \E o \in lstart..LEnd :
       /\ \A n \in Segs : EndC(n) >= o => STerm(n) < db.vote
       /\ tgt' = o
       /\ dlog' = Restrict(dlog, {x \in DOMAIN dlog : x < o})
  /\ epoch' = epoch + 1
  /\ tflag' = (TruncFlag = "atomic")
  /\ IF TruncFlag = "separate"
     THEN /\ pc' = "tr_flag"
          /\ UNCHANGED <<kret, kop>>
     ELSE TrBody
  /\ UNCHANGED <<fsv, db, kvoff, kvact, act, lstart, rdel, rtr,
                 hlog, gone, ackVote, ackStart, rec, rcrash, ctr, mseg, fgen, rdr, errs>>
TrFlag ==   \* TruncFlag = "separate": the flag in its own step
  /\ pc = "tr_flag"
  /\ tflag' = TRUE
  /\ TrBody
  /\ UNCHANGED <<fsv, db, kvoff, kvact, logv, ghost, ctr, mseg, epoch, fgen, rdr, errs>>
Later == {n \in Segs : Base(n) >= tgt}
TrDel ==    \* unlink the later segments, back to front
  /\ pc = "tr_del"
  /\ IF Later = {}
     THEN /\ pc' = "tr_cut"
          /\ UNCHANGED <<dirC, act>>
     ELSE LET n == CHOOSE n \in Later : \A m \in Later : Base(m) <= Base(n) IN
          /\ Unlink(n)
          /\ act' = IF act = n THEN NoName ELSE act
          /\ pc' = IF Later = {n} THEN "tr_cut" ELSE "tr_del"
  /\ UNCHANGED <<fs, dirD, kvv, lstart, tgt, rdel, rtr, ghost, ctr>>
CutSeg == {n \in Segs : Base(n) <= tgt - 1 /\ tgt - 1 <= EndC(n)}
TrCut ==    \* ftruncate the segment that holds the cut
  /\ pc = "tr_cut"
  /\ IF \E n \in CutSeg : EndC(n) >= tgt
     THEN LET n == CHOOSE n \in CutSeg : EndC(n) >= tgt
              i == dirC[n]
          IN /\ Ftrunc(n, tgt - Base(n))
             /\ fgen' = [x \in DOMAIN fgen \cup {i} |-> IF x = i THEN FG(i) + 1 ELSE fgen[x]]
     ELSE UNCHANGED <<fs, fgen>>
  /\ pc' = IF SyncAfterTrunc THEN "tr_sync"
           ELSE IF SegDirSync THEN "tr_dsync" ELSE "tr_meta"
  /\ UNCHANGED <<dirC, dirD, kvv, logv, ghost, ctr, mseg, epoch, tflag, rdr, errs>>
TrSync ==
  /\ pc = "tr_sync"
  /\ IF CutSeg # {} THEN Fsync(CHOOSE n \in CutSeg : TRUE) ELSE UNCHANGED fs
  /\ pc' = IF SegDirSync THEN "tr_dsync" ELSE "tr_meta"
  /\ UNCHANGED <<dirC, dirD, kvv, logv, ghost, ctr>>
TrDsync ==
  /\ pc = "tr_dsync"
  /\ DirSync
  /\ pc' = "tr_meta"
  /\ UNCHANGED <<fs, dirC, kvv, logv, ghost, ctr>>
TrMeta ==   \* the in-memory size and end catch up with the files
  /\ pc = "tr_meta"
  /\ mseg' = [n \in {m \in DOMAIN mseg : Base(m) < tgt} |->
               IF Base(n) + mseg[n].sz - 1 >= tgt
               THEN [mseg[n] EXCEPT !.sz = tgt - Base(n), !.g = FG(mseg[n].ino)]
               ELSE mseg[n]]
  /\ pc' = "tr_done"
  /\ UNCHANGED <<fsv, kvv, logv, ghost, ctr, epoch, tflag, fgen, rdr, errs>>
TrDone ==   \* truncate() returns (the guard clears the flag, the lock is
            \* released): the suffix is gone for good
  /\ pc = "tr_done"
  /\ gone' = gone \cup {hlog[o] : o \in {x \in DOMAIN hlog : x >= tgt}}
  /\ hlog' = Restrict(hlog, {x \in DOMAIN hlog : x < tgt})
  /\ tflag' = FALSE
  /\ pc' = "up"
  /\ UNCHANGED <<fsv, kvv, logv, dlog, ackVote, ackStart, rec, rcrash, ctr,
                 mseg, epoch, fgen, rdr, errs>>
\* the truncation fails or is cancelled before one of its steps
TrError ==
  /\ ErrorPath # "none"
  /\ errs < MaxErr
  /\ pc \in {"tr_flag", "tr_del", "tr_cut", "tr_sync", "tr_dsync", "tr_meta"}
  /\ errs' = errs + 1
  /\ tflag' = IF ErrorPath = "keep" THEN tflag ELSE FALSE
  /\ mseg' = IF ErrorPath = "reconcile" THEN FilesMeta ELSE mseg
  \* what the log now is: the files
  /\ hlog' = FilesLog
  /\ dlog' = Restrict(dlog, DOMAIN FilesLog)
  /\ pc' = "up"
  /\ UNCHANGED <<fsv, kvv, logv, gone, ackVote, ackStart, rec, rcrash, ctr, epoch, fgen, rdr>>

(* Prefix truncation to s. *)
Prefix ==
  /\ pc = "up"
  /\ prefixes < MaxPrefix
  /\ \E s \in (lstart + 1)..(Max({lstart - 1} \cup DOMAIN dlog) + 1) :
       /\ tgt' = s
       /\ IF PersistStartFirst
          THEN KPut(KRec("start", s, 0, 0), "pf_meta")
          ELSE /\ pc' = "pf_meta"
               /\ UNCHANGED <<kret, kop>>
  /\ prefixes' = prefixes + 1
  /\ UNCHANGED <<fsv, db, kvoff, kvact, act, lstart, rdel, rtr, ghost,
                 nextId, crashes, stops, snaps>>
PfMeta ==   \* the metadata changes in one step, before any unlink
  /\ pc = "pf_meta"
  /\ mseg' = Restrict(mseg, {n \in DOMAIN mseg : Base(n) + mseg[n].sz - 1 >= tgt})
  /\ lstart' = tgt
  /\ pc' = "pf_del"
  /\ UNCHANGED <<fsv, kvv, act, tgt, rdel, rtr, ghost, ctr, epoch, tflag, fgen, rdr, errs>>
Below == {n \in Segs : EndC(n) < tgt}
PfAfterDel == IF PersistStartFirst THEN "pf_done" ELSE "pf_put"
PfDel ==    \* unlink whole segments below the new start, front to back
  /\ pc = "pf_del"
  /\ IF Below = {}
     THEN /\ pc' = PfAfterDel
          /\ UNCHANGED <<dirC, act>>
     ELSE LET n == CHOOSE n \in Below : \A m \in Below : Base(n) <= Base(m) IN
          /\ Unlink(n)
          /\ act' = IF act = n THEN NoName ELSE act
          /\ pc' = IF Below = {n} THEN PfAfterDel ELSE "pf_del"
  /\ UNCHANGED <<fs, dirD, kvv, lstart, tgt, rdel, rtr, ghost, ctr>>
PfPut ==
  /\ pc = "pf_put"
  /\ KPut(KRec("start", tgt, 0, 0), "pf_done")
  /\ UNCHANGED <<fsv, db, kvoff, kvact, logv, ghost, ctr>>
PfDone ==
  /\ pc = "pf_done"
  /\ lstart' = tgt
  /\ hlog' = Restrict(hlog, {x \in DOMAIN hlog : x >= tgt})
  /\ dlog' = Restrict(dlog, {x \in DOMAIN dlog : x >= tgt})
  /\ pc' = "up"
  /\ UNCHANGED <<fsv, kvv, act, tgt, rdel, rtr, gone, ackVote, ackStart, rec, rcrash, ctr>>

(* Clean shutdown: fsync, put the marker, stop. *)
LastSeg == CHOOSE n \in Segs : \A m \in Segs : Base(m) <= Base(n)
Stop ==
  /\ pc = "up"
  /\ stops < MaxStop
  /\ IF act # NoName THEN Fsync(act) ELSE UNCHANGED fs
  /\ dlog' = hlog
  /\ act' = NoName
  /\ stops' = stops + 1
  /\ pc' = IF Segs # {} THEN "st_mk" ELSE "down"
  /\ UNCHANGED <<dirC, dirD, kvv, lstart, tgt, rdel, rtr, hlog, gone, ackVote, ackStart,
                 rec, rcrash, nextId, crashes, snaps, prefixes>>
StMk ==
  /\ pc = "st_mk"
  /\ KPut(KRec("mk", Base(LastSeg), STerm(LastSeg), Len(Cur(LastSeg))), "down")
  /\ UNCHANGED <<fsv, db, kvoff, kvact, logv, ghost, ctr>>

(* A background kvstore snapshot. *)
Snap ==
  /\ pc = "up"
  /\ snaps < MaxSnap
  /\ snaps' = snaps + 1
  /\ kvact' = NoName
  /\ kret' = "up"
  /\ pc' = "ks_create"
  /\ UNCHANGED <<fsv, kop, db, kvoff, logv, ghost, nextId, crashes, stops, prefixes>>

(* Power loss, at any step (also while stopped). *)
Crash ==
  /\ crashes < MaxCrash
  /\ pc # "broken"
  /\ DOMAIN dirC \cup DOMAIN dirD # {}
  /\ LET N  == DOMAIN dirC \cup DOMAIN dirD
         Ch == [n \in N |-> (IF n \in DOMAIN dirC THEN {dirC[n]} ELSE {0})
                            \cup (IF n \in DOMAIN dirD THEN {dirD[n]} ELSE {0})]
     IN \E ch \in Prod(N, Ch) :
        LET live == {ch[n] : n \in N} \ {0}
            nd   == [n \in {m \in N : ch[m] # 0} |-> ch[n]]
            Tn(i) == IF \E n \in N : ch[n] = i /\ n[1] = "s" THEN T ELSE KT
            Im   == [i \in live |-> Images(fs[i].c, fs[i].d, Tn(i))]
        IN \E im \in Prod(live, Im) :
           /\ fs' = [i \in live |-> [c |-> im[i], d |-> im[i]]]
           /\ dirC' = nd
           /\ dirD' = nd
  /\ crashes' = crashes + 1
  /\ rcrash' = InRec
  /\ pc' = "down"
  /\ act' = NoName
  /\ kvact' = NoName
  /\ mseg' = [x \in {} |-> 0]
  /\ fgen' = [x \in {} |-> 0]
  /\ tflag' = FALSE
  /\ rdr' = [r \in Readers |-> RIdle(rdr[r].cnt, rdr[r].res)]
  /\ UNCHANGED <<kret, kop, db, kvoff, lstart, tgt, rdel, rtr,
                 hlog, dlog, gone, ackVote, ackStart, rec, nextId, stops, snaps, prefixes,
                 epoch, errs>>

(****************************** readers ***********************************)
KpPCs == {"kp_create", "kp_dsync", "kp_write", "kp_sync"}
\* the log's lock, held by a suffix truncation from its first step to its last
Locked == pc \in {"tr_flag", "tr_del", "tr_cut", "tr_sync", "tr_dsync", "tr_meta", "tr_done"}
          \/ (pc \in KpPCs /\ kret = "tr_del")
Open == pc \notin {"down", "broken", "st_mk"} /\ ~ InRec
MEnd == Max({lstart - 1} \cup {Base(n) + mseg[n].sz - 1 : n \in DOMAIN mseg})

\* read the epoch and the flag together
RStart(r) ==
  /\ rdr[r].st = "idle"
  /\ rdr[r].cnt < MaxReads
  /\ Open
  /\ IF tflag
     THEN rdr' = [rdr EXCEPT ![r].st = "wait"]
     ELSE \E o \in lstart..(MEnd + 1) :
            rdr' = [rdr EXCEPT ![r].st = "meta", ![r].o = o, ![r].e = epoch]
\* wait for the log's lock, then restart
RWait(r) ==
  /\ rdr[r].st = "wait"
  /\ ~ Locked
  /\ rdr' = [rdr EXCEPT ![r] = RIdle(@.cnt, @.res)]
\* the metadata of the segment that holds o
RMeta(r) ==
  /\ rdr[r].st = "meta"
  /\ LET o  == rdr[r].o
          hs == {n \in DOMAIN mseg : Base(n) <= o /\ o <= Base(n) + mseg[n].sz - 1}
     IN IF hs = {}
        THEN rdr' = [rdr EXCEPT ![r] = RIdle(@.cnt + 1, "eof")]   \* past the end
        ELSE LET n == CHOOSE n \in hs : TRUE IN
             rdr' = [rdr EXCEPT ![r].st = "file", ![r].n = n, ![r].ino = mseg[n].ino,
                                ![r].sz = mseg[n].sz, ![r].mg = mseg[n].g]
\* the file's contents, through the open file
RFile(r) ==
  /\ rdr[r].st = "file"
  /\ rdr' = [rdr EXCEPT ![r].st = "check", ![r].c = fs[rdr[r].ino].c,
                         ![r].fg = FG(rdr[r].ino)]
\* re-check the epoch; then return the batches o..end, or an error
RCheck(r) ==
  /\ rdr[r].st = "check"
  /\ LET x  == rdr[r]
          b  == Base(x.n)
          bad == \E k \in x.o..(b + x.sz - 1) :
                   LET v == x.c[k - b + 1] IN
                   v = T \/ v \in gone \/ (k \in DOMAIN hlog /\ hlog[k] # v)
     IN IF epoch # x.e
        THEN rdr' = [rdr EXCEPT ![r] = [RIdle(x.cnt, x.res) EXCEPT !.rs = TRUE]]  \* restart
        ELSE rdr' = [rdr EXCEPT ![r] = RIdle(x.cnt + 1,
                       IF Len(x.c) < x.sz THEN "short"
                       ELSE IF bad THEN "bad"
                       ELSE IF x.mg # x.fg THEN "mixed"
                       ELSE "ok")]
RStep(r) == RStart(r) \/ RWait(r) \/ RMeta(r) \/ RFile(r) \/ RCheck(r)
Read(r) ==
  /\ RStep(r)
  /\ UNCHANGED <<fsv, kvv, logv, ghost, ctr, pc, mseg, epoch, tflag, fgen, errs>>

Init ==
  /\ fs = [x \in {} |-> 0]
  /\ dirC = [x \in {} |-> 0]
  /\ dirD = [x \in {} |-> 0]
  /\ pc = "down"
  /\ kret = "up"
  /\ kop = KRec("vote", 0, 0, 0)
  /\ db = DB0
  /\ kvoff = 0
  /\ kvact = NoName
  /\ act = NoName
  /\ lstart = 1
  /\ tgt = 0
  /\ rdel = {}
  /\ rtr = <<NoName, 0>>
  /\ hlog = [x \in {} |-> 0]
  /\ dlog = [x \in {} |-> 0]
  /\ gone = {}
  /\ ackVote = 0
  /\ ackStart = 1
  /\ rec = [kvok |-> TRUE, vote |-> TRUE, start |-> TRUE, intact |-> TRUE,
            flushed |-> TRUE, prefix |-> TRUE, gone |-> TRUE,
            skip |-> FALSE, torn |-> FALSE, extra |-> FALSE, dropped |-> FALSE,
            again |-> FALSE]
  /\ rcrash = FALSE
  /\ nextId = 1
  /\ crashes = 0
  /\ stops = 0
  /\ snaps = 0
  /\ prefixes = 0
  /\ mseg = [x \in {} |-> 0]
  /\ epoch = 0
  /\ tflag = FALSE
  /\ fgen = [x \in {} |-> 0]
  /\ rdr = [r \in Readers |-> RIdle(0, "none")]
  /\ errs = 0

\* steps that touch no reader state
Plain ==
  \/ KpCreate \/ KpDsync \/ KpWrite \/ KpSync
  \/ KsCreate \/ KsWrite \/ KsSync \/ KsRename \/ KsDsync \/ KsDel
  \/ Start \/ RcRep \/ RcTr \/ RcSync \/ RcDsync
  \/ Vote \/ ApRsync \/ ApDsync \/ Flush
  \/ TrDel \/ TrSync \/ TrDsync
  \/ Prefix \/ PfDel \/ PfPut \/ PfDone
  \/ Stop \/ StMk \/ Snap
\* the steps that continue an operation already started (fair in LiveSpec)
Cont ==
  \/ (\/ KpCreate \/ KpDsync \/ KpWrite \/ KpSync
      \/ KsCreate \/ KsWrite \/ KsSync \/ KsRename \/ KsDsync \/ KsDel
      \/ Start \/ RcRep \/ RcTr \/ RcSync \/ RcDsync
      \/ ApRsync \/ ApDsync \/ TrDel \/ TrSync \/ TrDsync \/ PfDel \/ PfPut \/ PfDone
      \/ StMk) /\ UNCHANGED rv
  \/ RcLog \/ ApCreate \/ ApWrite \/ TrFlag \/ TrCut \/ TrMeta \/ TrDone \/ PfMeta

Next ==
  \/ Plain /\ UNCHANGED rv
  \/ RcLog \/ AppendEntry \/ ApCreate \/ ApWrite
  \/ Truncate \/ TrFlag \/ TrCut \/ TrMeta \/ TrDone \/ TrError \/ PfMeta
  \/ Crash
  \/ \E r \in Readers : Read(r)

Spec == Init /\ [][Next]_vars
LiveSpec == Spec /\ WF_vars(Cont) /\ \A r \in Readers : WF_vars(Read(r))

(****************************** invariants ********************************)
\* the kvstore loads (a corrupt snapshot or a hole in its log stops startup)
KvRecovers == rec.kvok
\* the recovered vote is never older than an acknowledged one
VoteNeverRegresses == rec.vote
\* the recovered start offset never regresses
StartNeverRegresses == rec.start
\* recovery reads no garbage, and nothing below start or missing above it
NoReadIntoDeleted == rec.intact
\* every flushed entry at or above start is recovered
FlushedSurvives == rec.flushed
\* the recovered log is a prefix of what was appended (so every entry above
\* the flushed offset is intact and contiguous)
RecoveredIsAppendedPrefix == rec.prefix
\* an entry a completed suffix truncation removed never comes back
TruncatedNeverReappears == rec.gone

\* readers: a read never combines file contents and metadata from different
\* sides of a truncation (it returns a consistent view, or restarts)
ReadsConsistent == \A r \in Readers : rdr[r].res # "mixed"
\* no spurious "segment shorter than its records"
NoSpuriousShort == \A r \in Readers : rdr[r].res # "short"
\* a read returns the log's entries (never a truncated or torn one)
ReadsCorrect == \A r \in Readers : rdr[r].res # "bad"
\* every reader completes its reads
ReadersProgress == <>(\A r \in Readers : rdr[r].cnt >= MaxReads)
\* while no truncation runs, the in-memory metadata matches the files
MetaMatchesFiles == (pc = "up" /\ ~ tflag) => mseg = FilesMeta

\* model checks: the crash-image model assumes these
FilesPrefixRelated ==
  \A i \in DOMAIN fs : IsPrefix(fs[i].c, fs[i].d) \/ IsPrefix(fs[i].d, fs[i].c)
\* while running, the files hold exactly the appended log
RunningLogMatches ==
  pc = "up" =>
    /\ DOMAIN hlog \subseteq lstart..LEnd
    /\ \A o \in lstart..LEnd :
         /\ o \in DOMAIN hlog
         /\ \E n \in Segs : Base(n) <= o /\ o <= EndC(n) /\ Cur(n)[o - Base(n) + 1] = hlog[o]

(****************************** reachability (sanity) ********************)
(* Each is EXPECTED TO FAIL (run with -continue): the scenario it negates  *)
(* is reachable, so the invariants above are not vacuous.                  *)
NeverSkip            == ~ rec.skip      \* recovery trusted the marker
NeverTornDropped     == ~ rec.torn      \* recovery cut a torn tail
NeverUnflushedKept   == ~ rec.extra     \* recovery kept intact unflushed entries
NeverStaleDropped    == ~ rec.dropped   \* recovery dropped a gap/overlap segment
NeverRecoveryCrash   == ~ rec.again     \* a recovery after a crash during recovery
NeverTruncated       == gone = {}       \* a suffix truncation completed
NeverPrefixTruncated == lstart = 1      \* a prefix truncation completed
NeverRolled          == Cardinality(Segs) < 2
NeverVoteRecovered   == ~ (crashes > 0 /\ pc = "up" /\ ackVote >= 1)
\* readers
NeverReaderWaited    == \A r \in Readers : rdr[r].st # "wait"
NeverEpochRestart    == \A r \in Readers : ~ rdr[r].rs
NeverTruncError      == errs = 0

=============================================================================
