# Findings from the freshness-check model

`Freshness.tla` models dots on every field, ghosts, and the freshness check
`unchanged(Q, actor, V)` (design §3, §8, §11.7). It also models
`require unchanged` inside a mutation, under two kinds of concurrency control:

- **`occ`** (the target design). Storage is per-partition logs. A dot is
  `(partition, log index)`, and indexes are contiguous per partition. A
  mutation reads at snapshot `S`, and its home partition's sequencer
  appends it only if no entry committed in `(S, commit]` wrote into its read
  set.
- **`ssi`** (the previous design). One Postgres node at SERIALIZABLE,
  modelled as an ideal certifier.

Spec B, DotFrontier with the dot taken before writes, was dropped when
storage moved off Postgres: a Raft log index has no holes, voids, or void
fence.

Configs that fail are kept failing on purpose. Configs that pass have the fix
turned on, and none of them weakens an invariant.

**Trace notation.** `n1` and `n2` are nodes in index order, and `u` is the
actor. `f` is the indexed field. Q is "the first `max_first` nodes with
`f = a` that `u` can see", with `max_first = 1`. Dot `0` is the initial
state, which every version contains.

| # | Severity | Area | Finding | Config |
|---|---|---|---|---|
| FR1 | high | §11.7 "How it runs" | A walk that stops after `max_first` candidates answers "unchanged" wrongly | `Freshness_maxfirst` |
| FR2 | high | §3, §11.7 | A live index entry must also be tested by the lifecycle dot | `Freshness_fielddot` |
| FR3 | high | §11.7, §6.1 | The policy heads to test include delegate targets and imported definitions | `Freshness_ownpolicy` |
| FR4 | high | OCC | A guard whose read set spans partitions needs every one of them validated | `Freshness_parts_local` |
| FR5 | medium-high | OCC | The validated read set must include the scanned index range and the policy heads | `Freshness_norange`, `Freshness_policycache` |
| FR6 | medium | §11.7 | The check's snapshot must contain `V` | `Freshness_stale` |
| FR7 | low (cost) | §3 | The edge-set dot isn't needed for soundness; under Postgres it serialized links | `Freshness_noedgeset`, `Freshness_ssi_linkserial` |
| FR8 | info | §11.7 under Postgres | Under SSI, "unchanged" holds at the serialization point, not at the wall-clock commit | `Freshness_ssi_realtime` |

## FR1: the walk must reach `max_first` visible rows

**Trace.**
1. n1 and n2 both have `f = a`. `u` is a member of n2 only, so Q at `{0}`
   is {n2}.
2. A delete of n2 leaves a ghost with dot 1.
3. The check walks only one candidate (n1). `u` can't see n1, and its dots
   are 0, so the check answers "unchanged". But Q is now {}.

**Rule.** The walk visits every candidate, live or ghost, until Q's own plan
at `S` has produced `max_first` rows the actor can see. If it never gets
that far, the walk covers the whole range. Ghost rows store the full index
key, including the sort columns.

## FR2: the lifecycle dot gates live index entries

**Trace.**
1. n1 has `f = a`, and `u` is a member.
2. A delete of n1 produces a ghost with dot 1, and a later restore brings n1
   back.
3. At `V = {0, 1}`, Q is {}.
4. Now the field dot is 0, the ghost dot is 1, and the edge dot is 0. All of
   them are in `V`, so the check answers "unchanged". But Q is {n1}.

**Rule.** A live entry is tested by the dots of every fact in its index key
and in its index's predicate. That includes the lifecycle dot: delete,
restore, and placeholder fill.

## FR3: policy heads reached through delegation and definitions

**Trace.** `u` is a member of nothing, so Q at `{0}` is {}. A deploy changes
the imported definition to `anyone`. The check tests only the queried type's
own policy head, which is unchanged, and answers "unchanged". But Q is now
{n1}.

**Rule.** Test the deploy heads of all of the following:
- the queried type's policy
- every policy reached through `delegate`
- every schema whose definitions are inlined
- schema heads and viewer-mapping policies

The alternative is for a deploy to move every importer's head.

## FR4: guards whose read set spans partitions

**Trace.**
1. n1 is on partition 1 and n2 on partition 2. `u` sees both, so Q at `{0}`
   is {n1}.
2. A guard on partition 2 passes its check, then claims n2.
3. Concurrently, n1 is deleted on partition 1.
4. Partition 2's sequencer validates only its own log, so the guard commits.
   But Q at the guard's commit is {n2}.

**Rule.** Every partition the read set touches is validated atomically with
the commit. That includes the partition holding the policy heads. The
alternative is to reject such guards at deploy time. The standalone check is
unaffected.

## FR5: the validated read set

**Trace (range).**
1. n1 has `f = b`, so Q is {}.
2. A guard reads that range.
3. Concurrently, n1 moves to `f = a`.
4. The guard's read set holds only the rows it found, so validation passes.
   But Q is now {n1}.

**Trace (policy cache).** A concurrent policy deploy makes n2 visible. The
policy head isn't in the read set, so validation passes.

**Rule.** The read set of `require unchanged` holds three things, all read at
`S`, never from a cache:
- every index-range key the walk scanned, with its ghost range
- every fact whose dot the walk tested
- every policy and schema head it tested

## FR6: the check's snapshot must contain V

