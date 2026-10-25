# Formal verification status

## Resumed implementation checks (2026-10-22)

Resumed from the paused handoff below against implementation `759122e`.
The historical statement that everything was uncommitted no longer applies:
the inherited formal artifacts are tracked in this checkout.

- Confirmed BF1 with a 22-transition scripted trace: stale HELLO replies
  allow two different founding decisions after a seed-list correction.
- Strengthened BF2/B2 with a 23-transition trace: one wiped seed, unchanged
  seed lists and no second initialize still lose a committed founding
  decision. A 14-transition trace also checks ungated readmission.
- Recorded both open implementation bugs in root `BUGS.md` (MODEL-1/2).
- Added `BootstrapFoundTrace.tla`, a restriction to existing guarded
  actions; `check_founding.sh` checks exact expected failing invariants,
  reduced candidate safety, and founding reachability.
- Restored TLC with the inherited jar hash. `run.sh` now supports Linux
  `flock` while retaining serialization, cleanup and TLC exit codes.
- Completed `BootstrapFound_typo_connpromise`: 20,798,540 distinct states,
  7m29s, no violations (two terms, one crash/configuration change, no wipe).
- The focused founding runner completed all nine checks with their expected
  results; both reduced candidates have founding reachability witnesses.
- Removed the resumed session's temporary logs and empty TLC extraction
  directories. The runner now contains Java temporary files and model state
  in one disposable directory; checked cleanup on passing and failing runs.
- Restored pinned Lean/Mathlib and built all 1079 targets successfully.
  New `ShepZSet/Founding.lean` checks the pledge uniqueness and vote-amnesia
  arguments. It also proves the unlanded promise candidate strands an
  initializer after restart, since `prop` is volatile and `pledge` persists.
- Detailed results, bounds, limitations and remaining runs:
  `tla/FINDINGS-founding-resume.md`.

## Historical handoff

The state of the TLA+ models (`tla/`) and Lean proofs (`lean/`) as of 2026-10-22, when all checking was paused.

- **Findings:** the `tla/FINDINGS-*.md` files and `lean/FINDINGS.md`.
- **The design:** `docs/design.md`, frozen at v54.
  - Findings that call for design-text changes are recorded in the FINDINGS files and have not been applied to the doc.
- **Nothing here is committed.**

## Running the checks

- **TLC (TLA+):**
  - Run `cd tla && TLA2TOOLS=<jar> ./run.sh <Spec> <cfg>`.
  - **Only one TLC process may run on the machine at a time.** `run.sh` takes the lock at `tla/.tlc.lock`. Never bypass it.
  - Keep each config to about 10 minutes.
  - `run.sh` now deletes its state directory however it exits. Before that fix, failed and killed runs leaked about 250 GB and filled the disk.
  - To stop a run, kill the `java` process, not the `sh`.
- **Lean:** run `cd lean && lake build`. The toolchain is elan, kept in the session scratchpad. The build was green, with no `sorry`, at the last report.
- **Configs come in four kinds.** The first comment line of each `.cfg` says which it is and its expected result.
  - **Design** configs must pass.
  - **Broken-on-purpose** configs disable one safeguard and must fail. Each one is the evidence for a finding.
  - **Reachability** configs (`_sanity`, `_reach`, `_w`) must fail, which shows that the passing configs actually reach the behaviour.
  - **Superseded** configs are kept for history.

## Done

These were checked against the current design, and the results are recorded.

