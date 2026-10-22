# Findings from the live-frontier and bootstrap models

## The models

### `LiveFrontier.tla`

Covers §9.7, §13.1–§13.3, §11.4 and §6.3. Extends `LiveRevocation.tla`.

- **Partitions.** There are two:
  - A holds C1's messages and members.
  - B holds C2's members, the actor nodes (`@deleted_at`), the agent's `acts_for`
    edge, and policy deploys.
- **Query.** The query is `in_channel(C1)` with quotes. m2 quotes q, which lives
  in C2.
- **Frontiers.** The meta leader publishes closed committed cuts.
- **Stepping.** The topic steps between published frontiers. Phase 2 caches
  each subscriber's flags and re-evaluates them on routed facts.
- **Front-end failure.** The front end can crash. A replacement topic may start
  at any published frontier, and clients resume with `since`.

### `Bootstrap.tla`

Covers §9.6 and §14.1.

- **Fleets.** There are two fleets, prod and np. Each has:
  - a system partition on its seed node
  - a WAL node hosting meta and data
  - a front end
- **Discovery** goes seed list → system partition → meta map → data partitions.
- **Names.** Production owns the app and schema-name records, and np keeps a
  copy that follows them.

## Findings

| # | Severity | Finding | Config |
|---|---|---|---|
| LB1 | medium | A resume answered by a topic that is behind its `since` moves the stream's version backwards | `LiveFrontier_lagjoin` |
| LB2 | medium | Where np registers app and schema names is unspecified; both obvious answers break §14.1 | `Bootstrap_forward`, `Bootstrap_local` |
| LB3 | confirms | Topics must step to published frontiers, not per-partition maxima | `LiveFrontier_maxima` |
| LB4 | confirms | Joins go through the loop, not an independent snapshot (F4) | `LiveFrontier_snapshot` |
| LB5 | confirms | Mapped nodes must be re-registered at each step (FL3) | `LiveFrontier_noreregister` |
| LB6 | confirms | The system partition's membership must be static configuration | `Bootstrap_selfmember` |

### LB1: a resume to a lagging topic

**Trace:**
1. u2 holds m1 at {A1}.
2. Its front end crashes.
3. It resumes on a front end whose topic is at {}.
4. The first update arrives at {}, so m1 disappears and the version decreases.

**Fix.** A join whose `since` or handle position is ahead of the topic waits in
the loop until the topic reaches it, bounded by the wait bound.
`LiveFrontier_resume` passes with this fix.

### LB2: where np registers names

**Forward trace.** A dev registration goes to production's system partition.
That is a non-production write to production.

**Local trace.** Production assigns n1 to t2, but np has already recorded n1
for t1 before the copy arrives.

**Fix.** Pick one:
- Only production registers names, and np waits for its copy.
- np uses a separate draft namespace that cannot collide with production names.

### LB3–LB6

- **LB3.** A post whose context names u2's deletion is applied before the
  deletion is. A topic at the maxima cut then shows the post to u2.
- **LB4.** A subscriber that joins by snapshot at {A1, A2} is then pushed m1 at
  {A1}.
- **LB5.** g1 is remapped to u2, then u2 is deleted, and g1 keeps its view.
- **LB6.** With the system partition's membership stored only in itself, a cold
  start never reaches anything.

## Results

| Config | Expected | Got | Distinct states | Time |
|---|---|---|---|---|
| `LiveFrontier` | pass | pass | 1,578,112 | 37 s |
| `LiveFrontier_resume` | pass | pass | 2,031,228 | 19 s |
| `LiveFrontier_maxima` | fail | `DeleteCloses` | 1,091 | <1 s |
| `LiveFrontier_snapshot` | fail | `Monotone` | 43 | <1 s |
| `LiveFrontier_noreregister` | fail | `ViewExact` | 1,932 | <1 s |
| `LiveFrontier_lagjoin` | fail | `Monotone` | 108 | <1 s |
| `Bootstrap` | pass | pass (+ liveness) | 683,642 | 1 m 08 s |
| `Bootstrap_selfmember` | fail | `EventuallyReached` | 64 | <1 s |
| `Bootstrap_forward` | fail | `NpNeverWritesProd` | 473 | <1 s |
| `Bootstrap_local` | fail | `NamesAgree` | 2,556 | <1 s |

**Witness invariants.** Each witness is violated in the design configs, which
shows these paths are reachable:
- the stream closes
- a quote is seen
- an agent loses access
- a resume misses the hot window and has to resync

### Invariants checked

**LiveFrontier:** `Monotone`, `Published`, `DeleteCloses`, `PushSafe`,
`FirstAtTopic`, `PushExact`, `ViewExact`, `ResyncExact`.

**Bootstrap:**
- Safety: `SysNotSelf`, `NoSharedWritable`, `NpNeverWritesProd`, `NamesAgree`.
- Liveness, under weak fairness: `EventuallyReached`, `EventuallyCopied`.

## Not modelled

**Versions and frontiers:**
- No extra dots and no splits.
- Frontiers are modelled as committed cuts.

**Bounds and failures:**
- At most 4 entries and 2 messages.
- One crash.
- Agents across a crash are not checked.

**Live-query features:**
- live ranges and paging
- coalescing
- `reauth`
- residual policies
- topics keyed by app

**Bootstrap:**
- One replica per group: no quorums, no membership changes.
- No universe creation, API keys, or quotas.