**Trace.**
1. n1 has `f = a`, and `u` is a member.
2. n1 is deleted, so at `V = {0, 1}`, Q is {}.
3. A check runs at the older snapshot `{0}` and answers "unchanged". But Q
   there is {n1}.

**Rule.** The check runs at a snapshot that contains `V`. A lagging reader
waits, as `at_least` does. If a `V` names an entry the partition doesn't
have, the check answers "changed" or `lagging`, never "unchanged".

## FR7: the edge-set dot (cost only)

Ghost edges alone make the check sound, and so does the edge-set dot alone.
Under Postgres the edge-set dot was a column on the source row, so every
link through one node serialized (`Freshness_ssi_linkserial`). Under OCC it
does no harm if written blindly (`Freshness_linkserial`). Keep it only as a
shortcut that saves walking ghost edges, and validate it only when a check
tested it.

## FR8: SSI orders by serialization point (informational)

**Trace.**
1. A guard passes its check with Q = {n1}.
2. A concurrent delete of n1 commits first.
3. The guard commits after it.

SSI orders the guard first, so the result is correct in serial order. At the
wall-clock commit, though, Q was already {}. OCC rejects the guard instead.

## Design rules confirmed

Each of these negative configs removes one rule the design already states,
and each fails in 2 steps:
- `Freshness_noghosts`
- `Freshness_noghostedges`
- `Freshness_returned`
- `Freshness_nopolicyinputs`
- `Freshness_nopolicydots`

## Interpretations chosen

- **IF1.** A version is a set of commits, and the state at `V` is read from
  the multi-version history. Each item's writers are totally ordered: the
  item lives on one partition, and every writer of it also reads it.
- **IF2.** `Soundness` quantifies over every snapshot version, every write
  token, and every per-partition vector, against every `S ⊇ V`.
- **IF3.** The queried type's policy is `pol` ("use the definition" or
  "open"), and the definition is `def` ("member" or "anyone"). The only
  graph input is the membership edge.
- **IF4.** The index is partial on "not deleted" and ordered by node. Q's
  order is the index order, and a ghost sits at its node's position.
- **IF5.** A write of an equal value writes nothing. Linking an existing edge
  or unlinking an absent one is a no-op.
- **IF6.** A guarded mutation checks first, then writes nothing or claims a
  node. If validation fails, it retries with the same `V`. If the check
  fails, it ends with an error.
- **IF7.** OCC validates facts, index-range keys, and ghost-range keys. The
  Postgres variant validates rows.
- **IF8.** Atomicity under SSI quantifies over every serial order consistent
  with the committed dependency graph.

## Results

Each config ran with 12 workers. Mark `Freshness`, `Freshness_parts`,
`Freshness_ssi`, `Freshness_edgeset`, and `Freshness_noedgeset` as slow in
`check_all.sh`.

| Config | Expected | Got | Distinct states | Time |
|---|---|---|---|---|
| `Freshness` | pass | pass | 1,319,802 | 411 s |
| `Freshness_parts` | pass | pass | 1,635,970 | 891 s |
| `Freshness_walkall` | pass | pass | 1,311,216 | 45 s |
| `Freshness_edgeset` | pass | pass | 1,319,802 | 162 s |
| `Freshness_noedgeset` | pass | pass | 1,319,802 | 98 s |
| `Freshness_ssi` | pass | pass | 1,344,050 | 168 s |
| `Freshness_ssi_linkserial` | pass (shows the cost) | pass | 47,944 | 4 s |
| `Freshness_maxfirst` | fail | `Soundness` | 1,592 | 1 s |
| `Freshness_fielddot` | fail | `Soundness` | 12,830 | 3 s |
| `Freshness_ownpolicy` | fail | `Soundness` | 1,330 | 1 s |
| `Freshness_parts_local` | fail | `AtomicityOCC` | 17,341 | 4 s |
| `Freshness_norange` | fail | `AtomicityOCC` | 20,620 | 4 s |
| `Freshness_policycache` | fail | `AtomicityOCC` | 16,999 | 4 s |
| `Freshness_stale` | fail | `Soundness` | 627 | 1 s |
| `Freshness_noghosts` | fail | `Soundness` | 609 | 1 s |
| `Freshness_noghostedges` | fail | `Soundness` | 1,940 | 1 s |
| `Freshness_returned` | fail | `Soundness` | 610 | 2 s |
| `Freshness_nopolicyinputs` | fail | `Soundness` | 1,147 | 1 s |
| `Freshness_nopolicydots` | fail | `Soundness` | 1,493 | 2 s |
| `Freshness_ssi_policycache` | fail | `Atomicity` | 15,682 | 3 s |
| `Freshness_ssi_realtime` | fail (informational) | `AtomicityRealTime` | 15,398 | 2 s |
| `Freshness_linkserial` | fail (sanity) | fail | 33,318 | 6 s |

## Not verified

- Causal rollback of multi-partition reads.
- Raft and storage internals: leader change, log truncation, and lost
  entries. FR6 covers what they mean for the check.
- Schema heads beyond policy heads, viewer mappings, placeholders,
  pagination cursors, non-index sort orders, search, and purge with key
  ghosts.
- A guarded mutation's own writes before its check.
- Bounds: at most 3 requests, 2 nodes, 2 values, 1–2 actors, and
  `max_first = 1`.
