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
