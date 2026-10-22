# Findings from the files and live-revocation models

## The models

`Files.tla` models the following (design §16.2–§16.5):
- uploads, staging, complete, and copies (`begin_upload(from:)`)
- attaching before or after complete
- per-universe dedup by hash
- expiry

`LiveRevocation.tla` models live-query revocation (§13.2, §6.3, §16.4):
- membership changes and soft-deleted actors
- agent remaps (`acts_for`) and policy deploys
- the anchor's cached audience
- download links

**What a version is.** A version is a prefix of one partition's log. The
models don't depend on the storage engine.

**How the configs work.** Every config gave its expected result. The failing
configs are kept failing on purpose: each one is the counterexample for a
finding.

## Findings

| # | Severity | Finding | Config |
|---|---|---|---|
| FL1 | high (liveness) | A file attached before its upload completes can stay `Uploading` forever | `Files_push` |
| FL2 | medium (privacy) | A copy outlives revocation of access to its source | `Files_revoke` |
| FL3 | medium (wording) | Routing uses mapped nodes from subscribe time, so remaps aren't followed | `LiveRevocation_staticmap` |
| FL4 | low (consistency) | Deleting an agent's owner closes the agent's stream, but §6.3 doesn't deny the agent | `LiveRevocation_mapclose` |
| FL5 | confirms the design | Clients must never write blob keys; uploads go to staging | `Files_direct` |
| FL6 | confirms the design | Copies need both the readability check and the stored check | `Files_noreadcheck`, `Files_nostoredcheck` |
| FL7 | confirms the design | Routing `@deleted_at`, routing mapping facts, and the deleted-actor link check are all needed | `LiveRevocation_nodelete`, `_noactsfor`, `_linknodel` |

### FL1: attach-before-complete can get stuck

**Trace.**
1. An attach reads the upload as `pending`.
2. Complete runs and moves "each file attached to it", of which there are
   none yet.
3. The attach commits, creating the file in the `Uploading` state.

The upload is now `complete`, so it never expires and complete never runs
again. The file never leaves `Uploading`. Expiry races the same way.

**Fix.**
- Attaching an incomplete upload queues the file in its own partition, inside
  the attaching mutation.
- A worker claims the file and waits for the upload to leave `pending`. It
  then moves the file to `Processing`/`Ready`, or to `Failed`, conditioned on
  the file's revision.
- An upload's state never changes once it leaves `pending`, so the worker
  can't miss the transition.
- Completing an upload only wakes these workers.

The fixed design passes `Files_live` (`UploadingResolves`, under fairness).

### FL2: copies and revocation

**Trace.**
1. mallory copies alice's file `f1` while she can read it.
2. alice detaches `f1`, or mallory's access is revoked.
3. mallory attaches her copy anyway.

A copy upload is effectively a 24-hour capability.

**Fix.** Attaching a copy re-checks, as the actor, that it isn't deleted and
can still read the source. If not, the attach fails with `denied`. This
matches links (§16.4).

The re-check alone also closes the hash-capability hole: `Files_noreadcheck`
passes with it turned on.

### FL3: mapped nodes at the step's snapshot

**Trace.**
1. g1 subscribes, with u1 as its owner.
2. g1 is remapped to u2.
3. u2 is deleted.
4. The router still tracks u1, so m1 is pushed to g1, which now has no viewer.

**Fix.** Track "every node its mappings lead to *at the step's snapshot*".
Routing a change to a mapping's facts re-registers the nodes it leads to.

### FL4: owner deletion

**Trace.** g1's owner is deleted. §13.2 closes g1's stream with `denied`, but
§6.3 only leaves g1 without a viewer, so a fresh query as g1 succeeds.

**Fix.** Only deleting the subscriber's own actor node closes the stream.
Deleting a mapped node leaves the subscriber with no viewer. Phase 2
re-evaluates it, usually resyncing to empty, and restoring the node resyncs
back. `LiveRevocation_resync` passes with this rule.

### FL5: staging keys

If clients could PUT to blob keys with If-None-Match, mallory could declare a
hash she saw, send nothing, and complete. The object at that key would match
the hash, so complete would accept it.

**Rule.** Every presigned PUT targets its upload's own staging key. Only Shep
copies a verified object to its blob key.

### FL6 and FL7: rules confirmed

- **Copy without the readability check:** an unreadable file's bytes leak.
- **Copy without the stored check:** the copy points at a blob that never
  exists.
- **Routing that ignores `@deleted_at` or `acts_for` changes:** pushes reach
  deleted or remapped actors.
- **Links without the deleted-actor check:** links work after deletion. Soft
  delete keeps the membership edges, so `can_op`'s policy part still admits
  the deleted actor.

All three live-query leaks share one path: the batched lookup and the cached
anchor audience come from edges and `viewer_of`, and soft delete changes
neither. The anchor's cached audience must be kept current by the same routed
facts as `Δcan_read`.

### Assumption (not checked)

A new subscription joins through its topic's loop, and its first resync is at
the topic's version. This is the built-in counterpart of F4's fix.

## Results