| Spec / proof | Covers | Findings | Notes |
|---|---|---|---|
| `WalValidate.tla` | §10.3/§10.4: leader validation, idempotency, claims, void entries, flushed commit, refusal at receipt, the failure fast path | W1–W13 in `FINDINGS-wal.md` | The void rule stays in the code: validate and append aren't atomic there. W8 and W12 are confirmed in the code. |
| `Planner.tla` | §7.6 planner, discipline barriers, leader field ops | P1–P7 in `FINDINGS-planner.md` | |
| `AuthHorizon.tla` | §6.3 horizon, tokens, splits, follower fetch, RF = 1 restart | AH1–AH14 in `FINDINGS-auth-horizon.md` | AH13 (the data-directory lock) is implemented. AH10 and AH12 wait for horizon/auth code. |
| `LiveRevocation.tla`, `LiveFrontier.tla` | §13 live queries under the token model | FL8–FL9, LB7 | |
| `Files.tla` | §16 uploads; downloads (soft-state handles, reauth, presign) | FD1–FD5 in `FINDINGS-files-live.md` | Six upload regression configs were never re-run (see "In progress"). |
| `TwoPhase.tla` | §8 2PC, including a split of the home (Split2) | 2PC-1 to 2PC-14 in `FINDINGS-2pc.md` | Nine regression configs were never re-run. |
| `Bootstrap.tla` | §9.6 controller founding, join, growth, re-image | B1–B5 in `FINDINGS-live-bootstrap.md` | Five configs ran only on an earlier revision and need re-running. |
| `Freshness.tla`, `Cutover.tla`, `FrontierMaterialize.tla`, `RegistryBarrier.tla`, `Split.tla` | Earlier rounds | `FINDINGS-freshness.md`, `-cutover.md`, `-frontier.md` | Split and RegistryBarrier have no findings file yet (see "In progress"). |
| Lean `Polarity`, `Planner`, `PolicyCombine`, `Totality`, `Checks`, `PatchMerge`, `LeaderOps` (`FieldOps`), `PlaceholdersQL` | The ShepQL restatement: union minus deny, totality, checks, patch merge, leader ops, placeholders, the Ent port | Q1–Q9, F1–F10 in `lean/FINDINGS.md` | All 9 obligations proved. |
| Lean `Principals` | Apps, actors and viewers: pre-checks, per-atom absent principals, act-for not amplifying | `lean/FINDINGS.md` "Principals" | |
| Lean (other files) | DVV stamps, validation, closure, backfill, lineage, disciplines, freshness, walks, search privacy, … | `lean/FINDINGS.md`, `lean/README.md` theorem map | |

## In progress (paused)

Each entry says what was left when work stopped.

### Highest priority

- **`BootstrapFound.tla`, controller founding as implemented.**
  - **Verdict so far:** unsafe. HELLO is a time-of-check-to-time-of-use race (BF1), and uninitialized or wiped nodes take part in Raft (BF2). An id checked per message stops log mixing but not two clusters (BF3). A per-connection id needs two extra rules (BF4).
  - **Candidate fix, unverified:** the id is chosen at HELLO, and each seed persists a durable, non-expiring pledge. Possibly combined with an object-store claim (B1).
  - **To do:** run `_typo_promise`, `_typo_connpromise`, `_same3_promise`, `_same3_connpromise` and the `*_claim_t1` configs. If any fails, find the smallest fix. Then send the verdict to the implementor.
  - **Until then,** founding is safe only for a fresh, static, single-attempt bootstrap.

### Partly run, no findings file yet

