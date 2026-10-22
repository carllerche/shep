# Findings from the WAL validation model

## What the model covers

`WalValidate.tla` models one partition as an abstract Raft group of 3 replicas:

- terms and elections
- the no-op a new leader appends
- whole-log AppendEntries
- commit through a current-term quorum
- deposed leaders
- truncation and snapshot install

On top of that it models the protocol from §10.3:

- **Proposals** carry a DVV read version `S`, a read set of keys and range predicates, a write set, and an idempotency key.
- **The validation table** is entered at append and tagged with the index and term.
- **The idempotency table** is rebuilt, together with the validation table, by every new leader.
- **The window** moves forward, and proposals behind it get `too_old`.
- **Retries** happen through other front ends while earlier proposals are still in flight.
- **Leases** back a `claim`, with a generation fence and a sweeper run by the leader.

## Invariants

- **Serializable:** each entry's reads at its log position equal its reads at `S`.
- **NoUnseenConflict:** validation catches conflicting entries, uncommitted ones included.
- **TooOldEnforced:** a proposal behind the window gets `too_old`.
- **TablesComplete:** a new leader's tables cover every entry they should.
- **DeposedCommitsNothing:** a deposed leader commits nothing.
- **RepliesDurable:** every reply to a client is durable.
- **KeyCausal:** entries for the same idempotency key are causally ordered.
- **IdemOnce and OneOutcome:** each idempotency key gets one commit and one reported outcome.
- **FenceHolds, NoDoubleLease, CompletedByHolder:** the claim's fence, lease and completion rules hold.
- **Raft:** ElectionSafety, LogSafety, LeaderCompleteness.

## Findings

| # | Severity | Finding | Config |
|---|---|---|---|
| W1 | high (confirms C1) | Write sets must enter the table at append. Entering them at commit lets a phantom commit. | `_c1`, `_claim_c1` |
| W2 | high | A new leader whose log was truncated, or which installed a snapshot, inside the window rebuilds an incomplete table. | `_trunc` (fix: `_truncfix`) |
| W3 | medium-high | A front end that reports a failure on a re-execution bypasses the key, so the client hears "failed" for a call that committed. | `_fail`, `_probe` |
| W4 | medium | Claims never conflict, so only the idempotency check at append stops a retried claim from taking a second node. | `_claim_idem` |
| W5 | low | The idempotency table can't be rebuilt from the log alone, because the 24 h window outlives the log. | reasoning |
| W6 | confirmation | Each of these is necessary: rebuilding on leader change, idempotency checked at append, range predicates, `too_old`, and the lease fence. | `_norebuild`, `_idemcommit`, `_norange`, `_notooold`, `_claim_nofence` |

### Fixes

- **W2.** A leader's window starts no earlier than its first log entry, and proposals below it get `too_old`. Alternatively, replicas never truncate inside the validation window.
- **W3.** A client-visible failure of an attempt is reported through the anchor. The anchor returns the key's outcome if it has one, and otherwise logs the failure under the key. Retryable rejections don't consume the key, and a deposed leader must not answer.
- **W5.** Rebuild the idempotency table from the state machine's snapshot table, plus the log after it.

## Results

| Config | Result | Distinct states | Time |
|---|---|---|---|
| `WalValidate` | pass | 8,817,638 | 5m08s |
| `_terms` | pass | 1,596,224 | 46s |
| `_window` | pass | 1,895,746 | 51s |
| `_truncfix` | pass | 1,614,810 | 56s |
| `_claim` | pass | 9,395,210 | 6m29s |
| `_claim_terms` | pass | 4,559,911 | 3m08s |
| `_claim_retry` | pass | 15,522,123 | 10m27s |
| `_c1` | fail: Serializable | 16,167 | 1s |
| `_norebuild` | fail: Serializable | 14,881 | 1s |
| `_idemcommit` | fail: IdemOnce | 24,608 | 1s |
| `_norange` | fail: Serializable | 19,705 | 1s |
| `_notooold` | fail: Serializable | 61,237 | 1s |
| `_trunc` | fail: Serializable | 253,530 | 5s |
| `_fail` | fail: OneOutcome | 5,146 | 1s |
| `_probe` | fail: OneOutcome | 96,043 | 3s |
| `_claim_nofence` | fail: FenceHolds | 94,835 | 2s |
| `_claim_c1` | fail: Serializable | 556 | 0s |
| `_claim_idem` | fail: IdemOnce | 723 | 0s |

The `_sanity` and `_sanity_claim` configs show that every behaviour the model relies on is reachable.

## Not modelled

