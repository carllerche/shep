# Findings from the TLA+ models

The models are described in [README.md](README.md). Section numbers refer to
`docs/design.md`, and every finding below has since been folded into it.
Findings are ordered by importance. Each one says what the design said, what
the model showed (with a compact counterexample), why it matters, and what
changed.

Trace notation: on a single node, dots are plain numbers. Across nodes, they
are written `A1`, `B2`, and so on. A version is the set of committed dots it
names, such as `{A1, B1}`.

| # | Severity | Area | Finding | Model |
|---|---|---|---|---|
| F1 | critical | §9.4, §10.3 | Reading the version inside the serializable mutation makes every pair of overlapping mutations on a node conflict | `SerialLog` |
| F2 | high (privacy) | §13.2 | A poll response and a post response with the same `since` are both applied; a removed user receives a message | `SlackRT_poll*` |
| F3 | high (liveness) | §12 | A view blocks forever on a context that names a dot on a node it doesn't tail | `ViewMaintainer_untailed` |
| F4 | medium-high | §13.2 | Clients end up holding tokens that cover messages they never received | `SlackRT_failover`, `SlackRT_connect` |
| F5 | medium-high | §9.4, §13.2 | A `since` that isn't the cached result's own version loses changes | `ChangeQueries_handle`, `SlackRT_token` |
| F6 | medium | §9.6 | A read spanning nodes can show an effect without its cause | `MultiNode_read` |
| F7 | medium | §12, §10.4 | A log reader tracking a max-`n` cursor skips entries that commit out of order | `ViewMaintainer_cursor` |
| F8 | medium | §9.2 | Compacting a version up to the node's frontier adds dots the version never saw | `DotFrontier_compact` |
| F9 | medium (liveness) | §9.3, §10.3 | The frontier stalls behind a hung transaction that holds an uncommitted log row | `DotFrontier_hang` |
| F10 | medium | §12 | `changes(view, since V)` is wrong when `V` isn't one of the view's own versions | `ViewMaintainer_changes` |
| F11 | medium | §11.4, §10.4 | Whether a token is in the hot window must be decided from its vector, not its highest dot | `ChangeQueries_window` |
| F12 | low (wording) | §9.5 | A context isn't transitively closed | `MultiNode_closure` |

## F1. Reading the version inside the serializable transaction serializes every write on a node

**What the design said.** §10.3 read the snapshot's version (the `frontier`
row plus the log entries above it) inside the serializable mutation, then
inserted the mutation's own log row into the same range.

**What the model showed.** `SerialLog.tla` uses an ideal certifier that is
more permissive than Postgres's SSI. At 3 transactions it shows:

- no two overlapping mutations ever both commit, even when they touch
  disjoint data;
- dots always commit in dot order.

So the out-of-order commits that §9.2 is built to handle can never happen.
This is the textbook SSI pattern of reading a range and then inserting into
it.

**Why it matters.** Postgres is stricter than the model: its B-tree predicate
locks are page-granular, so new dots always land on the rightmost leaf page.
Per-node writes therefore run one at a time, and each retry burns a dot and a
void. That is worse than the advisory-lock scheme it replaced.

**Fix (§9.4, §10.3).** Never read `mutation_log` or `frontier` inside the
serializable transaction:

- Read `frontier` at READ COMMITTED before `BEGIN`.
- Make `pg_current_snapshot()` the transaction's first statement.
- Store the writer's xid in each log row.
- Resolve the version outside the transaction with `pg_visible_in_snapshot`.

`SerialLog_fix.cfg` shows overlapping and out-of-order commits both
succeeding once this is in place.

## F2. Concurrent poll and post responses double-count the audience (privacy)

**The trace.** A poll and a post are both issued with `since = {}`. Both
responses are applied. Meanwhile an admin removed `u2` (dot 2), before the
message's snapshot. The audience is now `{u1, u2}` again, and `u2` receives
the message.

**Fix (§13.2).** All of a channel's transitions (posts, polls, and listener
joins) run one at a time in its task. A response is applied only if its
`since` equals the current `V_c`.

## F3. A view blocks forever on another node's dot

**The trace.** A client writes `C1`, then writes `A1` with `at_least {C1}`.
A view over nodes A and B never applies `C1`, so it never applies `A1`
either. Every `at_least` read that names `A1` hangs.

**Fix (§12).** The maintainer reads `(dot, context, void)` from every node,
and applies dots from nodes it doesn't query as no-ops.

## F4. Clients end up with tokens that cover messages they never received

There are two causes:

- **Overlapping owners after failover.** Each task pushes only its own
  message.
- **Listeners attaching at a newer snapshot.** A listener that joins at a
  newer snapshot than the task's later receives a push at an older version.

**Fix (§13.2).** Every post and poll also observes `changes(messages in
channel)` and pushes every message in it. Listeners join through the task's
serialized loop.

## F5. `since` must be the cached result's own version

**The trace.** An app's handle-merged token covers its own write, which it
made without `observe`. Polling with that token reports nothing, so the
cache stays wrong.

