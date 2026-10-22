# Findings from the authorization-horizon model

## The model

`AuthHorizon.tla` models these parts of the design:
- the authorization horizon (§6.3)
- the authorization-staleness and residual-access rows of §9
- topic pausing (§13.2)
- link checks (§16.4)

**Time.** Time is discrete. Every node's clock reading is within `ε` of real
time.

**Partitions.** Each has a leader that heartbeats `(commit index,
leader-lease time)`:
- `sys`: keys
- `meta`: policies
- `act`: the actor
- `map`: the agent's `acts_for` target, or the request's viewer under the
  credential model
- `chan`: the membership edge a policy checks

**Failover.** A deposed leader keeps heartbeating until its lease ends. A new
leader commits only after the old lease's end.

**Front ends.** They are stateless in the model. Any front end may hold any
earlier heartbeat or none, and answers `unavailable` without evidence.

**Operations.** The model covers these:
- reads, which can mint links
- mutations: read, then send (a proposal or a 2PC decision), then append after
  a delay
- a live topic stepping at the published frontier
- link checks, with a presigned redirect
- downloads, judged by the object store's clock

**Reduction.** Each behaviour has one revocation, in one focus partition. This
is sound because every invariant is a conjunction over partitions, and extra
revocations or staler evidence only cause denials.

**Invariants.**

| Invariant | What it checks |
|---|---|
| `Inv_Horizon` | Every actor-sensitive output at time `t` comes from an `S` at least the commit index at `t − B` |
| `Inv_Revoke` | Nothing is produced after `tr + B`, except a download within `PL` of its check |
| `Inv_FailClosed` | Data goes out only with real-time-fresh evidence for every relevant partition |
| `Inv_MaterializerLag` | The chan materializer serves nothing while behind chan's commit index of `B` ago |
| `Inv_LinkRevoked` | A link whose key was revoked fails after `tr + B` |

## Findings

| # | Severity | Finding | Config |
|---|---|---|---|
| AH1 | high | Checking the horizon at send time doesn't bound when a proposal or decision is appended | `_inflight` fails; `AuthHorizon` passes with the fix |
| AH2 | high | "Materializers keep up or refuse" doesn't reach topics, which read at the frontier | `_topicchan` fails; the fix passes |
| AH3 | low | The presigned GET lives `PL + 2ε` past the check | `_presign` fails; the fix passes |
| AH4 | confirms | Evidence must not be reused for a later decision; the deadline alone is enough | `_reuse` fails, `_reusedeadline` passes |
| AH5 | confirms | The viewer's (formerly the mapping target's) partition must be in the horizon | `_nomap` |
| AH6 | confirms | The materializer lag check is needed | `_nomatcheck` |
| AH7 | confirms, precise | Evidence is fresh iff `clock − lease time ≤ B − 2ε` | `_nodrift` |
| AH8 | confirms | A topic must not step at a frontier lacking the horizon | `_streamlag` |
| AH9 | confirms, precise | A deposed leader stamps no heartbeat past its lease; no extra margin is needed at handoff | `_deposed` |

### AH1: writes in flight need a deadline

**Trace.**
1. A policy denying agents commits in `meta` at tick 0.
2. A proposal reads with the instant-0 heartbeat, which is the old index. It
   re-checks at send time (t = 3, accepted because of skew).
3. The proposal is appended at t = 4 > `B`.

"Too old" doesn't help here: a quiet partition's component never ages.

**Fix.** Proposals and decisions carry a horizon deadline: the oldest
leader-lease time in their evidence, plus `B − 2ε`. The appending leader
rejects anything past the deadline (`too_old`, so the front end re-executes),
and a 2PC decision past it is an abort. `_reusedeadline` shows the deadline
alone carries the bound.

### AH2: the rule must be on the version read

**Trace.**
1. A channel removal commits at 0. The materializer applies it and is
   caught up.
2. The published frontier's chan component lags, for example because its
   context names a stalled partition.
3. The topic steps at `S[chan] = 0` and pushes at t = 4.

**Fix.** A materializer refuses an actor-sensitive read at a version whose
component for its partition is older than its leader's commit index of `B`
ago. It uses the same evidence and the same `B − 2ε` test. A topic whose
frontier lags pauses.

### AH3: the presigned residual

**Fix.** Set the expiry to the front end's clock + `PL − 2ε`, or state the
residual as "one minute plus clock skew".

## Results

The bounds: ε = 1, B = 3, PL = 2, LeaseLen = 2, time 0..8, at most one
failover.

