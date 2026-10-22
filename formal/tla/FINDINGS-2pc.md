# Findings from the two-phase commit model

## The model

`TwoPhase.tla` models these parts of the design:

- two-phase commit (§8)
- in-doubt resolution (§9.7)
- validation and registry barriers (§10.3)
- splits (§9.6)

It follows the anchor-home design: each attempt gets a fresh transaction ID, the anchor allows at most one commit per idempotency key, and there are deadlines. Materializers look ahead before falling back to forgetting, read intents block only writes, and every prepare records its registry dot.

Each design rule is a constant in the model, so every rule has a config that passes and one that fails. Raft is abstract: an append is a commit. Prepares can be delayed or duplicated. Readers are quantified over every causally closed version.

## Findings

| # | Severity | Finding | Config |
|---|---|---|---|
| 2PC-1 | high | If the home forgets a decision before the deadline, a late prepare plus a slow coordinator can commit an aborted transaction. Rule: keep the decision until every prepared participant has released **and** the deadline has passed. Before that, a lookup for an unknown ID answers `expired`. | `_forgetearly`, `_nodeadline`, `_nodeadline_atomic` |
| 2PC-2 | high | Without lookahead in its own log, a lagging materializer resolves a forgotten commit as uncommitted. Rule: read your own log up to the commit index first, and ask the home only if no release is there. | `_nolookahead` |
| 2PC-3 | medium-high | The barrier check at decide time can't order a release after a barrier. Rule: append a barrier only when no prepare at that partition is unreleased; while waiting, reject new prepares with `stale_registry`. | `_barrierlog`, `_noregcheck`, `_barrierdrain` |
| 2PC-4 | high | If each attempt has its own home, a retry with a different participant set commits twice. Rule: the home is the anchor, fixed by the request and routed through split lineage. | `_firsthome`, `_retry` |
| 2PC-5 | medium | A split can retire a home that still holds a decision. Rule: decision records move to the child that holds the home's lowest group. The drain is eventual, not bounded by the prepare timeout. | `_splitlost`, `_split` |
| 2PC-6 | medium (not checked) | Expiry must be monotone across home leader changes. Rule: the first time the home answers `expired` for an undecided transaction, it appends an abort. Deadlines use the leader's lease-bounded clock. | — |
| 2PC-7 | confirms | Confirmed rules: parts are stamped with the decision dot; prepare dots appear in the decision's context; the home is named in prepares; contention gets a rejection, never a wait; read-only participants hold read intents; duplicate prepares are rejected by ID; completion is guaranteed only under availability. | `_localstamp`, `_noprepdot`, `_twohomes`, `_wait`, `_noreadonly`, `_dup`, `_outage`, `_avail` |
| 2PC-8 | info | A read at a fresh token waits for a crashed coordinator's release, for up to the prepare timeout plus the home's availability. | — |

## Results

All 12 positive configs pass and all 14 negative configs fail as intended. Each takes 9 s of TLC time or less.

| Config | Expected | Got | Distinct states |
|---|---|---|---|
| `TwoPhase` | pass | pass | 4,210 |
| `_readonly` | pass | pass | 4,290 |
| `_skew` | pass | pass | 11,888 |
| `_3p` | pass | pass | 24,422 |
| `_retry` | pass | pass | 177,884 |
| `_split` | pass | pass | 476 |
| `_barrier` | pass | pass | 93,409 |
| `_barrierdrain` | pass | pass | 1,363 |
| `_dup` | pass | pass | 743 |
| `_allv` | pass | pass | 4,210 |
| `_live` | pass | pass | 4,210 |
| `_avail` | pass | pass | 648 |
| `_localstamp` | fail | AtomicVisibility | 152 |
| `_noprepdot` | fail | AtomicVisibility | 137 |
| `_noreadonly` | fail | ReadsValidAtCommit | 14 |
| `_wait` | fail | NoDeadlock | 1,474 |
| `_twohomes` | fail | OneDecision | 136 |
| `_forgetearly` | fail | OneDecision | 188 |
| `_nodeadline` | fail | OneDecision | 71 |
| `_nodeadline_atomic` | fail | AtomicVisibility | 78 |
| `_nolookahead` | fail | ResolveCorrect | 162 |
| `_firsthome` | fail | AtMostOnce | 11,850 |
| `_noregcheck` | fail | RegistryAtDecision | 499 |
| `_barrierlog` | fail | RegistryLogOrder | 1,051 |
| `_splitlost` | fail | OneDecision | 366 |
| `_outage` | fail | IntentsReleased | 648 |

## Not modelled

- **Raft:** leader changes, uncommitted entries, and log truncation.
- **Validation and idempotency windows:** the validation window, `too_old`, and expiry of the idempotency window.
- **Timing:** frontier publication and clock skew.
- **Operations:** `claim`, range predicates, phantoms, and retries after `contended`.
- **Barriers and splits:** at most one barrier per partition. Splits produce a single child.

## Splitting a transaction's home (Split2)

Partition 1 splits into two children:
- **C1** gets g1, the home's lowest group, and with it the decision records.
- **C2** gets g2, A's anchor group.

The idempotency table is handed off by snapshot reference, and a drain runs during the split.

| # | Severity | Finding | Config |
|---|---|---|---|
| 2PC-9 | high | A retired home's transaction ID must route through the lineage to the child that holds the home's lowest group, where the records went. Only new attempts route by anchor. Routing by anchor lets a timeout append a second decision. | `_split2_lookupanchor` (`_atomic`) fails |
| 2PC-10 | high | Every idempotency key must go to the child that holds its anchor group. Filtering the keys this way is required, not optional. | `_split2_idemlowest` fails |
| 2PC-11 | high | The idempotency snapshot must be the table exactly at split entry i. | `_split2_idemsnap` fails |
| 2PC-12 | medium | The drain must reject new prepares. Waiting only for the prepares held when the drain started strands a prepare at the retired parent. | `_split2_drainsnap` fails |
| 2PC-13 | medium | Decisions, releases and timeout aborts must keep committing at p during the drain; otherwise it never ends. | `_split2_drainall` fails |
| 2PC-14 | confirms | Each of these is reachable and resolves correctly: a retry across the split, an expired lookup, a lookup of carried records, a `splitting` rejection, and the drain ending. | `_split2` (10,042,424 states) and `_split2_live` pass; the witnesses fire |

**Pending re-runs.** The disk filled before these regression configs ran: `_noregcheck`, `_outage`, `_readonly`, `_retry`, `_skew`, `_split`, `_splitlost`, `_twohomes`, `_wait`.