**Fix (§9.4).** Keep two distinct kinds of token: `at_least` tokens, which
are merged, and `since` versions, which are the version of a specific
result.

## F6. A read spanning nodes can show an effect without its cause

**The trace.** The read's snapshot of node A misses `A1`, and its snapshot
of node B sees `B1`, whose context includes `A1`.

**Fix (§9.6).** Roll such a read back to the largest causally closed subset
of its merged version.

## F7. A max-`n` cursor loses out-of-order commits

**Fix (§10.4, §12).** Every log reader tracks per-node progress as a version,
and rescans the window above the frontier.

## F8. Compaction must not fold up to the frontier

**Fix (§9.2).** Fold only dots that are explicit in the version or void, and
compare versions as sets, not as vectors.

## F9. Frontier liveness needs a bound on open transactions

**Fix (§9.3, §10.3).**

- Require `idle_in_transaction_session_timeout` and `statement_timeout` on
  mutation transactions.
- Make the log insert the transaction's last statement, after `observe`.

With transactions bounded, `DotFrontier_live` shows every allocated dot
eventually falls under the frontier.

## F10. `changes(view, since V)` is wrong for non-linear queries unless V is on the view's chain

**Fix (§12).** `since` must be one of the view's own versions. Any other
version gets a resync.

## F11. Whether a token is in the hot window must be decided per node

**Fix (§11.4, §10.4).** A token is in the window when its vector is at least
the archive horizon on every node. The archiver only archives dots at or
below the frontier.

## F12. A context is not transitively closed

**Fix (§9.5).** This is a wording change: the causal past is the closure of
the context. Causal application checks only each entry's own context.

## Not modelled (reasoning only)

- **U1.** A Postgres failover can lose dots that tokens already name. Fix:
  dots carry an epoch (§9.1).
- **U2.** `since` must be merged into the context (§9.5).
- **U3.** `observe` can only read the mutation's own node (§11.3).
- **U4.** The residual's add and remove sets must be disjoint (§11.3).
- **U5.** "Before the commit" means in version order, not wall-clock order
  (§13.2).
- **U6.** The advancer and the fast-path void run at READ COMMITTED. A
  unique violation on a fast-path void counts as success (§9.3).

## Interpretations chosen

- **I1.** The void timeout is nondeterministic, and safety doesn't depend on
  it.
- **I2.** The advancer voids the first hole, then advances over the
  contiguous resolved prefix.
- **I3.** The advancer knows the counter's last value. Without it, a
  trailing hole is never voided (`DotFrontier_noseq`), which is now stated in
  §9.3.
- **I4.** Versions are modelled by the set of committed dots they denote.
  `DotFrontier` checks that the DVV encoding denotes that set.
- **I5.** `DotFrontier` treats SSI as nondeterministic aborts, while
  `SerialLog` models SSI itself. Every other model assumes the F1 fix.
- **I6.** A context is the snapshot's version plus `at_least`. A commit's
  token is its context plus its own dot.
- **I7.** Concurrent writes to the same fact conflict.
- **I8.** `changes` is exact by definition, except where the window test
  computes it from the hot log.
- **I9.** The Slack policy is: author, else `delegate(channel)`. The residual
  is `{author}`.
- **I10.** A push carries its commit version.
- **I11.** Failover has no fencing.

## What checked out

These are exhaustive TLC runs at small bounds: evidence, not proof.

| Config | Properties | Bounds | Distinct states |
|---|---|---|---|
| `DotFrontier.cfg` | token and context soundness, frontier resolved and monotonic, monotonic reads, compaction | 3 requests, counter ≤ 3, crashes and aborts | 24.2M |
| `DotFrontier_deep.cfg` | same | 2 requests, counter ≤ 5 | 26.3M |
| `DotFrontier_live.cfg` | same, plus every allocated dot eventually resolved | 2 requests, counter ≤ 4 | 8.2M |
| `MultiNode.cfg` | contexts committed, well-founded causal order, causal multi-node reads (with F6) | 2 nodes, 2 clients, 3 dots per node | 26.0M |
| `ViewMaintainer.cfg` | applied set closed, view exact, changes exact (with F3, F7, F10) | 3 nodes, 2 clients | 42.7M |
| `ViewMaintainer_live.cfg` | same, plus eventually applied and reads return | 1 dot per node | 0.5M |
| `ChangeQueries.cfg` | cache exact through polls, `observe`, archiving, and resync | 2 facts, ≤ 5 dots | 0.3M |
| `SlackRT.cfg` | push safe, no leak, audience exact, cache complete (with F2, F4) | 2 users, 2 overlapping tasks | 0.3M |
| `SlackRT_single.cfg` | same | 1 task, 2 polls | 13K |

All 18 configs that are expected to fail do fail, with traces of 13 steps or
fewer.

**Not verified:**

- Postgres internals: SSI is abstracted or idealized.
- High availability and failover (U1).
- Correctness of the DBSP operators.
- Policies beyond author and `delegate` (U4).
- `observe` across nodes (U3).
- Anything beyond these bounds.
