---
title: Repair and Improvement — Retained Mechanism Profile
chapter: 10b
version: 0.2.0
status: draft
date: 2026-09-05
---

# Repair and Improvement — Retained Mechanism Profile

**DAEMON-REPAIR-001 — Retained mechanisms.** Parts A and B preserve the original repair and improvement requirements, numerical defaults, event shapes, trust gradient and conformance levels. Section numbers restart in each part; a reference to repair §6 means Part A §6, while improvement §6 means Part B §6. Legacy chapter IDs 08 and 09 remain provenance identifiers, not new distribution IDs.

This profile retains intent during refactoring. The [accepted handover](authority-handover.md) governs its policy status and takes precedence over historical constitutional procedures reproduced below. It does not override [authority](00-authority.md) or [foundations](01-foundations.md), or establish runtime conformance at baseline `96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be`. The [daemon chapter](10-daemon-agent-operation.md) identifies the narrower implementation evidence. Original files remain preserved historical records.

Known discrepancies remain explicit: Part A's user-service-only statement conflicts with the later [identity profile](10a-daemon-identity-profile.md), which distinguishes system and user services; a daemon cannot enforce `Restart=no` simply by exiting; the whole cross-process escalation/DLQ system and Part B proposal/approval pipeline have not been established as implemented. Part B's atomic multi-artifact transaction and backward-compatible reversal requirements require a concrete coordinator and schema recovery design. No implication of automatic authority follows from this retained text. The quorum rule is conditional and dormant under its stated trigger.

Other legacy references map by subject: self-awareness/property read surfaces → [properties](06-properties.md); topic delivery → [broker/topics](05-broker-topics.md); persistent recovery → [persistence/recovery](07-persistence-recovery.md); mesh trust → [mesh](08-mesh-trust.md); Mix language → [Mix integration](09-mix-integration.md). Constitution article numbers remain historical clause references pending the suite's complete claim mapping; they are not silently renumbered.

Paths under `deployment-tools/` stand for the deployment profile's legacy helper artifacts, not a claimed public directory or executable command recipe. Private operational-reference paths have been replaced by descriptive provenance labels. Numerical rules and safety obligations are otherwise retained. Dated worked examples and source-presence statements are historical evidence, not current test results.

## Part A — Deterministic self-repair (legacy chapter 08)

## 1. Purpose and Non-Goals

### 1.1 Why repair is a separate layer

Self-aware (SPEC 07) gives the substrate eyes. Self-improve (SPEC 09) gives
the substrate hands for *intentional change*. Self-repair gives the
substrate reflexes for *unintentional damage*. The three are orthogonal:

- A daemon can be observable (L1 in SPEC 07) without being repairable.
- A daemon can be supervised by systemd without being observable.
- An improvement proposal can be applied without ever invoking a repair
  action.

Repair MUST be deterministic. Deterministic means: given the same observable
signals and the same configuration, the system selects the same action. No
LLM, no probabilistic classifier, no "best guess." Determinism is what
makes repair safe to trigger automatically — the operator can predict and
audit what will happen.

### 1.2 Non-goals