| Config | Expected | Got | Distinct states | TLC time |
|---|---|---|---|---|
| `AuthHorizon` (all fixes) | pass | pass | 4,355,136 | 18 s |
| `_reusedeadline` | pass | pass | 120,887 | <1 s |
| `_inflight` | fail | `Inv_Revoke` | 11,406 | <1 s |
| `_topicchan` | fail | `Inv_Horizon` | 1,059 | <1 s |
| `_presign` | fail | `Inv_Revoke` | 5,682 | <1 s |
| `_reuse` | fail | `Inv_Revoke` | 6,247 | <1 s |
| `_nomap` | fail | `Inv_Revoke` | 497 | <1 s |
| `_nomatcheck` | fail | `Inv_MaterializerLag` | 484 | <1 s |
| `_nodrift` | fail | `Inv_Revoke` | 836 | <1 s |
| `_streamlag` | fail | `Inv_Revoke` | 3,947 | <1 s |
| `_deposed` | fail | `Inv_Revoke` | 2,032 | <1 s |

**Non-vacuity.** Four `Sanity_*` state predicates are each violated under
`AuthHorizon.cfg`.

## Not modelled

- **Ordering and delivery:**
  - registry barriers
  - the Raft quorum delay between append and commit
  - delivery delay on a read response
- **The key cache:** it must be the learner's state at `S`.
- **Other mechanisms:**
  - token expiry and `reauth`
  - `strict_revocation`
  - topic resyncs and routing (`LiveRevocation.tla` covers these)
- **Restores, liveness, and clock rate.**

**Model change.** The model predates the credential-based actor and viewer
change. `map` now corresponds to the viewer's partition. Its results carry
over.

## Token model and review fixes (AH10–AH14)

| # | Severity | Finding | Evidence |
|---|---|---|---|
| AH10 | medium | A write admitted just before its token expires is appended up to B later. "Bounded by token lifetime" needs each write's deadline capped at exp + skew. | `_tokendeadline` fails; `_token` passes with the cap |
| AH11 | high | After a split, the parent's live leader keeps the evidence fresh while the deletion commits at the child. | `_splitparent` fails; `_split` passes with the fix |
| AH12 | high | A follower must not stamp evidence with its own clock. Relaying the leader's stamp unchanged keeps the bound. | `_followerstamp` fails; `_follower` passes |
| AH13 | confirms, made precise | An RF = 1 restart may skip the lease wait only if the old incarnation is dead. Incarnation IDs don't carry the bound. | `_restart` passes; `_restartzombie` fails |
| AH14 | confirms | A deleted viewer is denied even under an app-only allow. | `_viewerapponly` passes; `_viewerapponly_nofix` fails |

**The token bound.** Suppose the auth service revokes a token at tr. Any output after that happens by tr + TokLife + ExpSkew + ε at the latest, plus PL for a presigned download. Writes need the exp cap from AH10. Shep's own revocations still take effect within B.

**Model changes.**
- The partition called `map` is renamed `viewer`, so `_nomap` is now `_noviewer`.
- `sys` (Shep-issued keys) is superseded and no longer in the design config.

**Results.**

| Config | Distinct states | TLC time |
|---|---|---|
| `AuthHorizon` | 8,970,748 | 39s |
| `_token` | 3,482,684 | 16s |
| `_split` | 409,104 | 2s |
| `_follower` | 3,530,264 | 16s |
| `_restart` | 723,824 | 3s |
| `_viewerapponly` | 824,872 | 4s |

Every failing config fails in under 1s. The sanity predicates confirm that commits and pushes at the deadline boundary, and residual downloads, are all reachable.

**Doc fixes these findings call for.** The design doc is frozen, so they are not applied.
- §6.3: a proposal's or decision's deadline is `min(horizon deadline, exp + skew)`. State the token bound exactly, as above.
- §6.3, "Leases bound stamps": skipping the lease wait at RF = 1 relies on the old process being dead. For example, take an exclusive lock on the WAL directory.
- `max_wait ≤ (B−2ε)/4` is an availability rule and is not model-checked.

**Implementation status (2026-10-22).**
- **AH13 is implemented.** `RaftNode::start` takes an exclusive flock on `<data_dir>/LOCK` before opening anything, and holds it until every core has stopped. A second node on the same directory fails to start with `DataDirLocked`. There are in-process and end-to-end tests. Leader restore is off, so a restarted RF = 1 node elects itself in a new term.
- **AH10 and AH12 are not yet applicable.** Fetch carries no lease evidence yet, and the WAL checks no tokens yet.