- 2PC, registry barriers, splits, and more than one partition
- membership changes
- leases for reads
- real time
- expiry of the idempotency window
- claims with n > 1
- pipelining
- liveness of the work queue

## Follow-up: shep-raft (atomic validate+append, flushed commit, fast path, refusal at receipt)

| # | Severity | Finding | Config |
|---|---|---|---|
| W7 | decision | The void-entry rule is needed only when validation and append are separate steps. When validate+append is one atomic step within one term, the rule never fires and can be dropped. If a validated entry is ever handed to raft separately, raft must refuse any entry whose term differs from the one it was validated in. | `_split` fails; `_split_void` passes; every atomic config passes with VoidRule = FALSE (VoidNeverFires) |
| W8 | high | Commit must count flushed copies only. Counting unflushed appends loses a committed entry when a replica crashes. | `_flush_unflushed` fails; `_flush` passes |
| W9 | high | An idempotency hit on an uncommitted entry must wait for it to commit, and return `not_leader` if the entry is overwritten. | `_hit_now` fails |
| W10 | medium-high | The failure fast path is sound when retries are marked. A client that has retried must drop late direct failures from abandoned attempts. | `_fast` and `_fast_probe` pass; `_fast_nomark` fails |
| W11 | high | NotAppended applies only to a refusal at receipt. LogEntryDiscarded and unknown outcomes must stay Pending. A refused slot left Pending blocks its resend forever. | `_refuse`, `_refuse_terms` and `_refuse_live` pass; `_refuse_discard`, `_refuse_unknown` and `_refuse_live_pending` fail |
| W12 | high | Raft must receive validated entries in validation order: one FIFO queue per leader term, with no reordering across cores. | `_refuse_reorder` fails |
| W13 | wording | A deposed leader commits nothing, but a later leader whose log holds its entries can commit them. This is safe, because each was validated against everything before it in that log. | `_sanity_shep` |

### Counterexamples

**W7, the split-step variant (`_split`).**
1. Leader s1 validates q2 in term 1 and holds it.
2. s1 is deposed, and receives s2's copy of q2 by AppendEntries.
3. s1 is re-elected in term 3 and appends the q2 it was holding.
4. q2 commits twice.

**W8 (`_flush_unflushed`).**
1. s1's no-op is replicated, unflushed, to s3, and committed on that unflushed ack.
2. s1 crashes and loses the no-op.
3. s1 is re-elected, with s2's vote, without the entry.

**W11, LogEntryDiscarded treated as NotAppended (`_refuse_discard`).**
1. A late LogEntryDiscarded arrives for s1's old copy of an entry.
2. Meanwhile s1, re-elected, has a rebuilt table holding s2's copy of the same proposal at that index.
3. The discard drops that slot, so the resend validates afresh and commits a second time.

**W12 (`_refuse_reorder`).** A guard validated before a write into its range is received by raft after the write, so it commits with a stale range read.

### Results

| Config | Result | Distinct states | Time |
|---|---|---|---|
| `WalValidate` (atomic, no void rule) | pass | 12,536,642 | 8m |
| `_terms` | pass | 2,402,961 | 1m |
| `_window` | pass | 2,326,280 | 1m |
| `_split_void` | pass | 21,170,004 | 11m |
| `_flush` | pass | 2,948,541 | 2m |
| `_fast` | pass | 5,302,607 | 5m |
| `_fast_probe` | pass | 200,826 | 7s |
| `_refuse` | pass | 45,862 | 1s |
| `_refuse_terms` | pass | 18,046,256 | 7m |
| `_refuse_live` | pass | 1,274 | 1s |

**To re-run:** `_flush` and `_split` predate the final spec; the disk filled before they could be re-run on it. `_truncfix` and `_claim*` were checked in round 1 only.

**Model assumption:** a client that has retried accepts an unanchored failure only from the attempt it is still waiting on.

**Implementation status (2026-10-22).**
- **W7: the void rule stays.** The code is the `_split` variant, which `_split_void` covers.
  - The proposer reads the term from openraft's metrics watch, which can lag the engine.
  - `client_write_ff` appends in the engine's current term without checking the term the entry was validated in.
  - Dropping the rule would need a conditional append inside the engine.
- **W12 is confirmed.** There is one proposer task per group. It admits a proposal, then awaits `client_write_ff` enqueuing it on the engine's single API channel, before it takes the next message. So the order of validation equals the order of receipt.
- **W8 is confirmed.** The leader counts itself only on `LocalIO` after fdatasync, and followers ack only after the same flush.
- **W9 and W11 match the code.** W10 waits for the front ends.