- **Agent-driven repair decisions.** An agent MAY observe repair events
  (via SPEC 07's `props.changed` and `harness.events` topic), record them,
  surface them in dashboards. An agent MUST NOT decide whether to restart,
  reset, or escalate. That is policy, not agency.
- **Predictive maintenance.** Forecasting failure before it happens is out
  of scope. Repair triggers on observed degradation, not predicted
  degradation.
- **Distributed consensus.** This SPEC scopes to single-node repair plus
  cross-process escalation within one mesh peer. Cross-node coordinated
  repair (e.g., fail over indexd from alpha to beta) is deferred until
  multiple mesh nodes are in production.
- **State-machine modelling.** Daemons MAY maintain rich internal state
  machines; this SPEC does not specify them. It specifies only the
  externally-observable repair surface.

### 1.3 Why this SPEC matters

The mandate's reconstructibility criterion (criterion 3) reads, in part:
"Where safe, components must be hot-swappable. The system must be able to
rebuild parts of itself given source changes." That hot-swap and rebuild
work is in SPEC 09. But before a system can be safely *modified*, it must
be safely *recoverable* — otherwise every modification carries the risk of
unrecoverable degradation. Repair is the floor that makes improvement safe.

---

## 2. The Action Space

Repair actions form a finite, ordered list. Adding a new action requires a
SPEC amendment.

| # | Action | Effect | Reversibility |
|---|---|---|---|
| 1 | **restart** | Terminate the process; supervisor relaunches it. | Trivial — same binary, same state on disk. |
| 2 | **reset-state** | Restart + reset volatile state to last-known-good (LKG) on disk. | Reversible if LKG is preserved (§6). |
| 3 | **fail-over** | Mark this instance as drained; route traffic to a peer instance. | Reversible by un-draining, *if a peer exists*. |
| 4 | **escalate** | Emit `harness.events crisis.unresolved` with a generated crisis ID; refuse further repair attempts on this signal. | Operator action required (constitution V.4). |
| 5 | **halt** | Exit with non-zero rc, refuse to restart (systemd `Restart=no` for this transition). | Operator action required. |

The actions are ordered by intervention severity. The escalation ladder
(§5) selects actions monotonically — once a higher-severity action has been
taken for a given signal, lower-severity actions are not reattempted until
the operator clears the crisis.

### 2.1 What is not an action

- "Notify the operator." Notification is a side-effect of every action via
  `harness.events`; it is not a standalone repair step. A repair that does
  nothing but notify is a missing repair.
- "Wait." Backoff *between* actions is a parameter (§5.2), not an action.
  A daemon that handles a degraded signal by silently waiting forever is
  in violation of §8 (root-cause discipline).
- "Apply a patch." Code modification belongs in SPEC 09, never in repair.
  Repair returns the system to a known state; it does not improve the
  system.

---

## 3. In-Process Degradation Primitives

The primitives below are specified as contracts. Their implementation lives
in a planned `cosmix-lib-core` crate (not yet in tree as of 2026-04-25).
The cosmix-indexd circuit-breaker logic is the precursor to extract.

### 3.1 Circuit breaker

A circuit breaker fronts a call site with three states:

- **closed** — calls pass through; failures increment a counter.
- **open** — calls short-circuit with a fail-fast error; no underlying
  call is made.
- **half-open** — a probe call is permitted; on success, transition to
  closed; on failure, return to open.

**Contract:**

| Property | Description | SPEC 07 path |
|---|---|---|
| `state` | `closed` / `open` / `half_open` | `breakers.<name>.state` |
| `failure_count` | failures in current window | `breakers.<name>.failures` |
| `opened_at` | timestamp of last open transition (null if closed) | `breakers.<name>.opened_at` |
| `next_probe_at` | timestamp half-open probe is permitted | `breakers.<name>.next_probe_at` |

State transitions MUST emit `<svc>.props.changed` per SPEC 07 §3 with the
breaker name in the path. State changes MAY also emit
`harness.events breaker.opened` / `breaker.closed` for cross-service
visibility.

**Configuration knobs** (per breaker):

- `failure_threshold` — number of failures within `window` to open (default
  5)
- `window` — sliding window for failure counting (default 60s)
- `probe_after` — time in open state before half-open probe (default 30s)
- `success_threshold` — consecutive half-open successes to close (default 1)

### 3.2 Dead-letter queue

When a daemon cannot process an ABP message — schema violation, persistent
backend unavailability, repeated handler exception — the message MUST be
captured in a DLQ rather than silently dropped or infinitely retried.

**Contract:**

- DLQ entries persist to disk under
  `~/.local/state/cosmix/<svc>/dlq/<id>.bus` (one file per entry, ABP
  format on disk).
- DLQ depth is exposed at `dlq.depth` (SPEC 07 path).
- A daemon SHOULD emit `<svc>.dlq.entry` on every DLQ write, with the
  failed message id and reason in the body.
- Operator inspection: `mix deployment-tools/dlq_show.mix <svc>` lists pending entries;
  re-injection is a manual operation.

DLQ writes are idempotent under message id — re-receiving the same id does
not produce duplicate DLQ entries.

### 3.3 Retry policy

Retry is the time dimension of the breaker. A daemon making downstream
calls MUST retry with bounded attempts and exponential backoff. The
contract:

- `max_attempts` — total tries including the first (default 3)
- `initial_backoff` — first-retry delay (default 100ms)
- `multiplier` — geometric growth factor (default 2.0)
- `max_backoff` — cap on individual delay (default 30s)
- `jitter` — randomisation factor 0.0–1.0 (default 0.2)

Retries inside the window count toward the breaker's failure threshold.
Once max_attempts is reached, the call MUST surface a structured error AND
emit `<svc>.retry.exhausted` to `harness.events`.

### 3.4 Timer wheel

A timer wheel provides bounded-cardinality scheduling for short-lifetime
timers (probe deadlines, backoff resumption, watchdog kicks). The contract
is operational, not API-shaped:

- Cancellation MUST be O(1).
- Insertion MUST be O(1) amortised.
- Timer drift under steady-state load SHOULD be < 10% of the smallest tick.
- The wheel exposes `timers.scheduled` and `timers.fired_total` as SPEC 07
  properties for capacity diagnosis.

The wheel exists to keep timer overhead off the daemon's hot path; daemons
without short-timer scheduling needs MAY skip this primitive.

### 3.5 Health classifier

Every daemon MUST expose a `lifecycle.health` property (SPEC 07 §2.1) with
one of:

- `ok` — all subsystems nominal
- `degraded` — partially functional; specific subsystems impaired but the
  service can answer requests
- `unhealthy` — primary function impaired; service responds to control
  commands but not to its main protocol surface
- `crashing` — recently restarted; in initialisation
- `crisis` — refusing further work; constitution V.4 crisis active

Transitions emit `<svc>.props.changed` per SPEC 07 §3. The classification
is deterministic given the daemon's internal state — no probabilistic
"health score."

---

## 4. Process Supervision

### 4.1 systemd is the supervisor

Cosmix runs as systemd user services per the constitution's Article VI
forbidden-targets list (`~/.config/systemd/user/cosmix-*.service`).
systemd is the supervisor; cosmix daemons do not implement their own
restart logic.

### 4.2 Required unit settings

Each `cosmix-<svc>.service` MUST set:

| Directive | Value | Reason |
|---|---|---|
| `Restart=` | `on-failure` (default) or `always` (for the broker) | Restart on non-zero exit; `always` for noded since loss of the local broker blocks everything else. |
| `RestartSec=` | `2s` (minimum) | Avoid tight crash loops while still recovering quickly. |
| `StartLimitIntervalSec=` | `60s` | Window for crash-loop detection. |
| `StartLimitBurst=` | `5` | Crashes within window before systemd gives up and goes to `failed` state — escalates per §5. |
| `WatchdogSec=` | `30s` (where supported) | Daemon must `sd_notify(WATCHDOG=1)` within this window or systemd kills + restarts. |
| `Type=` | `notify` (where watchdog used) or `simple` | `notify` enables `sd_notify` for ready/watchdog signaling. |

Daemons NOT setting `WatchdogSec=` MUST emit a heartbeat via SPEC 07's
`world.<svc>` retained topic at least once every 60s.

### 4.3 Drained state

Before restart, a daemon SHOULD enter a *drained* state: refuse new work,
finish in-flight work with a bounded deadline, persist DLQ entries for
anything not finishable. Drain is signaled by setting `lifecycle.health =
degraded` and `lifecycle.draining = true`. systemd's `TimeoutStopSec=`
gives the bound (default 90s; tune per daemon).

### 4.4 Health propagation to systemd

A daemon at `lifecycle.health = unhealthy` SHOULD `sd_notify(STATUS=...)`
with a one-line summary. systemd `systemctl status cosmix-<svc>` then
shows the human-readable summary alongside the structured props surface.

---

## 5. Cross-Process Escalation Ladder

The escalation ladder is the contract that turns observable signals into
repair actions. It runs in a meta-supervisor role — an L3 SPEC 07
subscriber that watches `world.*` and `harness.events`, applies the rules
below, and emits ABP commands to act.

In the bootstrap implementation, the meta-supervisor is `cosmix-noded`
itself (already the local broker) extended with an escalation engine.
A separate crate is permitted but not required.

### 5.1 The standard ladder

For each watched daemon, the ladder runs the following steps. Each step
has a *trigger condition* and an *action*. Steps are attempted in order;
once one succeeds, the daemon's signal counter resets.

| Step | Trigger | Action | Backoff before next step |
|---|---|---|---|
| 1 | `world.<svc>` stale > 60s OR `lifecycle.health = unhealthy` for > 60s | restart (action #1) | 30s |
| 2 | Step 1 fired and stale persists > 90s after restart | reset-state (action #2) | 60s |
| 3 | Step 2 fired and stale persists > 120s | fail-over (action #3) — only if peer instance exists, else skip | 120s |
| 4 | Step 3 unavailable or failed | escalate (action #4) — emit `harness.events crisis.unresolved` per constitution V.4 | (terminal) |
| 5 | Crisis count for this daemon ≥ 3 in 24h | halt (action #5) — `systemctl stop` and `Restart=no` | (terminal) |

Step 5 implements a daemon-scoped variant of the constitution's IV.5
hallucination-and-drift circuit breaker, applied to repair churn rather
than autonomous-commit churn.

### 5.2 Backoff and reset

- Backoff between steps prevents thrashing when restart alone fixes
  transient issues.
- The signal counter resets to step 1 when `lifecycle.health = ok` is
  observed for `reset_window` (default 600s) consecutively.
- Resetting between escalation crises (clearing crisis state) requires an
  operator-authored commit per constitution V.4.

### 5.3 Per-daemon overrides

A daemon MAY override the standard ladder via `~/.config/cosmix/
escalation.toml`. Overrides MUST:

- Not reduce the number of steps below the standard ladder.
- Not skip step 4 (the constitution V.4 crisis) without operator commit.
- Be loaded at meta-supervisor start; runtime mutation through `props.set`
  is forbidden (Operation scope per constitution IV.1).

Overrides are an Operation-scope artifact (constitution IV.1) — autonomous
modification requires Tier 2 with branch + human merge.

### 5.4 Repair events (activity-event format)

Every ladder transition — and every other discrete repair action elsewhere
in this SPEC — MUST emit a structured event using the activity-event
schema defined in SPEC 07 §3.5. The topic is `repair.<verb>`, a
cross-runtime event family per SPEC 07 §3.5.2: subscribers can watch all
repair activity across all daemons with a single `repair.*` subscription.

```
---
bus: 1
type: event
from: noded
command: repair.escalation
topic: repair.escalation
---
{
  "actor": "noded",
  "verb": "escalation",
  "ts": "2026-04-25T18:42:11.244Z",
  "cause": "world.indexd stale 95s after restart",
  "outcome": "ok",
  "duration_ms": 3,
  "details": {
    "svc": "indexd",
    "step": 2,
    "action": "reset-state"
  }
}
```

Field semantics (SPEC 07 §3.5.1 is authoritative):

- `actor` — the meta-supervisor or daemon emitting the event.
- `verb` — the action verb. For ladder transitions this is `escalation`
  with the per-step action carried in `details.action`. For direct
  primitives it is the action name (`restart`, `reset_state`, etc.).
- `cause` — the trigger signal **verbatim** (§8.2 mandate); never
  paraphrased, never summarised.
- `outcome` — `ok` if the action completed; `error` if the action itself
  failed (e.g., `systemctl restart` returned non-zero); `skipped` if a
  precondition was unmet (e.g., fail-over with no peer).
- `duration_ms` — wall time of the action; flags pathologically slow
  restarts before they manifest as ladder thrash.
- `details` — repair-specific structured fields. For `escalation`:
  `{svc, step, action}`. For other verbs: see §5.4.1 below.

#### 5.4.1 Repair verb catalog

The cross-runtime `repair.*` family covers every repair-relevant emission
in this SPEC. Daemons that emit on `<svc>.props.changed` for repair-
relevant transitions (per §8.2) MUST also emit the corresponding
`repair.*` topic so the operator can subscribe to one topic family and
reconstruct the full history.

The catalog uses **Topic** (the ABP `topic` / `command` header) and
**body.verb** (the value of the activity-event body's `verb` field per
SPEC 07 §3.5.1) as separate columns, because SPEC 07 §3.5.2's topic
shape `<family>.<verb>` includes the family prefix while the body
field does not. Implementers reading either column alone cannot derive
the other unambiguously when verbs themselves contain dots
(`breaker.opened`, `deploy.rolled_back`).

| Topic | body.verb | Emitter | `cause` field | Cross-reference |
|---|---|---|---|---|
| `repair.escalation` | `escalation` | meta-supervisor | trigger signal verbatim | §5.1 |
| `repair.restart` | `restart` | meta-supervisor | trigger signal | §2 action #1 |
| `repair.reset_state` | `reset_state` | meta-supervisor | trigger signal | §2 action #2 |
| `repair.fail_over` | `fail_over` | meta-supervisor | trigger signal | §2 action #3 |
| `repair.escalate` | `escalate` | meta-supervisor | trigger signal | §2 action #4; emits `crisis.unresolved` per V.4 |
| `repair.halt` | `halt` | meta-supervisor | trigger signal | §2 action #5 |
| `repair.breaker.opened` | `breaker.opened` | breaker owner | downstream call id or error | §3.1; also emits `<svc>.props.changed` |
| `repair.breaker.closed` | `breaker.closed` | breaker owner | probe call id | §3.1; ditto |
| `repair.retry.exhausted` | `retry.exhausted` | call-site owner | structured downstream error | §3.3 |
| `repair.daemon.crashed` | `daemon.crashed` | systemd journal scraper or daemon pre-exit hook | rc + signal if available | §8.2 |
| `repair.deploy.lkg_rollback` | `deploy.lkg_rollback` | operator script | operator identity | §6.4 |
| `repair.deploy.rolled_back` | `deploy.rolled_back` | deploy script | smoke-test failure | §6.2 |
| `repair.schema.refused` | `schema.refused` | database owner | `code_version` + on-disk version | §7.2 |
| `repair.schema.migrated` | `schema.migrated` | database owner | migration filename | §7.3 |

Per-daemon escalation history is reconstructible by filtering on
`details.svc`. Operator dashboards, audit log writers, and the
constitution's V.5 audit script subscribe to `repair.*` rather than to
the legacy `harness.events` fan-in.

#### 5.4.2 Migration from the pre-3.5 schema

Drafts of this SPEC at version ≤ 0.1.0 used a different field shape
(`{kind, svc, step, action, trigger, ts}`) emitted on the generic
`harness.events` topic. The shape above replaces it; the rename is driven
by SPEC 07 §3.5.6's distinction between activity events (discrete
actions) and `props.changed` events (state transitions). Repair events
are the canonical activity-event use case — collapsing them under a
generic `harness.events` envelope obscured the actor/verb/cause/outcome
shape that downstream subscribers actually need to reason about.

The `harness.events` topic continues to exist for substrate-level events
that pre-date the activity-event taxonomy (e.g., V.4 `crisis.unresolved`
itself); new repair emissions MUST use `repair.<verb>`.

---

## 6. Atomic Deploy and Last-Known-Good

This section addresses the gap between constitution V.1 (every change is
git-revertible) and the running-system reality that a reverted commit does
not roll back a running binary that has already shipped.

### 6.1 Two distinct rollback layers

| Layer | Artifact | Mechanism | Constitution reference |
|---|---|---|---|
| Source | git tree | `git revert <sha>` | V.1, V.2 |
| Running binary | `/opt/cosmix/bin/cosmix-<svc>` | LKG copy + restart (this section) | (this SPEC) |

Reverting source without rolling back the running binary leaves the system
running the bad code; rolling back the binary without reverting source
leaves the next deploy reintroducing the bug. Both layers MUST be kept in
sync; appliers per constitution V.3 are responsible for ordering them.

### 6.2 LKG retention

Before a binary swap, the deployer MUST:

1. Move the existing `/opt/cosmix/bin/cosmix-<svc>` to
   `/opt/cosmix/bin/cosmix-<svc>.lkg` (overwriting any prior LKG).
2. Copy the new binary to `/opt/cosmix/bin/cosmix-<svc>`.
3. Smoke-test the new binary (§6.3).
4. On smoke-test failure: move LKG back to the live path; emit
   `harness.events deploy.rolled_back`; do not restart the service (the
   old binary is already what's running).
5. On smoke-test success: `systemctl --user restart cosmix-<svc>`; observe
   `lifecycle.health` returning to `ok` within a deadline (default 30s);
   on health failure, manually invoke LKG rollback (§6.4).

### 6.3 Smoke test contract

Every cosmix daemon MUST support a `--smoke-test` invocation that:

- Loads its configuration without binding sockets or modifying state.
- Validates the binary is loadable, dependencies are resolvable, schema
  versions are consistent.
- Exits with rc 0 on success, non-zero on any failure.
- Completes within 5 seconds.

Smoke tests are the deploy-time analogue of constitution V.3 verification.
A daemon without a smoke test cannot be safely auto-deployed.

### 6.4 LKG rollback as a repair action

LKG rollback is *not* a step on the standard ladder (§5.1) — it is an
operator-invoked recovery for the case where a deploy failed and the
running binary needs reverting outside the autonomous repair flow.

```sh
mix deployment-tools/lkg_rollback.mix <svc>
```

This script swaps `cosmix-<svc>.lkg` back to `cosmix-<svc>`, restarts via
systemd, and emits `harness.events deploy.lkg_rollback` with the operator
identity in the cause field.

### 6.5 Multi-version coexistence

Cross-mesh deploys may temporarily run mixed versions. This SPEC does not
require synchronised mesh-wide version. SPEC 07's INFO version surfaces
allow consumers to detect and tolerate mismatches. Mesh-wide deploy
coordination is deferred to a future SPEC chapter.

---

## 7. Schema Drift

Every cosmix daemon that owns a SQLite database (indexd, cosmix-lib-skills,
maild's JMAP store, syncd's catalog) MUST treat schema as a versioned
artifact with explicit drift handling.

### 7.1 Schema version property

Each daemon owning a SQLite store MUST expose:

- `schema.version` — integer version of the on-disk schema.
- `schema.code_version` — integer version the running code expects.
- `schema.path` — filesystem path of the database.
- `schema.migration_applied_at` — timestamp of last successful migration.

These are SPEC 07 properties; on mismatch, `lifecycle.health = unhealthy`
and the daemon refuses to open the database.

### 7.2 Migration contract

- Forward migrations (code_version > schema.version) are applied
  automatically at startup, idempotently, in a single transaction. Failure
  rolls back the transaction; daemon exits with rc 20 (constitution V.4
  failure).
- Backward migrations (code_version < schema.version) are NEVER applied
  automatically. The daemon refuses to open the database, emits
  `harness.events schema.refused`, and waits for operator action.
- Breaking migrations (data loss, semantic shift) require an
  operator-authored commit per constitution IV.1 (Operation scope) — the
  migration script itself is a constitutionally-protected artifact.

### 7.3 Migration discipline

Every migration:

- Carries a numeric version: `migrations/0042_add_property_table.sql`.
- Is idempotent: re-running on an already-migrated database is a no-op.
- Is wrapped in `BEGIN EXCLUSIVE; ... COMMIT;` so a partial failure leaves
  the database at the prior version, not in between.
- Logs to `harness.events schema.migrated` on success with old/new
  versions and elapsed time.

---

## 8. Root-Cause-vs-Mask Discipline

This is the most important section of this SPEC and the easiest to violate.

### 8.1 The failure mode

A repair action that succeeds in restoring service can hide the underlying
fault from the operator. An indexd that crashes every 6 hours but is
restarted automatically by systemd appears healthy in `world.indexd` —
until disk fills with crash dumps or an upstream change escalates the
crash rate.

### 8.2 The discipline

Every repair action MUST surface the *signal that triggered it*, not just
the action taken. Specifically:

- The `harness.events escalation` event (§5.4) MUST include the trigger
  signal verbatim, not a paraphrase.
- A daemon emitting `<svc>.props.changed` for `lifecycle.health` MUST
  include in `cause` the signal that caused the transition (e.g., the
  ABP id of the failed downstream call, or the breaker that opened).
- A crash detected by systemd MUST result in a journal entry that survives
  the restart, AND a `harness.events daemon.crashed` event with rc and
  signal if available.

### 8.3 Cross-service causation

A cascade — indexd unhealthy → maild's search request fails → maild's
breaker opens → maild marks itself degraded — MUST be reconstructible from
the `cause` chain. Each daemon in the chain references the upstream cause
in its own `props.changed` event. SPEC 07 §7.4 encourages this; this
section makes it mandatory for repair-relevant transitions.

### 8.4 Crash dump retention

Daemons that produce crash dumps SHOULD write them to
`~/.local/state/cosmix/<svc>/crashes/<ts>.dump` with bounded retention
(default: keep last 10, oldest evicted). The retention bound is a property:
`crashes.retained` and `crashes.evicted_total`. Operator inspection is
manual; no autonomous path may delete crash dumps.

### 8.5 The trap

A daemon that "self-heals" by clearing its DLQ and resetting its breaker
on every restart is hiding failures. The DLQ MUST persist across restarts
(§3.2). Breaker state MAY reset on restart (it represents downstream
liveness, not local state) but the *count of breaker openings since last
operator clear* MUST persist as a property — that count is the signal that
something is recurrently failing even if individual incidents are
auto-resolved.

### 8.6 Worked example — MDS-class repair

`cosmix-mds` (Phase 7, shipped 2026-05-03) is the first concrete daemon
class to instantiate a substantial subset of this SPEC's action space.
This subsection maps the shipped primitives onto §2's actions and the
§5.1 escalation ladder, anchoring the SPEC against a daemon that exists
rather than against contracts that don't yet have an implementation
(§3, §9.3). It is the operational sister to SPEC 07 §8.4 (the
observability worked example for the same daemon class).

Sources: `crates/cosmix-mds/README.md`, the operator runbook at
`historical MDS operator guide (2026-05-03)`, and
`crates/cosmix-mds/tests/recovery.rs` (the SIGKILL recovery proof).

#### 8.6.1 Action space mapping

| Action (§2) | MDS primitive | Notes |
|---|---|---|
| restart | systemd restart of the host daemon (e.g. `cosmix-maild`) | MDS itself is library code; restart is mediated by the consuming daemon. WAL recovery on `SqliteCasMds::open` is the post-restart half — proven safe under SIGKILL by `tests/recovery.rs`. |
| reset-state | `cosmix-mds rebuild-index` (operator CLI) | Rebuilds box-wide `blobs.sqlite` from per-set `data.sqlite` walks. Idempotent; safe to re-run. **Caveat:** orphan CAS blobs (CAS file present, no `blob` row) are *reported* but not deleted — `put_blob` writes the CAS file *before* `add_item` writes the `blob` row, so a SIGKILL between them legitimately leaves an unindexed file. GC sweeps the `blob` table, not the filesystem. Cleanup of unindexed CAS files is operator-driven in v1. |
| fail-over | not yet — single-node | Per-set data lives at `containers/<uuid>/data.sqlite`; cross-mesh fail-over would replicate sets via `export_set` / `import_set`. Tracked under §9.1. |
| escalate | host daemon emits `repair.escalate` when `cosmix-mds verify --full` reports `mismatches > 0` after a recover attempt | Per the operator guide §Recover, manual surgery on individual CAS files is unsafe; the safe remediation is `import_set` from a recent backup, which is operator-mediated. |
| halt | host daemon halts; MDS does nothing autonomously | Storage layer never exits the substrate; only the host daemon does. See §8.6.5 on per-account isolation. |

#### 8.6.2 Standard ladder application

Applied to the maild daemon (the first MDS consumer):

| Step | Trigger | Action | MDS-specific notes |
|---|---|---|---|
| 1 | `world.maild` stale > 60s OR `lifecycle.health = unhealthy` for > 60s | restart maild | Post-restart, MDS re-opens via `SqliteCasMds::open`; SQLite WAL recovery completes before maild signals ready. The Phase 7 SIGKILL recovery test (10 randomised iterations per CI run, kill delay 5–50ms into the `add_item` loop — `tests/recovery.rs:153–158`) is the proof point that this restart path is safe. |
| 2 | Step 1 fired and stale persists > 90s | reset-state — operator runs `cosmix-mds rebuild-index`, then restarts maild | Reset is on the *index* (box-wide blob refs), not the per-set data. Per-set `data.sqlite` files are the source of truth and are not rebuilt. `orphan_blobs_found > 0` after rebuild is informational, not a failure — see the caveat above. |
| 3 | Step 2 fired and stale persists > 120s | fail-over (not yet) — skip to step 4 | Single-node today. |
| 4 | Step 3 unavailable | escalate | `repair.escalate` with `cause = "maild stale after rebuild-index"`; operator runs `cosmix-mds verify --full` to triage. If `mismatches > 0`, restore from the most recent `export` (§8.6.3). |
| 5 | Crisis count ≥ 3 in 24h | halt | But see §8.6.5 — per-account isolation may justify a finer-grained halt scope. |

#### 8.6.3 LKG mechanism for MDS-class daemons

`cosmix-mds export <SET_UUID> <path>` is the set-level analogue of §6's
binary LKG. The export holds the per-set lock for the duration so the
tarball is a point-in-time snapshot of `data.sqlite` plus referenced CAS
blobs. The receiving site uses `cosmix-mds import <path>` to install the
tarball atomically (rename of `data.sqlite` into place, single
transaction inserting `blob_ref` rows into `blobs.sqlite`); the importer
refuses with exit 2 if a set with the same UUID already exists at the
target root.

A nightly `export` of each high-value set is the recommended LKG rhythm
for MDS-class daemons. The operator guide §Backup documents the pattern;
the suggested baseline rhythm in §Health-check pairs every backup with a
preceding `verify --full` so corrupt sets are flagged before they become
the only surviving copy.

The two LKG layers are orthogonal:

- **Binary LKG (§6)** — code rollback. `/opt/cosmix/bin/cosmix-maild.lkg`.
- **Set LKG (this section)** — data rollback. `mds-<UUID>-<date>.tar`.

A bad deploy needs binary LKG; corruption from a hardware event needs
set LKG. The operator chooses by inspecting which symptom the trigger
signal points at.

#### 8.6.4 GC quiescence as a repair-safety primitive

`gc()` runs in two passes with a configurable quiescence wait between
them (`COSMIX_MDS_GC_QUIESCENCE_SECS`, default 60s, floor 5s — values
below the floor are logged at warn and ignored). The wait prevents a
delivery in flight at the start of pass 1 from having its blob collected
before its `add_item` commits — a classic time-of-check / time-of-use
trap that would manifest as silent data loss under concurrent load.

The discipline generalises: any "sweep then act" repair primitive in the
substrate MUST have an equivalent quiescence parameter, and that
parameter MUST be exposed as a SPEC 07 property (`gc.quiescence_secs` in
MDS's case) so the operator can observe what value is in effect without
restarting the daemon to inspect its config. Setting a value with no
property surface is a SPEC 07 §8.4 violation as well as a §8.5 trap
(operator cannot detect that the daemon is running with an unsafe
quiescence override).

#### 8.6.5 Per-account isolation and halt scope

MDS serialises writes per set via an in-process mutex; cross-set ops take
the per-set mutex first, then the box-wide `blobs.sqlite` mutex, never
the inverse. This means corruption confined to one account's
`data.sqlite` does not block writes to other accounts.

The §5.1 step-5 halt action is whole-daemon. For an MDS-class daemon
serving many accounts, a single corrupted set should ideally not halt
service for the rest. The currently-correct behaviour is to escalate the
corrupted set (`repair.escalate` with `details.set_uuid`) and continue
serving other sets, with halt reserved for box-wide `blobs.sqlite`
corruption or repeated whole-daemon restarts. The meta-supervisor
contract for per-tenant halt scope is open — see §9.6.

#### 8.6.6 Post-commit emission as a §8.2 cause-chain enabler

§8.2 mandates that repair events carry the trigger signal verbatim in
`cause`. MDS's *post-commit emission* discipline — every `MdsEvent` and
in-process `Notifier` event fires *after* the SQLite transaction commits
— is what makes downstream `cause` chains trustworthy: a subscriber that
receives an MDS event is guaranteed the underlying durable change
happened. Pre-commit emission would let `cause` references point at
half-applied state, breaking the §8.3 cross-service causation contract.

This is a generalisable invariant for any ABP event referenced as a
`cause`: emit post-commit on the durable write that makes the cause
true. The MDS pattern is the reference shape — `EventSink` is invoked
after the per-set transaction commits in `store.rs`, never before.

#### 8.6.7 Why this matters for the SPEC

MDS instantiates more of this SPEC's action space than any prior daemon
— not because it is special, but because *storage substrates have no
choice* about repair: a corrupt index is not a "degraded health"
condition that can be patched over, and silent data loss is not a
recoverable error mode. The MDS pattern (verify ledger, idempotent
rebuild, atomic export/import, post-commit emission, quiescence on
multi-pass operations, per-tenant lock isolation) is the reference shape
for any future MDS-class daemon — the skills store, the knowledge store,
the ABP audit log, the syncd catalog, and any other component that
custodies durable per-tenant state.

---

## 9. Open Questions and Future Work

### 9.1 Cross-mesh repair coordination

The standard ladder's step 3 (fail-over) requires a peer instance. Today
each service runs on at most one mesh node. When indexd ships on multiple
nodes, the ladder will need:

- A way to enumerate viable peers (`world.<svc>@<node>` snapshots from
  other brokers).
- A draining protocol so that a fail-over does not double-process queued
  work.
- A return protocol so the original instance can be re-promoted after
  recovery.

Deferred until the second mesh node is in production.

### 9.2 Operator-bypass for known-good repair

The constitution V.4 crisis state requires operator-authored commit to
clear. A pattern of identical, well-understood crises (e.g., "indexd
crashes once a week due to an upstream model file rotation") creates
noise. A future SPEC amendment may permit operator pre-authorisation of a
repair pattern: signed metadata declaring "if signal X is the trigger,
auto-clear on success." Not in this draft — the bias is toward operator
attention rather than away from it.

### 9.3 cosmix-lib-core extraction

The primitives in §3 are specified as contracts because the implementing
crate does not yet exist. Extracting `cosmix-lib-core` from indexd's
existing breaker logic is a near-term task; the SPEC will be amended to
reference the crate's actual API once it lands.

### 9.4 Watchdog-vs-heartbeat unification

§4.2 distinguishes daemons using systemd `WatchdogSec=` from daemons
emitting `world.<svc>` heartbeats. Both are liveness signals; running both
in parallel is redundant. A future iteration may unify them — `world.<svc>`
republish drives systemd watchdog satisfaction via a shim, so daemons emit
once and both surfaces work. Deferred.

### 9.5 Per-tenant halt scope for MDS-class daemons

§8.6.5 surfaces the gap: §5.1 step 5 (halt) is whole-daemon, but for an
MDS-class daemon serving many accounts a single corrupted set should not
halt service for the rest. A future SPEC amendment may introduce a
*scoped halt* primitive — drain and refuse work for an enumerated subset
(by `set_uuid`, `tenant_id`, or analogous tenancy key) while continuing
to serve others. The shape question is whether scoped halt is a sixth
action, a parameter of action #5, or a meta-supervisor concern that the
SPEC stays out of. Deferred until a second MDS consumer (skills store,
knowledge store) makes the per-tenant pattern concrete.

### 9.6 Repair-event subscription cardinality

§5.4 routes all repair emissions through `repair.<verb>`. Operator
dashboards subscribing to `repair.*` see every repair event substrate-
wide; on a busy node this can be high-cardinality. Whether subscribers
need per-daemon `<svc>.repair.<verb>` aliases (mirroring SPEC 07 §3.5.2's
per-daemon vs cross-runtime split for activity events) or whether a
single family topic with `details.svc` filtering is sufficient is open.
Resolution is informed by the same evidence as SPEC 07 §10's open
question on activity-event topic naming — keep the answers aligned.

### 9.7 Promotion to stable

This SPEC moves from `draft` to `stable` when:

- (a) `cosmix-lib-core` ships with the §3 primitives extracted.
- (b) The standard ladder (§5.1) is implemented in a meta-supervisor and
  has run for ≥ 60 days against real signals.
- (c) Schema drift handling (§7) is implemented in indexd and at least one
  other daemon.
- (d) §6 LKG retention has caught at least one bad deploy (the proof point
  that the mechanism works under real conditions).

---

*Document created: 2026-04-25. Drafted in collaboration between Mark
Constable and Claude Opus 4.7 as the second of three substrate-layer
SPECs (self-aware / self-repair / self-improve).*

*§5.4 reconciled with SPEC 07 §3.5 activity-event taxonomy and §8.6
worked example added 2026-05-03 — sister to SPEC 07 §8.4. Driver: the
Phase 7 cosmix-mds release shipped concrete repair primitives (verify,
rebuild_index as reset-state, gc with quiescence, SIGKILL recovery test,
export/import as set-level LKG, post-commit emission discipline) which
this SPEC could now anchor against rather than against contracts whose
implementing crate (cosmix-lib-core, §9.3) does not yet exist. New open
questions §9.5 (per-tenant halt scope) and §9.6 (repair-event
subscription cardinality) surfaced during the §8.6 drafting.*

## Part B — Gated improvement (legacy chapter 09)

The following section numbers refer to Part B. Calls back to repair §6/§7 refer to Part A. Neither proposal acceptance nor an L3 content grant overrides the invocation's actual resource scope and authorisation.

## 1. Purpose and Non-Goals

**Purpose.** Specify the mechanism by which Cosmix improves itself: how
signals become proposals, how proposals are triaged, how applications are
gated by trust class, how outcomes feed back into future selection. Bind the
mechanism tightly enough that an agent reading this SPEC can implement and
operate the loop without consulting the operator on routine cases.

**Non-goals.**

- **Replace the constitution.** Article IV defines tier ladder and policy
  matrix; this SPEC defines mechanism within those constraints. Where they
  appear to conflict, the constitution wins.
- **Specify the renderer or display surface for proposal review.** That
  belongs in a future GUI SPEC. The contract here is over ABP topics and
  capability namespaces; any surface meeting the contract is conformant.
- **Settle the autonomous-rebuild question.** Whether the substrate will
  ever apply code-modifying proposals without explicit approval is a
  constitutional question (Article IV). This SPEC enumerates the gating
  required *if* such autonomy is ever extended; it does not advocate for it.
- **Re-derive AI-safety canon.** The genuine risks here are operational:
  feedback collapse, drift, identity confusion, irreversible application.
  ARA-frontier framings do not apply (cf. project memory
  `project_substrate_layer_split.md`).

---

## 2. The Five-Stage Loop

Every improvement traverses five stages. Each stage has a dedicated topic
family on ABP and a dedicated capability on the originating service.

```
observe → propose → triage → apply → learn-back
   ▲                                       │
   └───────────────────────────────────────┘
```

| Stage | Trigger | Surface | Output |
|---|---|---|---|
| **observe** | signal from SPEC 07 (props.changed, world.*, harness.events) or human input | service-internal | candidate set |
| **propose** | candidate ranked above threshold | `improve.proposals` topic + Proposal record | proposal id |
| **triage** | proposal received | triage capability per service + `improve.triage` topic | accept / defer / reject + rationale |
| **apply** | triage = accept AND gate satisfied | service-internal + `improve.applied` topic | applied id + outcome record |
| **learn-back** | outcome observed within window | feedback capability + `improve.outcomes` topic | adjusted scoring for future observe |

**Idempotency.** Every stage MUST be idempotent on its input identifier.
Replaying a stage with the same id MUST NOT produce duplicate side effects.
This is the substrate's hedge against the loop firing twice during a partial
restart.

**Audit trailer.** Every transition MUST emit an audit trailer per
constitution Article IV.4, with `proposal_id`, `from_stage`, `to_stage`, and
`gate_class` (see §3).

---

## 3. Change Classes and the Trust Gradient

Five change classes, each with explicit gating. The class is determined at
the **propose** stage and travels with the proposal through all subsequent
stages. The class MUST NOT be downgraded after triage; if a proposal turns
out to require a higher class, it MUST be rejected and re-proposed.

| Class | Examples | Gating | Reversal |
|---|---|---|---|
| **L0 — read-only learning** | scoring updates, retrieval-rank tweaks, summarisation cache | automatic; no review | discard cache |
| **L1 — proposing** | a candidate skill, a candidate config change, a candidate doc edit | automatic to surface; no apply | drop proposal |
| **L2 — first execution** | first run of a newly proposed skill or action | **human-in-loop** (explicit accept) | abort / undo per skill |
| **L3 — content-addressed re-execution** | nth run of a skill whose content hash matches a previously approved hash | TOFU (trust on first use) bound to `<name>:<sha256-of-content>` | revert by content hash; refuse hash drift |
| **L4 — code-modifying / destructive** | binary swap, schema migration, ABP topic deletion, constitution edit, `cosmix-*.lkg` rotation, secret rotation | explicit operator approval per occurrence; quorum per §6 if autonomous quorum is ever enabled | per SPEC 08 §6 atomic deploy + LKG |

**The L2 → L3 transition is the only place TOFU is granted.** All other
class transitions are forbidden. A skill at L3 that is refined into a new
content (different sha256) re-enters at L2 — the previous TOFU does not
transfer (cf. §4).

**Article VI invariants override class.** A proposal whose target is on the
forbidden-targets list (constitution VI) is rejected at triage regardless
of class, with cause = `forbidden_target`.

### 3.1 Tool-surface authority — a projection of the change-class gradient

The L0–L4 gradient above governs *change proposals* (skills, configs,
code edits) — artifacts that travel through the five-stage loop (§2).
The same gradient also governs *agent tool surfaces*: any tool exposed
to an agent runtime (cf. SPEC 07 §3.5.7) is classified by the same
gradient at registration time, and the runtime MUST refuse to invoke a
tool whose authority class exceeds the runtime's mounted ceiling.

Tool surfaces project onto a **subset** of the gradient, because L1 and
L3 are skill-lifecycle concepts that don't exist at single-call
granularity:

| Class | Tool meaning | Examples |
|---|---|---|
| **L0 — read-only** | The tool reads substrate state; no observable side effect outside the agent's view. | `props.get`, `world.<svc>` snapshot, `context_search`, `log_search`, `bus_list_services`. |
| **L2 — staged write** | The tool mutates substrate state but only through a path that supports dry-run and is scoped to a bounded blast radius (single workspace, single set, single file under an explicit path). First invocation of a newly-mounted L2 provider requires operator acceptance per §3 L2. | `index_workspace` (deletes + re-stores under a workspace root), `write_file` (path-scoped), `mds.export` to an operator-supplied path. |
| **L4 — code-modifying or unbounded write** | The tool mutates code, schemas, secrets, or unbounded substrate state. | `bus_call` to arbitrary service, `mix_execute`, binary swap, schema migration, ABP topic deletion, secret rotation, `cosmix-*.lkg` rotation. |

L1 (proposing) and L3 (TOFU on content-addressed re-execution) are the
skill-lifecycle states a *proposal* passes through; a tool *invocation*
is one event in time and projects onto L0/L2/L4. A skill execution is
the special case where an L3 *proposal* state is enacted via an L2 or
L4 *tool invocation* — the gradient agrees on what the action is even
when the lifecycle state and the call state differ.

**Why the same gradient, not a parallel one.** A tool that ultimately
swaps a binary is L4 in both senses — the danger doesn't change because
the artifact is invoked rather than proposed. Inventing a parallel
`Authority::*` taxonomy would make every audit join (constitution V.5)
have to reconcile two enums for the same concept. The implementation in
`cosmix-lib-tools` (per the unification plan) uses the L0/L2/L4 enum
exactly because this SPEC owns the gradient.

**Catalog vs mount.** A runtime maintains two distinct registers:

- The **catalog** lists every provider known to the runtime, regardless
  of authority class. Catalog entries appear in `tools.list` (or the
  equivalent agent-facing enumeration) so the agent can see *what
  exists* and reason about it.
- The **mount set** is the subset of catalog entries the runtime will
  actually invoke. Membership is governed by the runtime's
  authority-class ceiling (declared at startup) and by per-provider
  grants.

A provider whose `authority()` exceeds the runtime's ceiling MUST be
catalogued but MUST NOT be mounted. The runtime MUST NOT silently omit
it. In `tools.list` it appears with `gated_authority: <class>` and
`mountable: false`, plus a stable `grant_token` the agent can present
when requesting a per-call grant. Invocation of an un-mounted provider
returns `Refused { reason: "authority_gated", required: <class> }` —
not "tool not found". This refusal-with-context is what lets an agent
know to ask for an upgrade rather than work around the absence.

**Defaults.** `cosmix-mcp` (in-the-loop operator implicit) ceilings at
L4; every provider is catalogued *and* mounted. `cosmix-agentd`
(autonomous) ceilings at L2; L4 providers are catalogued but not
mounted, awaiting a per-call grant from the operator (Tier 1
acceptance per constitution IV.2) or a session-scoped policy upgrade.

**Why startup-failure isn't the right default.** An earlier draft of
this section required over-ceiling providers to refuse registration as
a startup error. That made every agentd startup against the full
provider catalog impossible, contradicting the unification plan's
explicit goal of mounting one shared catalog in both runtimes. The
catalog/mount split preserves the *visibility* property (operator and
agent both see the gap) without forcing it through a registration
failure that would conflate "this tool is absent" with "this tool is
gated."

**Cross-reference.** Per-tool capability scoping (paths, set UUIDs,
HTTP origins) is orthogonal to authority class — a tool can be L2 with
a workspace-root scope, L2 with a per-set scope, etc. The
unification plan's `CapabilityRequest` carries the scope; this SPEC's
class carries the gradient. Both apply at every invocation.

---

## 4. Skill Content Addressing

Skills are the canonical L3 case. Their identity MUST be bound on **content
hash**, not name. Otherwise L3's "trust on first use" degrades into "trust
all future regenerations of anything called X" — a drift surface the
constitution's audit trailers cannot detect.

**Identity.** A skill's pinned identity is `<name>:<sha256-of-canonical-content>`,
where canonical-content is the deterministic serialisation defined in the
skills-store contract (TBD; see §10 open question). The bare name is a
convenience handle, not an identity.

**TOFU rule.** Granting L3 to `skill_X:hash_A` does NOT grant L3 to
`skill_X:hash_B`. The latter is L2 and re-prompts the operator on first
execution. This applies regardless of how the content changed (manual
refinement, automated refinement, store migration, model-version drift).

**Storage contract.** The skills store MUST persist the content hash with
each skill record and MUST refuse to return a skill under a TOFU grant if
the stored hash does not match the granted hash. The grant record itself is
indexed by `<name>:<hash>` and has no fallback.

**`skills_refine` semantics.** Refining a skill produces a new content and
therefore a new hash. The grant on the old hash is preserved (audit) but
no longer applicable to retrieval; the new hash starts at L2.

---

## 5. Feedback-Collapse Guard

The current skills loop has a known feedback-collapse problem: `skills_refine`
is called by the same agent that just executed the skill. The agent that
ran the skill is not an independent evaluator — it has every incentive to
confirm its own selection.

**Rule.** `skills_refine` (and any equivalent learn-back signal) MUST NOT
be the sole input to L1 → L2 promotion or to L3 grant renewal. At least one
of the following independent signals MUST also be present:

- a different agent (different session id, different operator, or a
  designated evaluator agent) ran the same skill on a related task and
  emitted a refine signal;
- an out-of-band check (test pass, schema validation, lint pass) succeeded;
- a downstream consumer (different service in the mesh) emitted a
  positive `world.*` signal correlated to the skill's effect window.

**Implementation hook.** The triage stage (§2) MUST read the proposal's
signal set and reject (or defer) any proposal whose only positive signal
is from the originating agent identity. The cause field is `feedback_collapse`.

**Why this isn't paranoia.** A skill that survives only because its author
keeps confirming it has not been validated; it has been laundered. The
substrate's modifiability criterion requires that improvements be
*detectable improvements*, which requires independent measurement.

---

## 6. Multi-Agent Quorum (For L4 Code-Modifying Proposals)

L4 proposals (code-modifying or destructive) require explicit operator
approval per occurrence under the current constitution (Article IV.2). If
constitution Article IV is ever amended to permit autonomous L4 application,
the gate MUST satisfy the minimum quorum contract below.

**Minimum contract for quorum (when enabled).**

- At least three agents from non-correlated sessions (different model
  versions or different operator origins) MUST independently approve.
- Each agent MUST have read the proposal, its diff, its triage record, and
  the affected SPEC chapter(s).
- A single dissent MUST defer the proposal to operator review; quorum is
  not majority-rule.
- The quorum record (agent identities, timestamps, signed approvals) MUST
  be persisted as an audit artefact and is itself L4 (immutable).

**Until the constitution permits autonomous L4, this section is dormant.**
It is specified now so that any future amendment has a concrete mechanism
to point at, rather than re-litigating quorum at amendment time.

---

## 7. Rollback for Self-Modifying Components

L4 changes that modify executables, schemas, or specs MUST be revertible
through the SPEC 08 §6 atomic deploy + LKG contract. This SPEC adds three
proposal-side requirements:

1. **Pre-apply LKG capture.** Before applying an L4 proposal, the apply
   stage MUST verify that an LKG snapshot of every artefact the proposal
   touches exists and is healthy (smoke-test rc 0).
2. **Apply window.** L4 application MUST occur within a single ABP
   transaction window: either all artefacts swap or none. Partial L4 is
   forbidden; on partial failure, SPEC 08 §6 rollback applies.
3. **Revert proposal.** Every applied L4 proposal MUST emit a
   `revert_proposal_id` referring to a pre-generated, validated reversal.
   The reversal does not itself need approval at apply time — it inherits
   the original's grant. This makes Article IV.5 circuit-breaker reverts
   mechanical rather than discretionary.

Schema migrations follow SPEC 08 §7 drift discipline: forward auto, backward
refused, breaking requires operator. Self-improve does not relax those
rules; it only enforces that L4 schema proposals carry a backward-compatible
revert proposal alongside.

---

## 8. Constitution Interlock

This SPEC operates within constitution Article IV's policy matrix. The
mapping:

| Constitution clause | This SPEC's mechanism |
|---|---|
| **IV.1 propose-only autonomy** | L1 (propose) automatic; surfaces to `improve.proposals` topic |
| **IV.2 first-execution gate** | L2 → L3 transition; TOFU on content hash (§4) |
| **IV.3 scope classes** | proposal record carries scope class; triage rejects mismatched class+target combinations |
| **IV.4 audit trailers** | every stage transition emits trailer (§2) |
| **IV.5 circuit breaker** | revert_threshold/window_days/pause_hours apply at the apply stage; revert_proposal_id (§7) makes breaker mechanical |
| **VI forbidden targets** | triage rejects with cause = forbidden_target regardless of class |
| **VII amendment process** | constitution edits are L4 by definition; require operator-authored commit per VII.1 |

**Historical amendment mechanism — superseded procedure.** The old constitution
required a human-authored commit, a prior-version reference and a private changelog
entry, while prohibiting autonomous proposals. The proposal mechanism above did
not reconcile that prohibition. The [accepted handover](authority-handover.md)
now maps the protected targets and procedure: operator-authorised changes can be
agent-prepared and honestly attributed; agents cannot accept their own amendments.
The original constitution is preserved, not edited or renamed to bypass a gate.

---

## 9. Required Topics and Capabilities

**Topics (retained snapshots per SPEC 03).**

- `improve.proposals.<service>` — open proposals targeting `<service>`
- `improve.triage.<service>` — triage decisions
- `improve.applied.<service>` — apply outcomes
- `improve.outcomes.<service>` — learn-back records
- `improve.grants` — active L3 grants by `<name>:<hash>` (mesh-wide)
- `improve.quorum` — quorum records, when §6 is enabled

**Capabilities (per service, discoverable via SPEC 07 §5 `spec.get`).**

- `<svc>.improve.propose` — submit a proposal (returns proposal_id)
- `<svc>.improve.triage` — request triage (idempotent on proposal_id)
- `<svc>.improve.apply` — apply an accepted proposal (idempotent)
- `<svc>.improve.refine` — submit learn-back signal (subject to §5 guard)

A service that bears no improvement surface (e.g., a pure transit daemon)
MAY omit these capabilities, but MUST advertise the omission via
`spec.get` so agents do not retry.

---

## 10. Open Questions

- **Rejection ledger.** Where do rejected proposals live? Per-service
  topic, or a mesh-wide ledger? An agent that re-proposes a rejected idea
  needs a way to find the prior rejection; otherwise rejection is silent
  and the loop wastes triage cycles.
- **First-execution human gate surface.** L2 requires human-in-loop, but
  the surface is not specified here (intentionally — display SPEC pending).
  The contract is "operator must explicitly accept"; the implementation
  may be a CLI prompt, a desktop notification, an SMS, or a queued review
  list. Which is canonical?
- **Quorum applicability.** §6 enumerates the contract for autonomous L4
  quorum, but the constitution does not currently permit autonomous L4.
  Should §6 be removed until the constitution permits it, or kept as
  forward provision? Current draft keeps it as forward provision.
- **Skill canonicalisation.** §4 references "deterministic serialisation
  defined in the skills-store contract" — that contract does not yet
  fully specify whitespace, metadata ordering, or refinement-history
  inclusion. Pending in the skills-store implementation.
- **Cross-mesh proposals.** A proposal originating on mesh node A
  affecting service on node B: who triages, who applies, where the audit
  trailer lives. Defer to a cross-mesh extension SPEC.
- **`historical self-updating-systems exploration (2026-04-20)` retirement.**
  When does the doc move to historical? Suggested: when SPEC 09 reaches
  status `stable` and at least one service implements the full loop end
  to end.

---

## Conformance

A service is **L0-conformant** if it does not participate in the improve
loop and advertises the omission via `spec.get`.

A service is **L1-conformant** if it emits proposals on `improve.proposals.<svc>`
with valid Proposal records and accepts triage decisions on
`improve.triage.<svc>`.

A service is **L2-conformant** if it additionally enforces the first-execution
human-in-loop gate for L2 proposals targeting itself, including refusal to
apply absent an explicit operator acceptance record.

A service is **L3-conformant** if it additionally honours content-addressed
TOFU per §4: storing hashes, refusing drift, and re-prompting on hash
change.

A service is **L4-conformant** if it additionally meets §7 rollback
requirements: pre-apply LKG verification, atomic apply window, and
revert_proposal_id emission.

The mesh as a whole is conformant at the lowest conformance level of any
participating service.

---

*§3.1 (tool-surface authority projection) added 2026-05-03 to make the
L0–L4 change-class gradient the canonical taxonomy for agent tool
surfaces as well as change proposals. Driver: the agentd ↔ mcp
unification plan (`historical agent-runtime unification plan (2026-05-03)`) introduces an `Authority::L0/L2/L4` enum on the shared
`ToolProvider` trait; without a SPEC home the implementation would have
been a parallel taxonomy that future runtimes would have had to
reconcile. Pinning it here keeps every audit chain (constitution V.5)
joining on a single gradient.*
