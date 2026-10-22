# Findings from the frontier and materializer model

## The model

`FrontierMaterialize.tla` models design §3, §8 (abstracted), §9.4–§9.7, §10.4 and §12.

- **Versions** are sets of dots.
- **Partitions**: two, or three in the context configs.
- **Writes**: at most 2 single-partition writes, plus one 2PC (prepare in B, decision in A, release in B).
- **Materializers** apply entries in order, flush in batches, and can crash. B has a second replica.
- **Frontier**: a meta leader publishes the largest causally closed subset of the applied entries.
- **Reader**: picks a version, closes it by the fixpoint, is admitted and pins its version, waits, then reads.
- **Retention**: a floor with compaction.
- **View**: one view over B, stepping between frontiers.

## Findings

| # | Severity | Finding | Config |
|---|---|---|---|
| FM1 | medium-high | Compaction by log position drops a version that a read at or above the floor still needs, when the newer version is a 2PC part stamped with a decision dot the read's version lacks. Fix: a release's context is its decision's context plus the decision dot. | `_relctx` |
| FM2 | medium | §9.4's "the closure is one `cf` lookup" holds only if contexts are causally closed, which §9.5 denied. Fix: state that contexts are closed, or use the fixpoint. | `_once` fails; `_onceclosed` and `_ctx3` pass |
| FM3 | medium | The frontier moves backwards unless the leader publishes F_prev ∪ Core(the highest applied index reported per partition). A view stepping backwards keeps dots its version lacks. | `_nokeep`, `_nokeepview` |
| FM4 | medium | A frontier can contain a decision whose release isn't applied yet. Reads, and maintainers that tail only their footprint, must resolve in-doubt prepares. `at_least` liveness depends on the participant's timeout appending the release. | `_noindoubt`, `_livenorel` |
| FM5 | confirms | A frontier made of per-partition maxima isn't closed, and a view stepping to it gets weight −1. | `_maxfrontier`, `_maxview` |
| FM6 | confirms | Reading each partition at its applied index, without closing, shows half a 2PC. | `_noclose` |
| FM7 | confirms | If `m/applied` becomes durable before its data, entries are lost on restart. | `_restart`, `_restartread` |
| FM8 | confirms | A floor that ignores pinned reads breaks them. A read below the floor gets `too_old`. | `_floor` |
| FM9 | low (reasoning only) | Keep `m/cf` per position. The floor is a version's contiguous prefix, not its highest dot. | — |

## Results

| Config | Expected | Got | Distinct states | Time |
|---|---|---|---|---|
| `FrontierMaterialize` | pass | pass | 634,556 | 28s |
| `_read` | pass | pass | 516,675 | 13s |
| `_ctx3` | pass | pass | 268,108 | 8s |
| `_onceclosed` | pass | pass | 66,420 | 2s |
| `_live` | pass | pass | 241,352 | 3m03s |
| `_maxfrontier` | fail | FrontierClosed | 238 | <1s |
| `_restart` | fail | MatConsistent | 154 | <1s |
| `_floor` | fail | ReadExact | 2,611 | <1s |
| `_noclose` | fail | ReadExact | 5,634 | <1s |
| `_maxview` | fail | ViewExact | 1,252 | <1s |
| `_nokeep` | fail | FrontierMonotone | 715 | <1s |
| `_nokeepview` | fail | ViewExact | 4,711 | <1s |
| `_restartread` | fail | ReadExact | 5,668 | <1s |
| `_noindoubt` | fail | ReadExact | 13,213 | <1s |
| `_relctx` | fail | ReadExact | 64,812 | <1s |
| `_once` | fail | ReadClosed | 2,349 | <1s |
| `_livenorel` | fail | AtLeastTerminates | 519 | <1s |

## Not modelled

- **Raft and 2PC detail:** Raft internals, 2PC aborts and timeouts, and more than one 2PC.
- **Versions:** no extras beyond a prefix.
- **Topology:** splits.
- **Registry and storage:** barriers, archive and truncation, and compaction by stamp.
- **Change queries and readers:** `changes(view, since)`, multiple readers, and DBSP operators.
