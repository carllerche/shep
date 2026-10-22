# Findings from the core-move model

`CoreMove.tla` models one node, one Raft group, and cores 0 to 2.
- **Each core's store** holds the vote, the config, the start offset, optionally the log, and a placement marker: none, receiving or hosted, with a revision.
- **Every replica start** is a fresh instance that comes up as a follower.
- **A crash** can happen between any two steps.

`RestartMode` selects one of three behaviours:
- **refuse:** the implementation's behaviour, `CoreRemoved`.
- **skip:** the old behaviour, which silently ignores removed cores.
- **merge:** a future design, following Redpanda's protocol for moving a partition between cores. It runs `prepare_transfer`, shuts the replica down, copies the persistent state, then calls `finish_transfer`. Afterwards the source is obsolete only in memory, and recovery takes the hosted placement with the highest revision.

## Findings

| # | Severity | Finding | Config |
|---|---|---|---|
| CM-1 | high (fixed) | A restart with fewer cores that skips the removed directories silently drops their groups. Refusing to start fixes it. Any non-empty directory counts as hosting a group, which is conservative. | `_skip` fails; `_refuse_moves` and `_live` pass |
| CM-2 | high (holds) | A fresh instance never leads in a term an earlier instance led in. Resuming leadership from a move's leader-term hint, or restoring the leader after a restart, breaks this, and with it the session ids (term << 32, plus the per-core counter). | `_resumelead`, `_restorelead` |
| CM-3 | confirms | Redpanda's step order is safe, and each guard is needed: shut the source down before the destination is hosted, copy the vote after shutdown, and clean the source only after the destination is complete. | `_merge` passes; `_noshutdown`, `_earlyvote*` and `_cleanearly*` fail |
| CM-4 | medium (future) | Step 5's "removes its log" contradicts step 3. With one log per group on the node, cleanup must not touch the log. With the per-core `cores/<k>/` layout, step 3 must move the log directory (a rename) before step 4. | `_cleanlog` fails; `_corelog_nocopy` fails and `_corelog` passes, both on an earlier spec |
| CM-5 | medium (future) | A merge may drop a receiving marker, which rolls the move back, but must never promote one to hosted. When merging stores, the hosted entry with the highest revision must survive. | `_droprecv` passes; `_recvdone` fails; `_fold_rev`/`_fold` not yet run |
| CM-6 | low (future) | "Obsolete" is not persisted, so revisions must be monotonic across every store, including the stores of removed cores. | reasoning |

## Results

| Config | Expected | Result | Distinct states | Time |
|---|---|---|---|---|
| `_refuse_moves` | pass | pass | 10,227,241 | 2m23s |
| `_live` | pass | pass | 58,093 | |
| `_merge` | pass | pass | 3,605,960 | 51s |
| `_droprecv` | pass | pass | 4,467,622 | |
| `_skip` | fail | fails NoGroupDropped | 87 | |
| `_restorelead` | fail | fails LeaderTermFresh | 788 | |
| `_resumelead` | fail | fails LeaderTermFresh | 2,031 | |
| `_noshutdown` | fail | fails LiveIsHosted | 1,257 | |
| `_earlyvote` / `_earlyvote_durable` | fail | fail VoteSafety / VoteNoRegress | 4,684 / 3,037 | |
| `_cleanearly` / `_cleanearly_durable` | fail | fail VoteSafety / CfgNoRegress | 24,777 / 3,514 | |
| `_recvdone` | fail | fails CfgNoRegress | 181 | |
| `_cleanlog` | fail | fails LogNoRegress | 1,826 | |

**Pending re-runs.** The disk filled before these could run:
- `CoreMove.cfg`, the main implementation config
- `_corelog`, `_corelog_nocopy`
- `_fold_rev`, `_fold`
- `_sanity_{moved,resumed,extra,vote,cleaned}`
- `_merge_live`, `_merge_deep`

## Not modelled

- more than one group
- log revisions
- the balancer
- a merge interrupted part-way
- other Raft replicas, except as candidates and terms
- disk errors
