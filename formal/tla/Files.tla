-------------------------------- MODULE Files --------------------------------
(***************************************************************************)
(* Shep §16.2-§16.5: uploads, blobs, attach, copies, links, processing.    *)
(*                                                                         *)
(* One universe, one object store. Content is named by its hash: the       *)
(* bytes "h1" hash to h1. A blob lives at a key derived from its hash and  *)
(* is never deleted. Two actors: alice holds the bytes h1 and can read     *)
(* nodes n1 and n2; mallory holds the bytes h2 and can read only n2. Every *)
(* hash is public (sha256 is a readable field), so mallory "knows" h1.     *)
(* Reading a node stands in for both the read and the update policy of its *)
(* file field. Read access can be revoked (MaxRevoke).                     *)
(*                                                                         *)
(* A bytes upload stages whatever bytes the client holds and sends (a      *)
(* multipart upload is checked by S3 only part by part, so they need not   *)
(* match the declared hash). Completing is two server steps: CompleteCopy  *)
(* verifies the staged object and copies it to the blob key with           *)
(* If-None-Match (a no-op when the blob exists); CompleteCommit moves the  *)
(* upload pending -> complete and deletes the staged copy. A copy upload   *)
(* (begin_upload(from: f)) is complete at once and points at f's blob. An  *)
(* upload's state changes by compare-and-set: pending -> complete |        *)
(* rejected | expired, and never back.                                     *)
(*                                                                         *)
(* Attach is a transaction on the owning node's PG node. It reads the      *)
(* upload's state (AttachBegin) and commits later (AttachCommit). The      *)
(* upload record lives elsewhere, so that read and the commit are not      *)
(* atomic with complete or expiry.                                         *)
(*                                                                         *)
(* A download is an operation that includes the file (a link is minted)   *)
(* followed by using the link, which checks the actor can still read the   *)
(* file (§16.4).                                                           *)
(*                                                                         *)
(* Knobs:                                                                  *)
(*  CopyReadCheck   (design TRUE): begin_upload(from) checks the actor can *)
(*                  read the source.                                       *)
(*  CopyStoredCheck (design TRUE): ... and that the source is stored       *)
(*                  (Uploading -> not_ready, Failed -> its error).         *)
(*  DirectToBlob    (design FALSE): the presigned PUT targets the blob key *)
(*                  with If-None-Match instead of a staging key.           *)
(*  CopyRecheckAtAttach (proposed): a copy's source readability is checked *)
(*                  again when the attach commits.                         *)
(*  Resolver = "push": complete, expiry and rejection move the Uploading   *)
(*                  files attached to the upload that exist at that moment *)
(*                  (the design's "completing marks each file attached").  *)
(*  Resolver = "pull" (proposed): each Uploading file is queued on its own *)
(*                  PG node, and a worker reads the upload's state after   *)
(*                  the attach committed, and moves the file on.           *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS Uploads, Files, NoF, MaxRevoke, NeedsText,
          CopyReadCheck, CopyStoredCheck, DirectToBlob, CopyRecheckAtAttach,
          Resolver

Actors == {"alice", "mallory"}
Hashes == {"h1", "h2"}
Nodes  == {"n1", "n2"}
NoH == "noH"

InitBytes == [a \in Actors |-> IF a = "alice" THEN {"h1"} ELSE {"h2"}]
InitRead  == [a \in Actors |-> IF a = "alice" THEN {"n1", "n2"} ELSE {"n2"}]

Stored  == {"Processing", "Ready"}
Visible == {"Uploading", "Processing", "Ready", "Failed"}   \* attached files

VARIABLES
  blob,      \* blob keys present in the object store
  stage,     \* stage[u]: content of u's staging object, or NoH
  up,        \* upload records
  file,      \* file records
  canRead,   \* canRead[a]: nodes a can read (and update)
  nrev,      \* revocations so far
  bytes,     \* bytes[a]: content a actually holds (initially, or downloaded)
  knows,     \* knows[a]: content a was ever entitled to: bytes it held, or
             \*   content of a stored file it could read at that moment
  attachOK   \* history: every copy attach committed while its actor could
             \*   still read a stored file with that blob, or held the bytes

vars == <<blob, stage, up, file, canRead, nrev, bytes, knows, attachOK>>

FreeUp   == [st |-> "free", who |-> "alice", decl |-> NoH, copy |-> FALSE,
             src |-> NoF, srcSt |-> "none", h |-> NoH]
FreeFile == [st |-> "free", node |-> "n1", upl |-> NoF, seen |-> "none", h |-> NoH]

Init ==
  /\ blob = {}
  /\ stage = [u \in Uploads |-> NoH]
  /\ up = [u \in Uploads |-> FreeUp]
  /\ file = [f \in Files |-> FreeFile]
  /\ canRead = InitRead
  /\ nrev = 0
  /\ bytes = InitBytes
  /\ knows = InitBytes
  /\ attachOK = TRUE

Who(f) == up[file[f].upl].who
AfterStore == IF NeedsText THEN "Processing" ELSE "Ready"
CanReadFile(a, f) == file[f].st \in Visible /\ file[f].node \in canRead[a]
\* a may obtain h right now: holds the bytes, or can read a stored file with it
EntitledNow(a, h) == h \in bytes[a] \/
                     \E g \in Files : CanReadFile(a, g) /\ file[g].st \in Stored /\ file[g].h = h

\* push resolver: the Uploading files of upload u that exist now move on
PushFiles(u, newSt, h) ==
  [f \in Files |-> IF Resolver = "push" /\ file[f].st = "Uploading" /\ file[f].upl = u
                   THEN [file[f] EXCEPT !.st = newSt, !.h = h]
                   ELSE file[f]]

(*************************** begin ****************************************)

BeginBytes(a, u, d) ==
  /\ up[u].st = "free"
  /\ DirectToBlob => d /= NoH                 \* the blob key needs the hash
  /\ up' = [up EXCEPT ![u] = [FreeUp EXCEPT !.st = "pending", !.who = a, !.decl = d]]
  /\ UNCHANGED <<blob, stage, file, canRead, nrev, bytes, knows, attachOK>>

\* what a copy of f points at: its blob, or (if not stored) the hash its
\* upload declared, which may never be stored
SrcHash(f) == IF file[f].st \in Stored THEN file[f].h ELSE up[file[f].upl].decl

\* begin_upload(from: f). A failed check returns an error and creates nothing.
BeginCopy(a, u, f) ==
  /\ up[u].st = "free"
  /\ file[f].st \in Visible
  /\ CopyReadCheck => file[f].node \in canRead[a]
  /\ CopyStoredCheck => file[f].st \in Stored
  /\ up' = [up EXCEPT ![u] = [FreeUp EXCEPT !.st = "complete", !.who = a, !.copy = TRUE,
                              !.src = f, !.srcSt = file[f].st, !.h = SrcHash(f)]]
  /\ knows' = IF CanReadFile(a, f) /\ file[f].st \in Stored
              THEN [knows EXCEPT ![a] = @ \cup {file[f].h}] ELSE knows
  /\ UNCHANGED <<blob, stage, file, canRead, nrev, bytes, attachOK>>

(*************************** send and complete ****************************)

\* The client PUTs bytes b that it holds.
Send(u, b) ==
  /\ up[u].st = "pending" /\ ~ up[u].copy /\ up[u].h = NoH
  /\ b \in bytes[up[u].who]
  /\ IF DirectToBlob
     THEN \* the signature fixes x-amz-checksum-sha256, and If-None-Match
          \* makes the PUT a no-op (412) when the blob exists
          /\ b = up[u].decl /\ b \notin blob
          /\ blob' = blob \cup {b}
          /\ UNCHANGED stage
     ELSE /\ stage[u] = NoH
          /\ stage' = [stage EXCEPT ![u] = b]
          /\ UNCHANGED blob
  /\ UNCHANGED <<up, file, canRead, nrev, bytes, knows, attachOK>>

\* Verify the stored object and copy it to its blob key (If-None-Match).
CompleteCopy(u) ==
  /\ up[u].st = "pending" /\ ~ up[u].copy /\ up[u].h = NoH
  /\ IF DirectToBlob
     THEN \* an object is at the declared hash's key, so size and hash match
          /\ up[u].decl \in blob
          /\ up' = [up EXCEPT ![u].h = up[u].decl]
          /\ UNCHANGED <<blob, stage, file>>
     ELSE /\ stage[u] /= NoH
          /\ IF up[u].decl /= NoH /\ stage[u] /= up[u].decl
             THEN \* checksum_mismatch: rejected, staged copy deleted
                  /\ up' = [up EXCEPT ![u].st = "rejected"]
                  /\ stage' = [stage EXCEPT ![u] = NoH]
                  /\ file' = PushFiles(u, "Failed", NoH)
                  /\ UNCHANGED blob
             ELSE /\ blob' = blob \cup {stage[u]}
                  /\ up' = [up EXCEPT ![u].h = stage[u]]
                  /\ UNCHANGED <<stage, file>>
  /\ UNCHANGED <<canRead, nrev, bytes, knows, attachOK>>

CompleteCommit(u) ==
  /\ up[u].st = "pending" /\ up[u].h /= NoH
  /\ up' = [up EXCEPT ![u].st = "complete"]
  /\ stage' = [stage EXCEPT ![u] = NoH]
  /\ file' = PushFiles(u, AfterStore, up[u].h)
  /\ UNCHANGED <<blob, canRead, nrev, bytes, knows, attachOK>>

\* TTL: a pending upload expires, and its staged copy is deleted.
Expire(u) ==
  /\ up[u].st = "pending"
  /\ up' = [up EXCEPT ![u].st = "expired"]
  /\ stage' = [stage EXCEPT ![u] = NoH]
  /\ file' = PushFiles(u, "Failed", NoH)
  /\ UNCHANGED <<blob, canRead, nrev, bytes, knows, attachOK>>

(*************************** attach ***************************************)

AttachBegin(u, f, n) ==
  /\ file[f].st = "free"
  /\ up[u].st \in {"pending", "complete"}
  /\ n \in canRead[up[u].who]
  /\ file' = [file EXCEPT ![f] = [FreeFile EXCEPT !.st = "attaching", !.node = n,
                                  !.upl = u, !.seen = up[u].st]]
  /\ UNCHANGED <<blob, stage, up, canRead, nrev, bytes, knows, attachOK>>

AttachCommit(f) ==
  LET r == file[f]
      u == r.upl
      a == up[u].who
      srcOK == CanReadFile(a, up[u].src)
  IN
  /\ r.st = "attaching"
  /\ IF r.node \notin canRead[a] \/ (up[u].copy /\ CopyRecheckAtAttach /\ ~ srcOK)
     THEN \* denied: nothing is created
          /\ file' = [file EXCEPT ![f] = FreeFile]
          /\ UNCHANGED attachOK
     ELSE /\ file' = [file EXCEPT ![f] =
                        IF r.seen = "pending"
                        THEN [r EXCEPT !.st = "Uploading", !.seen = "none"]
                        ELSE [r EXCEPT !.st = AfterStore, !.h = up[u].h, !.seen = "none"]]
          /\ attachOK' = (attachOK /\ (up[u].copy => EntitledNow(a, up[u].h)))
  /\ UNCHANGED <<blob, stage, up, canRead, nrev, bytes, knows>>

\* pull resolver: a worker on the file's PG node reads the upload's state
Resolve(f) ==
  LET u == file[f].upl IN
  /\ Resolver = "pull"
  /\ file[f].st = "Uploading"
  /\ up[u].st /= "pending"
  /\ file' = [file EXCEPT ![f] =
                IF up[u].st = "complete" THEN [@ EXCEPT !.st = AfterStore, !.h = up[u].h]
                ELSE [@ EXCEPT !.st = "Failed"]]
  /\ UNCHANGED <<blob, stage, up, canRead, nrev, bytes, knows, attachOK>>

\* processing worker, conditioned on the file's revision
Process(f) ==
  /\ file[f].st = "Processing"
  /\ file' = [file EXCEPT ![f].st = "Ready"]
  /\ UNCHANGED <<blob, stage, up, canRead, nrev, bytes, knows, attachOK>>

Detach(f) ==
  /\ file[f].st \in Visible
  /\ file' = [file EXCEPT ![f].st = "Detached"]
  /\ UNCHANGED <<blob, stage, up, canRead, nrev, bytes, knows, attachOK>>

(*************************** reads, links, revocation *********************)

Revoke(a, n) ==
  /\ nrev < MaxRevoke /\ n \in canRead[a]
  /\ canRead' = [canRead EXCEPT ![a] = @ \ {n}]
  /\ nrev' = nrev + 1
  /\ UNCHANGED <<blob, stage, up, file, bytes, knows, attachOK>>

\* an operation includes f (a link is minted for a), and a uses the link,
\* which is checked again (can_op on f's owner): a now holds the bytes
Download(a, f) ==
  /\ CanReadFile(a, f) /\ file[f].st \in Stored
  /\ file[f].h \in blob
  /\ file[f].h \notin bytes[a]
  /\ bytes' = [bytes EXCEPT ![a] = @ \cup {file[f].h}]
  /\ knows' = [knows EXCEPT ![a] = @ \cup {file[f].h}]
  /\ UNCHANGED <<blob, stage, up, file, canRead, nrev, attachOK>>

Next ==
  \/ \E a \in Actors, u \in Uploads :
        \/ \E d \in Hashes \cup {NoH} : BeginBytes(a, u, d)
        \/ \E f \in Files : BeginCopy(a, u, f)
  \/ \E u \in Uploads : \E b \in Hashes : Send(u, b)
  \/ \E u \in Uploads : CompleteCopy(u) \/ CompleteCommit(u) \/ Expire(u)
  \/ \E u \in Uploads, f \in Files, n \in Nodes : AttachBegin(u, f, n)
  \/ \E f \in Files : AttachCommit(f) \/ Resolve(f) \/ Process(f) \/ Detach(f)
  \/ \E a \in Actors, n \in Nodes : Revoke(a, n)
  \/ \E a \in Actors, f \in Files : Download(a, f)

\* Clients may stop anywhere (never send, never complete, never retry).
\* The server side is fair: TTL expiry, attach commits, resolver, processing.
Fairness ==
  /\ \A u \in Uploads : WF_vars(Expire(u))
  /\ \A f \in Files : WF_vars(AttachCommit(f)) /\ WF_vars(Resolve(f)) /\ WF_vars(Process(f))

Spec == Init /\ [][Next]_vars
LiveSpec == Spec /\ Fairness

Sym == Permutations(Uploads) \cup Permutations(Files)

(*************************** properties ***********************************)

TypeOK ==
  /\ blob \subseteq Hashes
  /\ \A u \in Uploads : up[u].st \in {"free", "pending", "complete", "rejected", "expired"}
  /\ \A f \in Files : file[f].st \in {"free", "attaching", "Detached"} \cup Visible

\* 1. No file in Processing or Ready points to a blob that doesn't exist.
StoredHasBlob == \A f \in Files : file[f].st \in Stored => file[f].h \in blob

\* 2. The same bytes are stored once. Blob keys are derived from the hash, so
\*    there is at most one blob object per hash, and each holds the content
\*    its key names (Send/CompleteCopy only ever write content h at key h).
\*    The only other objects are staging copies, and those exist only while
\*    their upload is pending: completing, rejecting, and expiring delete them.
StoredOnce ==
  \A u \in Uploads : stage[u] /= NoH => up[u].st = "pending"

\* 3. Knowing a hash grants nothing: every attached file points to content
\*    its actor held or could read through some stored file, and every byte
\*    an actor downloaded is content it could read.
NoHashCapability ==
  /\ \A f \in Files : (file[f].st \in Visible \cup {"Detached"} /\ file[f].h /= NoH)
                        => file[f].h \in knows[Who(f)]
  /\ \A a \in Actors : bytes[a] \subseteq knows[a]

\* 3'. Stricter, matching links (§16.4): a copy attaches only while its
\*     actor can still read a stored file with that blob, or holds the bytes.
CopyRespectsRevocation == attachOK

\* 5. A copy never succeeds from a source that isn't stored.
CopyFromStored ==
  \A u \in Uploads : (up[u].copy /\ up[u].st = "complete") => up[u].srcSt \in Stored

Safety == TypeOK /\ StoredHasBlob /\ StoredOnce /\ NoHashCapability /\ CopyFromStored

\* 4. Every attached file eventually leaves Uploading (or is detached).
UploadingResolves ==
  \A f \in Files : (file[f].st = "Uploading") ~> (file[f].st \in {"Ready", "Failed", "Detached"})

\* ... and every stored file eventually becomes Ready.
ProcessingResolves ==
  \A f \in Files : (file[f].st = "Processing") ~> (file[f].st \in {"Ready", "Detached"})
=============================================================================
