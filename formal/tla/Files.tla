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
(*                                                                         *)
(* DL: the download-link sub-model (design v49 §16.4, §6.3 reauth). "none" *)
(* runs the upload model above; any other value runs only the downloads,   *)
(* below, with the knobs that value selects (DLKnobs).                     *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS Uploads, Files, NoF, MaxRevoke, NeedsText,
          CopyReadCheck, CopyStoredCheck, DirectToBlob, CopyRecheckAtAttach,
          Resolver, DL

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
  attachOK,  \* history: every copy attach committed while its actor could
             \*   still read a stored file with that blob, or held the bytes
  dl         \* the download sub-model's state (DL /= "none")

vars == <<blob, stage, up, file, canRead, nrev, bytes, knows, attachOK, dl>>

FreeUp   == [st |-> "free", who |-> "alice", decl |-> NoH, copy |-> FALSE,
             src |-> NoF, srcSt |-> "none", h |-> NoH]
FreeFile == [st |-> "free", node |-> "n1", upl |-> NoF, seen |-> "none", h |-> NoH]

(*************************** downloads (§16.4) ****************************)
(* One file, one client (app, actor, viewer), front ends FEs. Real time is *)
(* dl.now; every clock reading is within Eps of it (front ends, the files  *)
(* host, the object store; a reading is not monotone).                     *)
(*                                                                         *)
(* A read (an operation that includes the file; a live query's phase 2 for *)
(* the subscription) at front end f mints a handle: soft state in f's      *)
(* memory, [fe, id, exp, tokE, cap, sub], with exp = min(clock + HL, the   *)
(* token's exp) and no WAL write. The link names f. Using a link: the      *)
(* files host forwards to f over shep-rpc; if f restarted (the handle is   *)
(* gone) the link 404s and the client re-mints with a fresh read. If f     *)
(* has it, f rejects it past exp, runs the access check (the horizon:      *)
(* a check at t still grants a revocation committed at r iff t <= r + B,   *)
(* AuthHorizon.tla), and either serves the bytes or redirects to a         *)
(* presigned GET whose expiry is its clock + PL - margin; the object store *)
(* accepts it while its clock is at or before that. Links are bearer       *)
(* capabilities: any link ever minted may be used, by anyone, any time.    *)
(*                                                                         *)
(* The token: the client holds one with exp tok; a check accepts it while  *)
(* a clock reading is at or before tok + ExpSkew. The auth service issues  *)
(* fresh ones (exp = now + TokLife) until it stops (a credential revoked   *)
(* outside Shep). reauth: the subscription's front end extends the         *)
(* subscription's handles to the new token (same claims, §6.3).            *)
(*                                                                         *)
(* Access: one revocation (the viewer removed from the channel, or the     *)
(* actor or viewer deleted) at dl.rev.                                     *)
(***************************************************************************)
\* a second front end only where a handle is re-created on another one
FEs == IF DL = "restore" THEN {"f1", "f2"} ELSE {"f1"}
DNone == -1
DEps == 1
DB == 2           \* the revocation bound B
DPL == 1          \* presigned GET lifetime (one minute)
DHL == 2          \* handle lifetime (one hour)
DTokLife == 2
DExpSkew == 1
DMaxT == 6
DMaxMints == IF DL = "restore" THEN 1 ELSE 2   \* restore: two front ends instead
DMaxRestarts == 1
DMin(a, b) == IF a <= b THEN a ELSE b

\* knobs per scenario (design v49 values; fixes marked)
ExpCapMint  == DL /= "nocap"             \* exp never later than the token's exp
ReauthTo    == CASE DL = "capped"   -> "capped"  \* fix: min(new exp, mint + HL)
                 [] DL = "reauthhl" -> "hl"      \* now + HL, ignoring the token
                 [] OTHER           -> "token"   \* as written: the new token's exp
UseCheck    == DL /= "nousecheck"        \* the access check at each use
Margin      == IF DL = "nomargin" THEN 0 ELSE 2 * DEps
Restore     == DL = "restore"            \* re-mint re-creates the old handle's
                                         \*   claims on another front end, no read

DLInit == [now |-> 0, rev |-> DNone, issuing |-> TRUE, tok |-> DTokLife,
           hs |-> {}, minted |-> {}, pres |-> {}, sub |-> "none", nid |-> 0,
           restarts |-> 0,
           okRev |-> TRUE, okTok |-> TRUE, okBearer |-> TRUE,
           w404 |-> FALSE, wExt |-> FALSE]

\* the horizon: a check at real time t grants unless the revocation is older than B
Grant(t) == dl.rev = DNone \/ t <= dl.rev + DB
\* the token passes a check at t: some clock reading is at or before exp + skew
TokOK(t) == t - DEps <= dl.tok + DExpSkew

\* a download at real time t through handle h
Downloaded(t, h) ==
  [dl EXCEPT !.okRev = @ /\ (dl.rev = DNone \/ t <= dl.rev + DB + DPL),
             !.okTok = @ /\ t <= h.tokE + DEps + DPL,
             !.okBearer = @ /\ t <= h.mintT + DHL + 2 * DEps + DPL]

DTick ==
  /\ dl.now < DMaxT
  /\ dl' = [dl EXCEPT !.now = @ + 1,
                      !.pres = {p \in @ : dl.now + 1 - DEps <= p.exp}]

DRevoke == dl.rev = DNone /\ dl' = [dl EXCEPT !.rev = dl.now]
DStopIssuing == dl.issuing /\ dl' = [dl EXCEPT !.issuing = FALSE]
DRefresh == dl.issuing /\ dl.tok < dl.now + DTokLife /\ dl' = [dl EXCEPT !.tok = dl.now + DTokLife]

DSubscribe(f) ==
  /\ dl.sub = "none" /\ TokOK(dl.now) /\ Grant(dl.now)
  /\ dl' = [dl EXCEPT !.sub = f]

\* a read at f mints a handle (on the subscription's front end, as its
\* phase 2: s); the clock reading now + DEps gives the latest expiry, which
\* dominates
DMint(f) ==
  LET s == dl.sub = f IN
  LET c == dl.now + DEps
      h == [fe |-> f, id |-> dl.nid, tokE |-> dl.tok, mintT |-> dl.now, cap |-> c + DHL,
            sub |-> s, ok |-> TRUE,
            exp |-> IF ExpCapMint THEN DMin(c + DHL, dl.tok) ELSE c + DHL]
  IN /\ dl.nid < DMaxMints
     /\ TokOK(dl.now) /\ Grant(dl.now)
     /\ dl' = [dl EXCEPT !.hs = @ \cup {h}, !.minted = @ \cup {h}, !.nid = @ + 1]

\* reauth: the subscription's front end takes the new token and extends the
\* subscription's handles
DReauth ==
  LET f == dl.sub
      Ext(h) == [h EXCEPT !.tokE = dl.tok,
                          !.exp = CASE ReauthTo = "token"  -> dl.tok
                                    [] ReauthTo = "capped" -> DMin(dl.tok, h.cap)
                                    [] ReauthTo = "hl"     -> dl.now + DEps + DHL]
      Mine(h) == h.fe = f /\ h.sub /\ h.tokE < dl.tok
  IN /\ f /= "none" /\ TokOK(dl.now)
     /\ \E h \in dl.hs : Mine(h)
     /\ dl' = [dl EXCEPT !.hs = {IF Mine(h) THEN Ext(h) ELSE h : h \in @}, !.wExt = TRUE]

\* a front end restarts: its handles and its subscription are gone
DRestart(f) ==
  /\ dl.restarts < DMaxRestarts
  /\ dl' = [dl EXCEPT !.hs = {h \in @ : h.fe /= f},
                      !.sub = IF @ = f THEN "none" ELSE @,
                      !.restarts = @ + 1]

Live(h0) == {h \in dl.hs : h.fe = h0.fe /\ h.id = h0.id}

\* using a link (any link ever minted). The front end's clock reading
\* now - DEps accepts the most; the redirect's expiry uses now + DEps.
DUse(h0) ==
  IF Live(h0) = {}
  THEN \* 404: the client re-mints with a fresh read (DMint)
       /\ ~ dl.w404 /\ dl' = [dl EXCEPT !.w404 = TRUE]
  ELSE LET h == CHOOSE x \in Live(h0) : TRUE IN
       /\ dl.now - DEps <= h.exp
       /\ UseCheck => Grant(dl.now)
       /\ \/ dl' = Downloaded(dl.now, h)                    \* served from cache
          \/ /\ dl' = [dl EXCEPT !.pres = @ \cup {[exp |-> dl.now + DEps + DPL - Margin, h |-> h]}]
             /\ dl.now + DEps + DPL - Margin >= dl.now - DEps

\* the object store serves a presigned GET while its clock is at or before
\* the expiry
DGet(p) ==
  /\ dl.now - DEps <= p.exp
  /\ dl' = Downloaded(dl.now, p.h)

\* Restore (negative): after a 404, the client has another front end g
\* re-create the handle from the old link's claims, without a read
DRestoreH(h0, g) ==
  /\ Restore /\ Live(h0) = {} /\ g /= h0.fe
  /\ LET h == [h0 EXCEPT !.fe = g, !.ok = TokOK(dl.now) /\ Grant(dl.now)] IN
     dl' = [dl EXCEPT !.hs = @ \cup {h}, !.minted = @ \cup {h}]

DLNext ==
  \/ DTick \/ DRevoke \/ DStopIssuing \/ DRefresh \/ DReauth
  \/ \E f \in FEs : DSubscribe(f) \/ DRestart(f) \/ DMint(f)
  \/ \E h0 \in dl.minted : DUse(h0) \/ \E g \in FEs : DRestoreH(h0, g)
  \/ \E p \in dl.pres : DGet(p)

\* No download after the viewer loses access, beyond B (the check's horizon)
\* plus PL (the presigned GET's residual minute).
DownloadRevokeBound == dl.okRev
\* No download after its handle's token (the minting token, or the one the
\* last reauth extended it to) expires, beyond Eps + PL.
DownloadTokenBound == dl.okTok
\* "A link is a bearer capability for up to an hour": no download through a
\* link later than HL after it was minted (+ skew + PL).
LinkBearerHour == dl.okBearer
\* Re-minting never widens access: every handle a front end holds was
\* created by a read that passed the access and token checks, and expires
\* no later than its token.
RemintNoWiden == \A h \in dl.hs : h.ok /\ h.exp <= h.tokE
\* witnesses (expected to be violated: the paths are reachable)
No404 == ~ dl.w404
NoReauthExtension == ~ dl.wExt

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
  /\ dl = DLInit

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
  /\ UNCHANGED <<dl, blob, stage, file, canRead, nrev, bytes, knows, attachOK>>

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
  /\ UNCHANGED <<dl, blob, stage, file, canRead, nrev, bytes, attachOK>>

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
  /\ UNCHANGED <<dl, up, file, canRead, nrev, bytes, knows, attachOK>>

\* Verify the stored object and copy it to its blob key (If-None-Match).
CompleteCopy(u) ==
  /\ up[u].st = "pending" /\ ~ up[u].copy /\ up[u].h = NoH
  /\ IF DirectToBlob
     THEN \* an object is at the declared hash's key, so size and hash match
          /\ up[u].decl \in blob
          /\ up' = [up EXCEPT ![u].h = up[u].decl]
          /\ UNCHANGED <<dl, blob, stage, file>>
     ELSE /\ stage[u] /= NoH
          /\ IF up[u].decl /= NoH /\ stage[u] /= up[u].decl
             THEN \* checksum_mismatch: rejected, staged copy deleted
                  /\ up' = [up EXCEPT ![u].st = "rejected"]
                  /\ stage' = [stage EXCEPT ![u] = NoH]
                  /\ file' = PushFiles(u, "Failed", NoH)
                  /\ UNCHANGED blob
             ELSE /\ blob' = blob \cup {stage[u]}
                  /\ up' = [up EXCEPT ![u].h = stage[u]]
                  /\ UNCHANGED <<dl, stage, file>>
  /\ UNCHANGED <<dl, canRead, nrev, bytes, knows, attachOK>>

CompleteCommit(u) ==
  /\ up[u].st = "pending" /\ up[u].h /= NoH
  /\ up' = [up EXCEPT ![u].st = "complete"]
  /\ stage' = [stage EXCEPT ![u] = NoH]
  /\ file' = PushFiles(u, AfterStore, up[u].h)
  /\ UNCHANGED <<dl, blob, canRead, nrev, bytes, knows, attachOK>>

\* TTL: a pending upload expires, and its staged copy is deleted.
Expire(u) ==
  /\ up[u].st = "pending"
  /\ up' = [up EXCEPT ![u].st = "expired"]
  /\ stage' = [stage EXCEPT ![u] = NoH]
  /\ file' = PushFiles(u, "Failed", NoH)
  /\ UNCHANGED <<dl, blob, canRead, nrev, bytes, knows, attachOK>>

(*************************** attach ***************************************)

AttachBegin(u, f, n) ==
  /\ file[f].st = "free"
  /\ up[u].st \in {"pending", "complete"}
  /\ n \in canRead[up[u].who]
  /\ file' = [file EXCEPT ![f] = [FreeFile EXCEPT !.st = "attaching", !.node = n,
                                  !.upl = u, !.seen = up[u].st]]
  /\ UNCHANGED <<dl, blob, stage, up, canRead, nrev, bytes, knows, attachOK>>

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
  /\ UNCHANGED <<dl, blob, stage, up, canRead, nrev, bytes, knows>>

\* pull resolver: a worker on the file's PG node reads the upload's state
Resolve(f) ==
  LET u == file[f].upl IN
  /\ Resolver = "pull"
  /\ file[f].st = "Uploading"
  /\ up[u].st /= "pending"
  /\ file' = [file EXCEPT ![f] =
                IF up[u].st = "complete" THEN [@ EXCEPT !.st = AfterStore, !.h = up[u].h]
                ELSE [@ EXCEPT !.st = "Failed"]]
  /\ UNCHANGED <<dl, blob, stage, up, canRead, nrev, bytes, knows, attachOK>>

\* processing worker, conditioned on the file's revision
Process(f) ==
  /\ file[f].st = "Processing"
  /\ file' = [file EXCEPT ![f].st = "Ready"]
  /\ UNCHANGED <<dl, blob, stage, up, canRead, nrev, bytes, knows, attachOK>>

Detach(f) ==
  /\ file[f].st \in Visible
  /\ file' = [file EXCEPT ![f].st = "Detached"]
  /\ UNCHANGED <<dl, blob, stage, up, canRead, nrev, bytes, knows, attachOK>>

(*************************** reads, links, revocation *********************)

Revoke(a, n) ==
  /\ nrev < MaxRevoke /\ n \in canRead[a]
  /\ canRead' = [canRead EXCEPT ![a] = @ \ {n}]
  /\ nrev' = nrev + 1
  /\ UNCHANGED <<dl, blob, stage, up, file, bytes, knows, attachOK>>

\* an operation includes f (a link is minted for a), and a uses the link,
\* which is checked again (can_op on f's owner): a now holds the bytes
Download(a, f) ==
  /\ CanReadFile(a, f) /\ file[f].st \in Stored
  /\ file[f].h \in blob
  /\ file[f].h \notin bytes[a]
  /\ bytes' = [bytes EXCEPT ![a] = @ \cup {file[f].h}]
  /\ knows' = [knows EXCEPT ![a] = @ \cup {file[f].h}]
  /\ UNCHANGED <<dl, blob, stage, up, file, canRead, nrev, attachOK>>

UploadNext ==
  \/ \E a \in Actors, u \in Uploads :
        \/ \E d \in Hashes \cup {NoH} : BeginBytes(a, u, d)
        \/ \E f \in Files : BeginCopy(a, u, f)
  \/ \E u \in Uploads : \E b \in Hashes : Send(u, b)
  \/ \E u \in Uploads : CompleteCopy(u) \/ CompleteCommit(u) \/ Expire(u)
  \/ \E u \in Uploads, f \in Files, n \in Nodes : AttachBegin(u, f, n)
  \/ \E f \in Files : AttachCommit(f) \/ Resolve(f) \/ Process(f) \/ Detach(f)
  \/ \E a \in Actors, n \in Nodes : Revoke(a, n)
  \/ \E a \in Actors, f \in Files : Download(a, f)

Next ==
  \/ DL = "none" /\ UploadNext
  \/ DL /= "none" /\ DLNext /\ UNCHANGED <<blob, stage, up, file, canRead, nrev, bytes, knows, attachOK>>

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