| Config | Expected | Got | Distinct states | Time |
|---|---|---|---|---|
| `Files` | pass | pass | 5,788,960 | 35 s |
| `Files_live` | pass | pass | 221,893 | 42 s |
| `Files_push` | fail | `UploadingResolves` | 231 | 1 s |
| `Files_revoke` | fail | `CopyRespectsRevocation` | 116,278 | 2 s |
| `Files_noreadcheck` | fail | `NoHashCapability` | 81,535 | 2 s |
| `Files_nostoredcheck` | fail | `CopyFromStored` | 1,496 | 1 s |
| `Files_direct` | fail | `NoHashCapability` | 6,611 | 1 s |
| `LiveRevocation` | pass | pass | 3,668,278 | 1 m 54 s |
| `LiveRevocation_resync` | pass | pass | 3,007,378 | 1 m 35 s |
| `LiveRevocation_nodelete` | fail | `PushSafe` | 38,437 | 3 s |
| `LiveRevocation_noactsfor` | fail | `PushSafe` | 20,495 | 4 s |
| `LiveRevocation_staticmap` | fail | `PushSafe` | 1,146,759 | 75 s |
| `LiveRevocation_linknodel` | fail | `LinkSafe` | 9,088 | 1 s |
| `LiveRevocation_mapclose` | fail | `CloseOnlyIfDenied` | 9,500 | 1 s |

### Invariants

**Files:**
- `StoredHasBlob`
- `StoredOnce`
- `NoHashCapability`
- `CopyRespectsRevocation`
- `UploadingResolves`
- `CopyFromStored`

**LiveRevocation:**
- `PushSafe` and `ViewSafe`
- `LinkSafe`
- `PushExact` and `ViewExact`, the differential property
- `CloseOnlyIfDenied`

## Not modelled

- **Multi-partition behavior:** causality across partitions, and quotes from
  other channels.
- **Residual policies:** author and blocked-list rules.
- **Live-query features:**
  - live ranges and paging
  - resume with `since`
  - coalescing
  - `reauth` and token expiry
  - topics keyed by app
- **Registry and policy changes:** schema heads and shadow promotion. Policy
  deploys are modelled only as P1 ↔ P2.
- **Download links:**
  - signatures, expiry, and universe binding
  - the one-minute presigned GET
- **Upload details:**
  - inline uploads and proxy mode
  - multipart checksums
  - media sniffing
  - size limits and quotas
  - the TTL of completed uploads
- **Blob internals:** GC (punted), the timing side channel, and details of
  extraction.
- **Bounds:**
  - Files: 2 actors, 2 hashes, 2 nodes, and at most 3 uploads. 3 uploads with
    3 files didn't finish in 10 minutes.
  - LiveRevocation: 3 actors and at most 5 log entries.

## Token model

| # | Severity | Finding | Evidence |
|---|---|---|---|
| FL8 | confirms | With external tokens, a stream pauses at exp: it pushes nothing and keeps its state until a reauth or the end of the grace period. Links die with their token. | `LiveRevocation_token_gracepush` fails; `LiveRevocation_token` passes (7,128,898 states, 3m56s) |
| FL9 | confirms | Deleting the viewer must close the stream, which resolves FL4. Link use must check the viewer. | `_token_viewernoclose` and `_token_linknocheck` both fail |

FL3, FL4 and FL7's `_noactsfor` are superseded by the token model, and their configs are marked `SUPERSEDED`.

## Downloads (§16.4)

The model:
- **Handles** are front-end soft state.
- **Use:** the files host forwards a request to the front end that minted the handle. After that front end restarts, the request gets a 404, then re-mints with a fresh read.
- **Expiry:** reauth extends a handle; token `exp` is checked with `ExpSkew`.
- **Presigned GET** has a 2ε margin.
- **Revocation:** one access revocation is modelled, under the horizon rule.

| # | Severity | Finding | Config |
|---|---|---|---|
| FD1 | low (wording) | With reauth extension, a live subscription's links outlive both the minting token and the hour. Fix: cap the extension at min(new exp, mint time + 1 h), or reword the text to say "never outlive the token that minted or last extended them". | `_dl_bearer` fails; `_dl_capped` passes |
| FD2 | confirms | A download after access loss happens at most B + one minute later. That needs the check at use and the presign margin. | `_dl_nousecheck`, `_dl_nomargin` fail |
| FD3 | confirms | A handle's exp is capped at the token's exp at mint, and reauth extends it to the token's exp, not to now + 1 h. | `_dl_nocap`, `_dl_reauthhl` fail |
| FD4 | medium | A re-mint must always be a fresh read. Downloads stay bounded either way, because each use checks again, but a handle restored from cached claims exists unchecked. | `_dl_restore` fails; `_dl_restore_dl` passes |
| FD5 | confirms | Both the 404-then-re-mint path and reauth extension are reachable. | witnesses fire |

Results:
- `Files_dl` passes with 45,103,444 distinct states, and `_dl_capped` with 31,437,292.
- The upload regression (`Files`) passes with 5,788,960.
- The disk filled before `_direct`, `_live`, `_noreadcheck`, `_nostoredcheck`, `_push` and `_revoke` ran; these need re-running.