| Spec | Covers | Status | To do |
|---|---|---|---|
| `SegmentLog.tla` | WAL storage crash recovery: segments, offsets, the clean-shutdown marker, truncation, kvstore snapshots; readers concurrent with truncation | The main config passes (4,636,919 states, 47s) | Run `_seg1` (shrink it first: it held the lock for over 40 minutes), `_crash2`, the negative configs and the sanity configs. Write `FINDINGS-storage.md`. |
| `RaftDriver.tla` | The shep-raft driver: replication sessions and seq, gap-timeout rejects, leader restart with restore off, batched heartbeats, openraft commit, prevote and lease vote rejection | No results reported | Run every config and write `FINDINGS-raft-driver.md`, including a "landed implementation" verdict. Answer two questions: does safety depend on the leader clamping commit to its own flushed index (confirmed in the code: it counts itself after `LocalIO`)? Can heartbeats keep a stuck leader alive? |
| `ReplicatedLog.tla`, `ReplicatedLogTrace.tla` | The generic shep-raft `Machine` contract (admit/lead/replay/resolve/apply), void entries, idempotency, commit-time seeding, the controller as a Machine | No results reported | Run every config and write `FINDINGS-replicated-log.md`. The model must reflect the code: admit and append are separate, the term may be stale, the void rule is on, receipt is FIFO, and seeding is max(applied, tail), void entries included. Optionally, a WalValidate refinement. `ReplicatedLogTrace.tla`'s purpose is undocumented; check it. |
| `CoreMove.tla` | Moving a replica between cores; restart with fewer cores | `FINDINGS-coremove.md` written (CM-1 to CM-6) | Run `CoreMove.cfg` (the main implementation config, never run), `_corelog`, `_corelog_nocopy`, `_fold_rev`, `_fold`, the five `_sanity_*` and `_merge_live`/`_merge_deep`. Fill in the results table. |
| `SplitCtl.tla`, `Split.tla`, `RegistryBarrier.tla` | The controller-driven split (routing via `moved`, cancel decided in p's log, drain, idempotency handoff); registry barriers | All 10 SplitCtl configs give their expected verdicts (main: 1,161,998 states); the idempotency filter was added for 2PC-10/11 | Write `FINDINGS-split.md` (only partial reports were received). Fix the misleading header of `RegistryBarrier_nobarrier_ix.cfg`: it fails BarrierCtx, not IndexExact. |
| `ControllerReconcile.tla`, `ControllerCreate.tla` | Reconciliation (fold, revisions, two-phase moves, cancel, vnode revisions); create (partition-id provenance, manual-create refusal, void creates, re-check in apply) | Reconcile passes and its negative configs fail; ControllerCreate's negative configs fail | Shrink `ControllerCreate.cfg` and `_live`, which reached 185M+ states without finishing, and run them. Write `FINDINGS-controller.md`, with a verdict on the create-only reconciler. |

### Re-runs pending (the disk filled before they ran)

- **WalValidate:** `_flush` and `_split` on the final spec; `_truncfix` and `_claim*`, so far run in round 1 only.
- **Bootstrap:** `_rf1seeds`, `_noallempty`, `_sameid`, `_voter2` and `_nouuid` (new constants).
- **TwoPhase:** `_noregcheck`, `_outage`, `_readonly`, `_retry`, `_skew`, `_split`, `_splitlost`, `_twohomes` and `_wait`.
- **Files:** `_direct`, `_live`, `_noreadcheck`, `_nostoredcheck`, `_push` and `_revoke`.

## Not started

- **Model the horizon and auth code once it exists:**
  - fetch carrying `(commit index, lease stamp)` relayed unchanged by followers (AH12);
  - write deadlines capped at `min(horizon, exp + skew)` (AH10);
  - the front end's failure fast path with retry marking (W10).
- **Model what the implementor builds next:** splits, moves, real core moves, join, lightweight heartbeats, `may_recover`, and the conditional append (if the void rule is ever to be dropped).
- **Lean, not proven yet:**
  - function inlining and `walk`'s `stop_at` in the totality model;
  - guards that read other nodes' fields, in `Polarity.lean`;
  - int64 overflow in leader ops.
- **TLA+ items listed as not modelled** in each FINDINGS file. Examples:
  - Raft failover in `Planner.tla`;
  - DVV extras;
  - real 2PC inside the planner;
  - liveness in `BootstrapFound.tla`;
  - expiry of the idempotency window.

## Housekeeping

- **`tla/README.md` and `check_all.sh` are out of date.** They cover only the early specs: DotFrontier, SerialLog, MultiNode, ViewMaintainer, ChangeQueries and SlackRT. Everything else needs adding, with its expected results. CoreMove's report lists the `check_all.sh` lines to add.
- **Early specs:** DotFrontier, SerialLog and MultiNode model the old Postgres-based storage. Decide whether to mark them superseded. ViewMaintainer, ChangeQueries, SlackRT and `PlacementMove.tla` were checked in earlier rounds and haven't been re-checked against the current design.
- **Possibly superseded:** `PlacementMove.tla` may be superseded by `ControllerReconcile`/`SplitCtl`. Confirm, then mark it.
- **Superseded configs** (`*_cred*`, `BootstrapSysPart_*`, `LiveRevocation.cfg`, …) carry a `\* SUPERSEDED` header. Decide whether to keep or delete them.
- **Untracked files:**
  - The new specs, configs and findings files are untracked: about 255 configs and 11 specs.
  - Decide what to commit. One option is to commit only the specs with findings files, and leave the unrun specs out or mark them as work in progress.
- **Design-doc fixes are pending** (the doc is frozen). Each FINDINGS file lists the text it calls for:
  - `FINDINGS-planner.md` items 1–6
  - AH10 and AH13 wording
  - `lean/FINDINGS.md` "Principals" doc fixes
  - W7–W13
  - B1–B4
  - 2PC-9 to 2PC-13
  - FD1 and FD4
  - CM-4 and CM-5
