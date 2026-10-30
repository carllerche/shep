# TLA+ models of Shep's version, log, and change protocols

Models of the protocols in `docs/design.md`. Topic-specific `FINDINGS-*.md`
reports record counterexamples, checked bounds, and proposed design changes.
Early findings already incorporated into the design are available in Git history.

The resumed implementation investigation is in
[FINDINGS-founding-resume.md](FINDINGS-founding-resume.md); confirmed bugs
are indexed in [../../BUGS.md](../../BUGS.md). Run `./check_founding.sh` for
the focused founding regressions. The tables below describe the earlier
design models, not the complete current suite.

| Spec | Design sections | What it models |
|---|---|---|
| `DotFrontier.tla` | §9.1–§9.4, §10.3 | One PG node: `nextval`, log-row insert, commit/abort/crash at every step, the void fence with Postgres primary-key semantics, fast-path void, the frontier advancer, snapshot versions, tokens, `at_least` reads, token compaction |
| `SerialLog.tla` | §9.4, §10.3 step 2 | What SERIALIZABLE does when every mutation reads its version from the log inside its own transaction (an ideal serialization-graph certifier, more permissive than Postgres SSI) |
| `MultiNode.tla` | §9.5, §9.6 | Two PG nodes, contexts built from snapshot + `at_least`, multi-node reads |
| `ViewMaintainer.tla` | §12 | A materialized count over two nodes' facts, maintained from the logs of three nodes; `at_least` reads; `changes(view, since)` |
| `ChangeQueries.tla` | §11, §10.4 | `changes`/`observe` over Z-sets with a hot window and resync; an app holding (cache, version) |
| `SlackRT.tla` | §13.2 | The Slack channel task: audience Z-set, posts with `since`/`observe`, polls, admin membership changes, failover with overlapping owners, client connect/resume |

## Running

Needs Java 11+ and `tla2tools.jar`. The models were checked with the
`v1.8.0` release asset from <https://github.com/tlaplus/tlaplus/releases>
(TLC reports itself as "TLC2 Version 2026.10.20.183001 (rev: d2e1472)";
sha256 `5bd7b64db99ae4c6c35f290de5e809c0ee41af43f8699cf696099ffb6452b1e2`).
`v1.8.0` is a rolling pre-release tag on GitHub, so record the hash of the
jar you use.

```sh
export TLA2TOOLS=/path/to/tla2tools.jar
./run.sh DotFrontier DotFrontier.cfg      # one model + config
./check_all.sh quick                      # every config except the slow ones
./check_all.sh                            # everything (about 30 minutes on 12 cores; DotFrontier_live alone takes ~12 minutes)
```

`run.sh` runs `java -XX:+UseParallelGC -cp $TLA2TOOLS tlc2.TLC -workers auto
-deadlock ...` (terminal states are expected, so deadlock checking is off).
It serializes runs with `flock` on Linux or `lockf` on BSD. Set `TLC_WORKERS`
to limit workers (the resumed runs use 8). A nonzero TLC exit is preserved;
the state directory is removed after successful, failing, or terminated runs.
`check_all.sh` prints, for each config, whether TLC's result matched the
expected one.

## Configs and expected results

"Pass" means TLC finds no error. "Fail" configs are kept failing on purpose:
each one demonstrates a missing safeguard or checks reachability. Historical
F#/I# identifiers refer to the original report in Git history. Passing configs turn
on the proposed fix instead.

| Config | Expected | Bounds | Checks |
|---|---|---|---|
| `DotFrontier.cfg` | pass | 3 requests, counter ≤ 3, 1 advancer crash | safety invariants, `ResolvedStable`, `FrontierMonotonic` |
| `DotFrontier_deep.cfg` | pass | 2 requests, counter ≤ 5 | same |
| `DotFrontier_live.cfg` | pass | 2 requests, counter ≤ 4, no symmetry | + `AllocatedEventuallyResolved` under fairness |
| `DotFrontier_ooo.cfg` | fail | | sanity: out-of-order commit is reachable |
| `DotFrontier_hang.cfg` | fail | | F9: a hung owner stalls the frontier forever |
| `DotFrontier_noseq.cfg` | fail | | I3: advancer that cannot see the counter never voids a trailing hole |
| `DotFrontier_compact.cfg` | fail | | F8: compacting a token up to the frontier changes its meaning |
| `SerialLog.cfg` | pass (this is the bug) | 3 txns, 6 dots with retries | F1: no two overlapping mutations ever both commit; no out-of-order commit |
| `SerialLog_fix.cfg`, `SerialLog_fix_ooo.cfg` | fail (this is the fix working) | | with the fix, overlapping and out-of-order commits happen |
| `MultiNode.cfg` | pass | 2 nodes, 2 clients, 3 dots/node | contexts committed, causal order well-founded, multi-node reads causal (with fix) |
| `MultiNode_read.cfg` | fail | | F6: multi-node read returns an effect without its cause |
| `MultiNode_closure.cfg` | fail | | F12 (informational): contexts are not transitively closed |
| `ViewMaintainer.cfg` | pass | 3 nodes (view reads 2), 2 clients, ≤ 2 dots/node, ≤ 4 dots total | applied set closed, view = Q at its version, `changes(view)` exact (with fixes) |
| `ViewMaintainer_live.cfg` | pass | 1 dot/node | + every committed entry applied, every `at_least` read returns |
| `ViewMaintainer_untailed.cfg` | fail | | F3: context names a dot of an untailed node; view stalls forever |
| `ViewMaintainer_cursor.cfg` | fail | | F7: max-n cursor skips out-of-order entries |
| `ViewMaintainer_changes.cfg` | fail | | F10: summing recorded output deltas for an off-chain token |
| `ChangeQueries.cfg` | pass | 2 facts, ≤ 5 dots, 1 other writer | cache = Q(version) through polls, observes, resyncs |
| `ChangeQueries_handle.cfg` | fail | | F5: `since` = the Db handle's merged token |
| `ChangeQueries_window.cfg` | fail | | F11: hot-window check by highest dot |
| `SlackRT.cfg` | pass | 2 users, 2 tasks (overlapping), ≤ 2 msgs, ≤ 2 admin ops, 1 poll | push safety, no leak, audience exact, client cache complete (with fixes) |
| `SlackRT_single.cfg` | pass | 1 task, 2 polls | the design with polls and connects serialized |
| `SlackRT_poll.cfg`, `SlackRT_poll_push.cfg` | fail | | F2: concurrent poll and post responses; a removed user receives a message |
| `SlackRT_failover.cfg` | fail | | F4: overlapping owners; a client's token covers a message it never got |
| `SlackRT_connect.cfg` | fail | | F4: a listener attached at a newer snapshot gets a push at an older version |
| `SlackRT_token.cfg` | fail | | F5: resuming from "the last token received" from any response |

Historical state counts and run times are available in Git history.
