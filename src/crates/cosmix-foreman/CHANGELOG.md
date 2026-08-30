# Changelog

## Halted 2026-08-30 — until further notice

Development of `cosmix-foreman` stopped at 0.24.10 (Mark, 2026-08-30): two
weeks of fleet-on-fleet work consumed the budget the product needed. The crate
remains a workspace member so the tree keeps building; no further releases,
deployments, or fleet runs are planned until further notice.

## 0.20.4

- Fix a regression shipped in 0.17.0: the review token cap bound cumulative
  INPUT as well as output, so every landing review was killed on its first
  usage event after emitting 15-30 output tokens and the fleet landed nothing
  for a day. `reserve_tokens` is the per-run hold against the daily OUTPUT
  ceiling and cannot also be a maximum input — a review's prompt carries only
  a changed-file index, so the reviewer opens files itself and each turn
  re-sends cached context; reviews above 500k input delivered 179 times
  between 2026-08-18 and 08-27, up to 8.8M. No reserve value satisfies both
  roles: one large enough for real reviews exceeds the whole daily ceiling.
  An input bound, if wanted, needs its own number counted on fresh input and
  reported distinctly from a vendor refusal (task 96).

## Unreleased

- Make task claim leases observable and renewable instead of a claim-time-only
  timestamp. Claims now return `lease_until`; local runs heartbeat every five
  minutes, PID-less MCP/remote workers can renew through `task_heartbeat`, and
  all generation-fenced release paths clear the lease atomically. The 24-hour
  window exceeds the longest verifier path and tolerates mesh outages without
  stealing healthy work.
- Bind every production reopen to the primary ledger object's device and inode,
  verified from SQLite's opened descriptor. A pathname repointed at another
  valid same-project database is now a hard refusal before WAL setup or ledger
  I/O, preventing accidental split-brain while retaining SQLite's normal
  pathname and sidecar locking semantics.
- Prune a landed task's own recorded branch from `push_remote` after the
  verified integration update succeeds. The bounded delete records its result
  in the distinct delete journal row, refuses caller-selected refspecs before
  Git runs, and preserves ambiguous exits as non-retryable `unknown`.
- Honour the project manifest's new `push_remote` key after a successful
  local landing CAS. The remote integration update uses the journalled
  `<verified-sha>:refs/heads/<integration>` refspec, never a mutable branch
  name, and records the bounded runner's `succeeded`, provable `failed`, or
  ambiguous `unknown` result back into the intent row. Remote Git receives
  only one complete non-empty credential set selected from manifest `lanes`;
  an omitted `push_remote` is reported as an explicit no-op.
- Add schema-18's durable push-intent journal. Each remote-configured landing
  commits an immutable verified-SHA update refspec and a distinct task-branch
  deletion intent before the local integration ref advances. Recovery exposes failed
  intents to an injected replayer only after a guarded, durable
  `failed -> unknown` claim; ambiguous `unknown` outcomes remain report-only,
  including after a crash during replay.
- Add the read-only `attachment-harm` transcript analyser. It streams Claude
  JSONL records, fails closed when a file attachment's whole/slice extent is
  unknown, deduplicates boundary/summary compaction pairs, joins Foreman runs
  by exact `runs.session_ref`, and reports operator and Foreman populations
  separately. Existing-file work is ranked by observed post-compaction
  reattachment count, requiring the same file in an earlier compaction epoch;
  record/content size is context only, and no transcript content is retained
  or emitted. Opt-in private-corpus tests reproduce the known session and emit
  the live top ten plus task 111/112/113 coverage without persisting results.
- Add a bounded, argv-only remote Git runner with a cleared child environment,
  non-interactive Git/SSH policy, capped concurrent output capture and
  process-group timeout cleanup. Remote delivery now has an explicit
  `succeeded` / `failed` / `unknown` taxonomy; spawn refusal is a provable
  `failed`, while timeouts, signals, post-spawn I/O trouble and ambiguous
  non-zero exits remain `unknown`, never retry-safe failures.
- Require reasons for operator-driven reservation and release, filing each
  state transition atomically as an info finding while suppressing duplicate
  findings for no-op sets. Mark unexplained legacy reservations in dispatch
  summaries and fleet status, and clear stale operator-driven flags on landing
  (including schema-17 reconciliation of already-landed rows).
- Decouple implementation-ladder entry from task risk. Every risk now starts
  at scalar `start_rung` (default 0), charged failures alone climb the ladder,
  and an explicit rung outside the resolved ladder is a load error rather than
  a silent clamp. `FOREMAN_START_RUNG` provides the one-shot override.
  `task add --budget` admission now checks for a dollar-metering rung
  reachable from `start_rung`, not anywhere in the ladder.
- Split the merge-review silence budget by lane: streaming Claude remains at
  300 seconds while silent-reasoning Codex defaults to 900 seconds. Both are
  operator-configurable through `foreman.conf.mix` and environment overrides.
  Harness-owned stall, wall, and token kills now record `harness_error` with a
  cause-specific report instead of looking like vendor `resource_exhausted`.
- Reclaim a landed task's allocated Cargo scratch immediately while retaining
  its worktree and tracked files. Add `gc-scratch` as the timer-driven
  backstop for old terminal rows, with dry-run reporting, exact target-root
  containment, a generous default shared-cache cap, replayable age selection,
  and real ZFS-capacity escalation for younger terminal scratch. Running,
  landing and operator-driven tasks are never selected. Ship the daily user
  timer/service and its environment example.
- Close two remaining gaps in scratch-gc reclaim: a leased reclaim now
  revalidates its `SCRATCH_GC_CLAIMANT` lease before removing each of a
  task's (at most two) candidate directories, so a `--force` requeue racing a
  live sweep can only let an already-in-flight `remove_dir_all` finish, never
  start deleting a second candidate the requeue has already handed back to a
  live run. A real (non-`--dry-run`) `gc-scratch` invocation now also
  requires `--confirm`; the installed timer's `ExecStart` always passes it,
  so the bare command name previews instead of deleting when a caller
  unfamiliar with it runs it directly against a live fleet. Document stuck
  scratch-cleanup lease detection and recovery (`task requeue --force`).
- Make the scratch-cleanup lease an enforceable interlock rather than an
  advisory one. `begin_scratch_cleanup` now stamps the lease with the
  reclaiming process's `(pid, /proc starttime)`
  (`foreman-scratch-gc:pid=<pid>:start=<t>`), and `requeue_task` refuses to
  clear it — **with or without `--force`** — for as long as that pid is
  actually running. Revalidating between candidate directories was never
  sufficient on its own: a check and a `remove_dir_all` are two operations,
  so a `--force` landing just after a successful check could still dispatch
  a live agent into a deletion already in flight. The override is now the
  host's decision, not the flag's; a sweep that died mid-lease is not alive,
  so the documented crashed-sweep recovery still works and no longer asks the
  operator to judge whether the process is dead. A wedged live sweep is
  recovered by killing the pid the refusal names. `end_scratch_cleanup` and
  the sweep's revalidation are guarded on the exact stamp, so a reassigned
  lease is neither mistaken for ours nor clobbered by a late release.
- Refuse to gc a shared `target`/`target-refine` cache that has not been
  proven untracked and gitignored. The cache gc deletes any entry under
  `{debug,release}/{deps,build,.fingerprint}`, and a fleet root routinely
  sits inside a Git checkout, so a tracked file there was eligible for
  deletion — against the hard prohibition on removing tracked files. Shared
  caches now pass the same Git proof as a task directory (no tracked file
  beneath the root, root itself ignored, inconclusive probe = refusal), in
  `--dry-run` as well as for real, so a preview cannot advertise candidates
  the sweep would refuse.
- Replace the exported `scratch::reclaim_task_scratch` with plan-only
  `scratch::plan_task_scratch`. The old helper deleted for real from a
  caller-supplied task snapshot, which was a way around
  `begin_scratch_cleanup` — the reservation that the entire safety argument
  rests on. Deletion is now reachable only through
  `reclaim_task_scratch_leased`; the module exports a planner and an arbiter,
  and nothing in between.
- Fix task 94: a dead dispatch supervisor left its claim `running` forever —
  no reaper, and `lease_until` was schema-only and never populated. Two
  independent gaps, both closed:

  **1. Two structural asymmetries in the runner's release path.** Every
  run-path disposition lives inside one `catch`-shaped closure in
  `runner::run_task_with_clock_and_policy`; a driver failure INSIDE the
  closure is turned into an outcome and dispositioned (and released)
  through `finish_task_classified_at`, and the outer `match` on the
  closure's result is the only other thing that releases a claim on the way
  out. Audit of every run-ending path, and which released BEFORE this fix:

  | Path | Released before fix? |
  |---|---|
  | Normal completion (`done`) | yes |
  | Tier-0 verifier red (`bounced`) | yes |
  | Branch contract broken (`bounced`) | yes |
  | Verifier engine could not run (`failed`, infra) | yes |
  | Driver/harness error from `drive()`, ordinary | yes |
  | Driver/harness error from `drive()`, SQLite-busy-exhausted | yes (`finish_infrastructure_failure_at` via the outer match) |
  | Agent-abandoned-background disposal | yes |
  | Budget refusal (`executor.check_budget`) | n/a — fails before the claim is ever taken |
  | Claim itself refused | n/a — nothing was claimed |
  | **Seq-0 "rebase" ledger-event write** (recorded right after claiming, before `drive()` starts) | **NO** — this write sat *outside* the closure, so its `?` returned straight out of the function, skipping every disposal arm |
  | **Verifier-profile resolution** (the task's profile name looked up right after claiming) | **NO** — same structural position as the rebase write: an unknown or removed profile `?`-returned out of the function with the task claimed and `running`. Nor could the reaper recover it — the supervisor process is still alive, and liveness is the reaping predicate — so the strand outlived the run and only an operator could clear it |
  | **A REPORTING write inside the closure failing non-busily** — the branch-contract finding, the tier-0 finding, `record_run_verification`, the sccache-bypass finding, `set_run_quality`, `finish_run`, the abandoned-background disposal, or `finish_task_classified_at` itself | **NO** — `ledger_write_with_busy_retry` only retries SQLite-busy errors (every other failure "retains its original error and fails immediately"), so a non-busy failure `?`-escaped the closure into an outer `match` whose only disposal arm matched busy-exhausted errors. Everything else fell through the pass-through arm with the run over and the task still claimed and `running` |

  Both post-claim rows are fixed the same way — by moving the work inside
  the closure, not by adding a catch around it. Profile resolution cannot
  move to *before* the claim instead: the profile name is a column of the
  task the claim returns, and reading it unclaimed answers for a task that
  may be claimed by someone else a moment later.

  The seq-0 row is the actual reported incident: run 425 died writing that
  rebase event and stranded task 70 `running` for 31 hours, while run 535
  hit the same *kind* of failure (a ledger-event append) inside `drive()`
  and released task 82 correctly — same failure class, different structural
  position relative to the closure. The rebase-event write now happens as
  the closure's first statement and shares its disposal path exactly like
  every other write.

  The reporting-write row is the same gap one step later, found by auditing
  rather than by an incident, and it is why the fix is not "move one write":
  the outer arm now disposes of **any** error escaping the closure, not
  only SQLite-busy-exhausted ones. A run that ends while reporting an
  outcome it already reached is still a run that ended — the claim comes
  back, the run row is recorded as a harness error, the ladder is not
  charged, and the original error still surfaces to the caller. A
  disposition that already committed sets `claim_released`, so the outer
  arm reports without trying to release a claim that is already gone.

  **2. No reaper for a claim whose process is simply gone** (the dispatch
  supervisor itself crashed, was OOM-killed, or the host rebooted — no
  runner code ever gets to run in that case, so no error-handling fix could
  have covered it). `tasks.lease_until` is now populated at claim time (a
  generous 6-hour lease — `ledger::CLAIM_LEASE_SECS`); `foreman dispatch`
  calls the new `Ledger::reap_dead_claims_with` once per sweep, before
  planning, which requeues a claim whose lease has expired AND whose
  claimant process is confirmed dead. Liveness is the actual predicate, not
  the lease age — a long-lived but live claim is never touched (proven with
  the real check against a live pid, and separately against an injected
  liveness answer so the reap decision is a pure function of ledger state,
  never of unrelated pids on the test host).

  Process liveness is an **observation of the host**, and the only sweep
  input a later reader cannot re-derive — a process that was gone at sweep
  time is gone permanently, one that was alive may be gone by the next
  look. So it is treated like every other such input here: **supplied at
  the seam** (dispatch hands in `procutil::owner_alive` beside its recorded
  `sweep_now`, so both inputs are visible at the call site and a replay
  supplies the recorded answer for each), and **written down when it
  decides anything**. Every reap files a `major`-severity (never `blocker`,
  so it parks nothing) `dead_claim_reaped`
  finding recording the dead claimant, the pid observed absent, the instant
  it was observed, how long the claim had been HELD, and how far past its
  lease it was — the claim age and the lease-overdue time differ by up to
  the whole six-hour lease window, so both are reported rather than one
  standing in for the other. The claim time comes from a new
  `tasks.claimed_at` column written at claim time, not back-derived from
  `lease_until - CLAIM_LEASE_SECS` (a derivation that silently lies about
  any claim taken before a release that changed the constant) and not from
  `updated_at` (which moves on every later write of the run); a claim taken
  before that column existed reports its age as unknown rather than a
  guess. A reap never touches `ladder_failures`: the task did nothing
  wrong, its supervisor did. Proven end-to-end against a real `foreman
  dispatch` process: claimed, SIGKILLed mid-run, reaped by the next sweep,
  with the pid and claim age in the filed finding.

  The claiming process's pid is carried in a new `tasks.claim_pid` column,
  written ONLY by the trusted production claim path
  (`runner::run_task_with_clock_and_policy`, from its own
  `std::process::id()`) — never parsed back out of the `claimed_by` text,
  which for an MCP-originated claim is agent-controlled free text an agent
  could otherwise shape as `claude@<any pid>` to force or suppress a reap.
  A claim with no `claim_pid` (every claim taken through the generic
  `Ledger::claim_task`, including all MCP claims) cannot be proven dead by
  this reaper and is left alone regardless of lease age or what its
  claimant text looks like. **Known residual, deferred:** an MCP-originated
  claim whose agent session dies can therefore still strand `claimed`, and
  recovery for it stays operator-only (`foreman task requeue --force`). The
  obvious fix — stamping the serving `foreman mcp` process's own pid, which
  IS trustworthy provenance — is deliberately not taken: the MCP server is
  a stdio child that Claude Code restarts on `/mcp` reconnect, so a live
  agent's claim would name a dead pid and the reaper would steal it out
  from under a working agent. That is the one false-dead direction this
  reaper must never take. Closing the gap needs a liveness signal that
  tracks the *agent session* rather than the server process; until there is
  one, NULL is the honest value.

  The sweep retries SQLite contention internally, per candidate, rather
  than being wrapped in one `ledger_write_with_busy_retry` at the dispatch
  call site. A retry out there re-ran the whole sweep, and claims reaped in
  the abandoned pass are no longer candidates (they are `queued`), so the
  operator's only report of what the sweep did silently omitted them. The
  durable record was never at risk — each reap commits its requeue and its
  finding together — but a report that understates the sweep is the same
  class of dishonesty as the phantom claim itself. A candidate whose write
  ultimately fails costs only that candidate — it is left claimed, and the
  next sweep (minutes away) finds it just as expired and just as dead — but
  the failure is **returned, not swallowed**: `reap_dead_claims_with` now
  returns a `ReapSweep { reaped, unreaped }` report, and `foreman dispatch`
  treats any `unreaped` entry as a harness fault (named on stderr with the
  write's own error, sweep continues, exit non-zero), the same rule every
  other ledger fault in the sweep already follows. The earlier cut printed
  the failure and returned `Ok`, so a persistent write fault (exhausted
  contention, a constraint, a storage fault) left the dead claim `running`
  behind a green dispatch — the very silent strand this reaper exists to
  end. Proven against a real `foreman dispatch` process with a SQLite
  trigger refusing the requeue write: non-zero exit, claim and findings
  untouched, and the next sweep after the fault clears reaps it normally.

- Grow infrastructure-refusal retry backoff with the consecutive count, park
  stuck tasks after a configurable limit (default 10), and preserve the exact
  refusal in a blocker without charging the quality ladder. A refused task no
  longer consumes its sweep's task slot ahead of ready work.
- Tell merge-authority reviewers that the refinery resets agent-authored
  package-version edits and records `VersionBumpDiscarded`, so historical
  branch bumps do not reject otherwise valid landings. Other manifest edits,
  package/workspace identity faults, and inconsistent lockfiles remain review
  concerns.
- Add operator-owned `task add --bump patch|minor` and `task set --bump`
  controls. The refinery honours explicit intent before its historical
  risk/kind derivation, while `task show` identifies the effective bump and
  whether it was explicit or derived.
- Allow whole-file package-manifest edits for crates already in task scope,
  including dependency additions, removals, and re-pins. Unscoped manifests
  and Foreman's own gate files remain refused. Projects may opt into the old
  version-only fence with `restrict_manifest_edits: true`; it defaults off.
- Replace free-prose, last-line merge-review verdicts with a structurally
  validated JSON contract carrying the verdict, typed source findings, and
  inspected-file evidence. Reviews now receive a complete changed-file index
  and must inspect every path; omissions synthesize a blocking MAJOR finding.
  Every diff receives either the existing foreman harness checklist or a
  generic correctness/tests/versioning/docs rubric. Typed findings and tier-3
  evidence commit atomically to the ledger. Reviewer routing is configurable,
  with Codex primary, Claude secondary, and two arms for high-risk tasks by
  default.
- Rebase and verify normal landings in the completed task's registered
  worktree, preserving the task-44 common-directory, branch and target pinning
  checks while reusing the implementer/runner-warmed private target. Branches
  without a dedicated task worktree retain a contained detached fallback.
- Run executable provenance once on the principal tier-1 landing test step.
  Tier 0 records only `target_dir`; landing reports name `provenance_tier: 1`
  and contain exactly one `executed_binaries` result. The byte/path guarantee
  remains unchanged and diagnostic-only.
- Scope tier-0 fmt, clippy and tests to `tasks.crates` plus their transitive
  workspace reverse dependencies from `cargo metadata --all-features`. Scope
  the tier-1 feature matrix to the same closure while keeping exactly one full
  `cargo test --workspace`; `crates=[]` preserves the previous behaviour. A
  checked-in fixture proves a broken renamed-path reverse dependency is caught.
- Tell implementers to run package-scoped fmt, clippy and tests only for crates
  they changed. Foreman, not the implementation session, owns the authoritative
  tier-0 gate.
- Same-workstation medium-task measurement (2026-08-25, this `cosmix-foreman`
  change): the pre-change cold full tier-0/provenance run recorded below was
  611.32s; the warm `crates=[cosmix-foreman]` tier-0 shape was 85.05s total
  (`cargo fmt --check --package cosmix-foreman` 0.30s, package clippy 2.35s,
  package tests 82.39s), a 7.2x wall reduction before landing. The managed
  sandbox could not access memguard's systemd user scope or sccache, so the
  after run used an empty `RUSTC_WRAPPER`, `-j8`, and the task worktree's own
  existing `src/target`; it did not redirect Cargo into `/tmp`.
- A separate warm, temporary-ledger tier-1 observation kept one full
  `cargo test --workspace` and one provenance-bearing step as intended. It
  reached 669.71s before the first network test failed because the managed
  sandbox forbids even a localhost UDP bind, so this is recorded as a red
  environment-limited observation, not a green landing measurement.
- Fix a false-positive landing bounce introduced by the worktree reuse above:
  the post-verification dirt check flagged any git-ignored path outside the
  exact pinned target directories as tree corruption, so cosmix-foreman's own
  `testdata/feature-fixture` and `testdata/scope-fixture` integration tests —
  which run Cargo as a subprocess inside those checked-in fixture
  directories, each producing its own nested, `.gitignore`-matched `target/`
  — bounced every cos tier-1 landing that ran this crate's own test suite. The
  check now recognizes any `target`-named path component and ignored
  `Cargo.lock` anywhere in the tree as Cargo build output, while still
  treating a late write to any OTHER ignored path as real dirt — a checked-in
  regression test proves both halves. A full
  local tier-1 landing (real `cargo deny`, no sandbox network restriction) now
  goes green end to end with provenance recorded exactly once.
- Resume instead of restart. `Executor` gains `resume(session_ref, turn, ws,
  budget)`, implemented by the Claude driver (`claude ... --resume <id>`) and
  the Codex driver (`codex exec resume <id> ...`). A same-rung retry (same
  agent + model as the task's immediately preceding attempt, with a recorded
  session id) resumes that session with the lowered findings prompt as the
  next turn instead of opening a fresh conversation; a rung change or a
  missing session id still starts cold.
- Merge-authority review threads persist per (task, reviewer kind, model): a
  re-review after a reject-and-fix resumes the SAME reviewer session with a
  turn naming the new tip and re-sending the CURRENT complete changed-file
  index, then asking it to re-judge each earlier finding (fully fixed /
  partially fixed / unaddressed) and flag anything new. The full index is
  repeated deliberately — an arm that approved a prior tip must still be shown
  every file in the diff it is now re-affirming, not just told a hash moved,
  or its resumed approval would cover commits it never read. Registered
  `task-<id>` worktrees retain their stable path, while a legacy/manual task
  recreates its detached review checkout at the same deterministic
  `.foreman-review-<repo>-task-<id>` path on each landing. That stable cwd
  permits reviewer resume for both task classes; the legacy checkout is
  removed when each landing finishes, and `gc-scratch` reclaims a crash stray
  once its task is terminal. The turn repeats the machine-parsed JSON output
  contract. When the current diff
  touches `cosmix-foreman`, it also repeats the harness checklist so a thread
  opened on an earlier non-Foreman diff cannot approve without ever receiving
  those invariants; other current diffs rely on the opening rubric already
  held by the thread.
  A recorded session the vendor has since pruned spawns successfully and then
  errors, so a failed resume cannot be caught at spawn time: a resumed arm
  that dies at the vendor without rendering a verdict falls back to a full
  fresh review in the same run rather than failing closed into a rejection
  that repeats every sweep (`tests/resume_cycle.rs`).
  The acceptance clause's measurement is a checked-in test rather than
  prose: `rereview_round_ingest_drops_by_at_least_80_percent` in
  `tests/resume_cycle.rs` drives a real reject → fix → re-review over a
  5-file diff whose fix touches one file. The fake reviewer derives its
  input total from the real `-p` payload plus the Git objects it actually
  reads, emits that total as streamed usage, and the test compares the two
  checkpointed ledger rows. The re-review ingest drops **92.6%**
  (852,430 → 63,298 derived byte-tokens), satisfying the ≥80% acceptance
  threshold without post-hoc argv/blob arithmetic. This is a deterministic
  fixture metric, not a model of vendor billing; a real resumed conversation
  may also report cached input from earlier turns.
- Codex resume argv: every `exec`-level flag (`--sandbox`, `--add-dir`,
  `--json`, `-m`) now precedes the `resume <id>` subcommand. `codex exec
  resume` is a clap subcommand with its own small option set and rejects
  `--sandbox` outright, so the previous ordering made every resumed codex
  session unrunnable. `tests/phase2.rs` now drives the installed CLI to prove
  the argv parses (skipped when codex is absent), since a string-shape
  assertion cannot see a clap rejection.
- The explicit resume path and the preconfigured
  (`--resume`-on-resource-exhausted) one no longer stack into two flags; the
  explicit id overrides. `Ledger::latest_resumable_session` also gained a
  model filter, so that older path agrees with the runner's same-rung guard
  on what a rung is — filtering on agent alone let a ladder climb resume the
  previous model's conversation.
- A pruned implementer session falls back to a fresh one instead of burning
  the attempt. Both CLIs spawn happily against a session the vendor has since
  dropped and only then exit with an error, so this cannot be caught at
  `Command::spawn` time — the reviewer path already had the same fallback.
  Without it the attempt bounces having opened no conversation, and the next
  attempt resumes the same dead id, so the task can never clear. The
  fallback is deliberately narrow: a resumed run that reported a session id,
  streamed any usage, or returned a result HAS run and its outcome stands.
  The discarded attempt and its replacement share one run row and one
  continuous event sequence, so the journal still reads in order.
  Dead ids are retired by clearing `runs.session_ref`, not by writing a
  `paused_for_resume`-vocabulary `runs.quality`, because that quality and its
  schema-v16 vocabulary were descoped with item 3 and schema v15 rejects it.

## 0.17.0

**Breaking — MINOR, not PATCH.** Four observable surfaces change, so a
patch bump would misreport this release to every consumer:

- MCP `task_bounce` requires `attempt` (the claim generation). It was
  `Option<i64>`; it is now a required `i64` with no serde default, so an
  existing caller that omits it fails to deserialise. Wire-schema break.
- `ladder_patience` in `foreman.conf.mix` changes from a scalar to a
  per-rung map type; the accepted config value's type changed.
- Ledger `SCHEMA_VERSION` 13 -> 14, and `migrate()` now refuses a ledger
  newer than the running binary outright, so an older foreman cannot open a
  migrated ledger.
- Charging semantics and the landing commit shape, which the task's own
  acceptance line calls observable.


- Bound the refinery's offline Cargo metadata and lockfile-update children to
  120 seconds and 4 MiB per output stream, killing and reaping their process
  groups on timeout as infrastructure refusals. MCP self-bounces now require
  the exact claim attempt, policy-denied merge-review retries use 30-second
  backoff and park at the operational-refusal threshold, and disposition
  events and runner/refinery reports name `parked` when a retry cap parks the
  task. Orphan-manifest healing now also permits removal of valid TOML whose
  package/workspace version is unusable, and SemVer bump overflow is a typed
  branch bounce.
- Preserve explicit landing-infrastructure classification through task-fault
  wrappers, route verifier-directory host I/O and ambiguous Cargo child
  failures to infrastructure backoff, and classify merge-review lane or
  credential refusals as `policy_denied`. Recognisable local Cargo
  manifest/lockfile and dependency-resolution diagnostics remain branch
  faults.
- Fence newly added workspace lockfiles as regular contained files, enforce
  `[package].workspace` authority for concrete and inherited versions, accept
  SemVer build metadata while dropping it from the new bumped release, and
  restrict poisoned-manifest healing to malformed manifests without a Cargo
  ancestor.
- Make the branch-contract retry park reachable for refinery bounces: ordinary
  agent completion preserves the categorical recurrence count, while a
  successful landing or explicit operator requeue resets it.
- Invert the refinery's landing-error default: only explicitly tagged host
  filesystem, Git/process and ledger failures use infrastructure refusal and
  backoff; every unannotated landing error becomes a durable task bounce so a
  new branch-influenced failure cannot wedge the merge queue. Poisoned orphan
  base manifests now name their path, later tasks continue, and a task may
  remove the poisoned manifest to heal the integration tree.
- Preserve both Cargo workspace membership controls (`members` and `exclude`)
  across landing, keep workspace-write I/O failures out of the branch-contract
  counter, require positive current-attempt verification evidence before a
  migrated NULL-attempt run can receive a charge, and carry cause details
  through in-passing park summaries.
- Complete round-3 landing hardening: agent-controlled discovery-path
  refusals are typed bounces; virtual workspace roots and their base lockfiles
  are validated even when no package is selected; bounce disposition and its
  finding commit atomically before wake; and migration-era NULL-attempt runs
  cannot absorb charges from a later MCP-only attempt. Operator messages now
  distinguish combined quality charges from all-remaining-rungs refusals, and
  the runbook reflects that project-policy denials leave the ladder unchanged.

- Cold-review hardening: ledger schema 14 now installs attempt charging and
  reason-specific routing columns in one transaction; replay-supplied wall
  time decides infrastructure-backoff admission; post-run/refinery refusals
  accumulate across claims and commit their counter/delay with disposition.
  Crash recovery charges a recorded red landing and files its retry handoff
  through the same classified transaction. Missing or malformed review
  verdicts are harness delivery failures, never quality rejections.
- Make dispatch planning clock-pure at the real planner boundary, parse
  RFC3339 backoff timestamps rather than ordering their spellings, and charge
  migration-era latest implementation runs whose schema-13 attempt is NULL.
- Resolve landing versions from the integration-base TOML, including
  workspace-inherited versions. Agent version edits are reset with an info
  finding before the refinery applies its own bump; package-name, workspace
  root/membership, deleted base lockfile and redirected-workspace changes are
  typed task bounces which do not stop the queue. Structured TOML edits accept valid
  formatting, while `O_NOFOLLOW` regular-file checks refuse symlinked package
  manifests and lockfiles before any rewrite or Cargo invocation.
- Make ladder charging an explicit delivery/quality decision attributed to an
  implementation run. Runnable verifier failures and genuine review rejects
  charge at most once per attempt; transition names, branch-contract defects,
  rebase conflicts, policy denials, resource ceilings, and vendor/harness
  failures do not. `task show` and disposition events expose each attempt's
  zero-or-one charge.
- Advance both verifier-red and review-rejected charges according to patience;
  cap consecutive branch-contract/MCP self-bounce loops; skip a rung which
  cannot meter the task budget; resume resource-exhausted sessions; and
  put vendor/harness failures on the infrastructure counter with a 30-second
  retry delay. `ladder_patience` now accepts per-agent or per-rung overrides.
- Launch after a provisioning rebase conflict on the cleanly-aborted branch,
  with the conflict finding and a trusted rebase-first instruction in the
  prompt. Repeated pre-claim conflicts no longer park tasks without an agent
  run.
- Remove package-version work from implementation prompts. The refinery now
  bumps packages owning changed files, refreshes their lockfiles offline, and
  commits the version change in the detached landing tree before verification.
  Concurrent same-crate task branches therefore avoid version-line conflicts.

## 0.16.4

- Stabilise the concurrent `unit_health` test fixture by retrying its freshly
  written fake-systemd probe while an `ETXTBSY` fork/exec window reports
  `systemd_unavailable`. This was a test-only change.

## 0.16.3

- Fix Codex `tokens_in` double-count bug (Task 72): codex-cli 0.145.0+ changed
  `input_tokens` semantics — it now includes cached tokens as a subset, not
  as an addend. The old fold `input_tokens + cached_input_tokens` double-counted
  the cached portion. Fixed to `tokens_in = input_tokens` only, with
  `fresh_input_tokens = input_tokens - cached_input_tokens`. Historical codex
  runs with a captured cache-read component, unknown fresh component, and a
  mathematically consistent old fold are migrated on ledger open: `tokens_in`
  and `fresh_input_tokens` are recomputed exactly. Codex rows lacking that
  evidence, inconsistent rows, and every non-Codex row remain byte-for-byte
  unchanged. The fixture pins input=100, cached=40 → `tokens_in`=100, fresh=60,
  cache-read=40.


## 0.16.2

- Add optional per-task USD budgets through `task add --budget`. Each attempt
  holds and caps at the unspent remainder, or at a narrower explicit
  `run --max-budget-usd`; a hold never exceeds the remainder. Known costs
  charge actuals and unpriced/dead-early attempts charge their recorded hold.
  `task show` and fleet status expose total, charged and remaining amounts.
- Treat exhaustion as a normal task outcome: park before claim with a blocker
  finding, keep dispatch green, and leave the task slot available for other
  work. Operators can replace/top up the total with `task set --budget <USD>`,
  requeue the parked task, or remove its ceiling with `--budget clear`.
- Refuse budgeted tasks on dollar-blind lanes and ladders, and refuse authored
  budgets above an enabled daily ceiling. A zero daily dollar ceiling remains
  the established disabled/unlimited setting. This remains 0.16.2 because
  0.16.1 belongs to task 25 and lands separately.

## 0.16.1

- Extend the opt-in bubblewrap filesystem view to the Claude and GLM lanes.
  Claude receives only its own stored authentication; GLM receives its Z.ai
  token through the child environment; neither lane can see Codex or Zcode
  credentials or the foreman environment file. The native Claude install is
  mounted read-only for both lanes, while Claude's own session/auth state is
  writable so the real CLI can start and refresh it.
- Compose policy-hook execution into that view by mounting the exact foreman
  executable and per-run settings read-only and the ledger plus live SQLite
  sidecars read-write. Project-mode hooks also mount the manifest and the
  repository/Git metadata its startup identity check reads, all read-only.
  Missing, inaccessible or wrong-type mandatory hook sources refuse the launch
  rather than being dropped as optional grants. Any policy-check startup error
  exits 2 with a denial diagnostic. The writable ledger remains reachable to
  shell code, so mount containment complements rather than replaces the
  gate-path rule.
- Prove the composed gate by effect: a fixture Claude process executes the
  mounted project-mode hook from the native-install symlink layout inside
  bubblewrap; both Claude and GLM must receive the documented exit-code-2
  denial for an attempted policy-settings write, with the deduplicated denial
  reaching the host ledger. A deliberately unmounted manifest independently
  proves the pre-verdict startup path fails closed with exit 2.

## 0.16.0

- Make the filesystem verify lane injectable with
  `FOREMAN_VERIFY_LANE=<absolute-path>` and bound contention with
  `FOREMAN_VERIFY_LANE_WAIT_SECS` (default 900 seconds). The lane now stamps
  its owner and reports that holder on timeout. A nested Foreman which would
  re-acquire the host lane held by its ancestor refuses immediately and names
  `FOREMAN_VERIFY_LANE` instead of self-deadlocking. Project manifests use
  their own `<manifest-root>/verify.lock` by default, alongside their scoped
  worktrees and `clone.lock`.
- Add the strict-data Mix project-manifest format and global
  `--project <path>` selector. A manifest requires a repository, dedicated
  ledger, cache root and non-empty project instruction pack, and may define
  integration/branch/worktree naming, package-manifest shape,
  implementation and merge-review lane eligibility/credentials, landing
  tier/review/argv policy. Each manifest gets a filename-and-name-derived root;
  relative ledger/cache paths, worktrees, `clone.lock` and `verify.lock`
  resolve beneath it.
  Escaping state and control/state paths inside the managed repository are
  refused, including final-component symlinks. The manifest name and stable
  Git root-commit identity are stamped into its ledger and checked on every
  project-mode open, so moved checkouts still match while same-name unrelated
  repositories are refused. Populated
  unbound ledgers require explicit migration. Repository/workdir and
  integration flags may repeat but cannot override the manifest. Manifest
  task worktrees and `clone.lock` live below the manifest root rather than a
  possibly shared repo parent.
- Add manifest-defined verifier profiles with an owned cwd and ordered argv
  steps for tiers 0, 1 and 2. Transparent commands retain Cargo preflight,
  per-worktree target pinning and provenance. A step's exit code alone decides
  pass/fail; `opaque: true` only records provenance as `unavailable` and can
  never change that verdict (task 44). Opaque exceptions exist only in the
  compile-time built-in table or operator-owned manifest, never task input.
- Resolve project settings consistently for task authoring, explicit runs,
  dispatch, MCP completion, verify, refine, physical acceptance, task landing,
  cache GC and mayor-spawned MCP sessions. Explicit legacy invocations without
  `--project` keep their existing defaults. MCP task routing and claiming
  enforce the same manifest lane and credential policy as dispatch. An
  omitted project `landing_gate` means no gate and never inherits ambient
  fleet policy. Legacy no-manifest package policy continues to match any
  `*/crates/<name>/Cargo.toml`. MCP completion can assert but never replace
  the task's ledger-recorded branch.
- Inject the project instruction pack into implementation, merge-review and
  mayor prompts. Project prompts use neutral worktree prose; integration,
  branch naming, build and versioning rules come from the operator pack rather
  than compiled project assumptions. Oversized packs are refused at manifest
  load rather than silently truncated. Reserving 8 KiB for the trusted pack
  reduces the legacy implementation-spec section cap from 76 KiB to 64 KiB
  while retaining the existing whole-prompt bound.
- Keep landing and task-branch cleanup local. This is round 6's explicit
  option (a); the `push_remote` manifest key is refused and every accumulated
  remote-push constraint is deferred to fleet task 67.
- Keep manifest refinement isolated from ambient sibling-repository refreshes,
  and advance MCP tasks past project-refused ladder rungs so a later eligible
  lane remains reachable.

## 0.15.0

- Add `foreman task retire <id> --reason <text>` as an operator-only terminal
  transition. Retirement refuses live claims and landing tasks, files the
  reason, stays fenced from claimant completion, and is hidden from the
  default task list while remaining visible with `task list --all`.
- Make task discovery fail soft on unrecognised stored statuses. The default
  list skips such rows, `--all` exposes their raw status, and dispatch plus MCP
  task selection skip them while filing one typed warning per open finding.
  Resolving that finding allows a fresh warning if the row remains invalid.

## 0.14.0

- Replace creation of the cwd-relative `./.foreman/ledger.db` default with
  durable state resolution: `--db`, `FOREMAN_DB`,
  `STATE_DIRECTORY/ledger.db`, an existing legacy cwd ledger, then the
  XDG/FHS Cosmix variable-data directory at `foreman/ledger.db`. Selecting a
  legacy ledger prints one path-naming deprecation note. Derived state
  directories must be absolute, and implicit selections never create missing
  parent directories. A selected legacy ledger is also opened without SQLite
  create authority, so it cannot be recreated if it vanishes after resolution;
  policy-hook and mayor/MCP child processes inherit that resolved authority.
- Resolve `foreman.conf.mix` from an existing file beside the ledger, then
  `CONFIGURATION_DIRECTORY/foreman.conf.mix`; `FOREMAN_CONF` no longer changes
  the selected file. Missing config is reported on stderr. `governor status`
  now prints the daily budget from the invocation's `FleetPolicy` snapshot
  with `env`, `conf`, or `default` provenance.
- Restore `FOREMAN_*` environment overrides in the public verifier wrappers
  without restoring their former cwd-relative config lookup.
- Keep the governor `STOP` file beside the resolved ledger without adding a
  second path setting. Existing units which pass an explicit `--db` retain
  that path unchanged.
- Keep Cargo targets private to each worktree. They are regenerable cache,
  but consolidating them under one `CACHE_DIRECTORY` would restore the
  cross-worktree stale-binary defect fixed in 0.13.0.

## 0.13.2

- Rust tier 1 now discovers each workspace member's non-default Cargo
  features and runs one crate-scoped test step per feature. It deliberately
  avoids workspace-wide `--all-features`; private `_...` harnesses, live
  `cosmix` citizen features, and operator exclusions remain visible in the
  report instead of being silently omitted.
- Add strict, snapshotted `FOREMAN_FEATURE_SETS` and
  `FOREMAN_FEATURE_EXCLUDE` overrides. Empty, malformed, unknown, default-only
  or undiscoverable coverage produces a red report step.
- Add a checked-in fixture crate proving a default-green, feature-gated red
  test is caught end to end. The new gate also exposed and fixed the stale
  `cosmix-maild-rules:dnsbl` Hickory transport import.

## 0.13.1

- Add `reserve_usd` and `reserve_tokens` to `foreman.conf.mix` and report
  their resolved values and provenance through `foreman config show`:

  ```mix
  reserve_usd: 5
  reserve_tokens: 500000
  ```

- Keep governed dispatch, explicit runs and refinery review reservations on
  the single `FleetPolicy` snapshot loaded for their sweep. Reserve estimates,
  enforced caps, vendor binaries and sandbox sibling-repository binds no longer
  re-read policy-owned environment values between claims.

## 0.13.0

- Fix: a shared `CARGO_TARGET_DIR` could let a verifier run in one worktree
  execute a binary compiled from ANOTHER worktree's source (proven live —
  fleet task 44, three separate incidents). Cargo gives same-named local path
  crates in sibling worktrees the same output slot, then uses mtimes to decide
  freshness without knowing which worktree produced the cached binary.
- Every verifier child and every Claude, Codex, or ACP agent child now has
  the ambient `CARGO_TARGET_DIR` replaced with a canonical private target as
  the last environment mutation before spawn. Cargo's environment setting
  outranks `.cargo/config.toml`, so an earlier build script cannot redirect a
  later step by rewriting config after the target was checked.
- `target_dir::pinned_target_dir` derives one canonical target from the
  verifier's Cargo workspace root. Agent drivers use the task worktree plus
  verifier subdirectory; the verifier uses its resolved workspace directly,
  and both forms produce the same path. Immediately before every Cargo step,
  `cargo metadata` must report that exact pin. The probe resolves Cargo from
  the verifier process's PATH outside the worktree, parses only recognised
  transparent wrapper environment assignments, and never executes or trusts
  stdout from env, memguard, timeout, flock, a shell, or another wrapper.
  `+toolchain`, `--manifest-path`, `--config`, and `--target-dir` semantics are
  preserved. The pin is applied after parsed `env KEY=VALUE` assignments, so
  a wrapper's stale `CARGO_TARGET_DIR` cannot override it. An opaque
  `sh -c '…'` command is refused clearly unless the exact built-in profile
  step is declared opaque (there are no such declarations today). The
  derived workspace and pin refuse escaping symlinks, and an argv-level
  target which outranks the pin is refused before the real step runs. An
  ordinary metadata failure remains an ordinary failed cargo step rather than
  an infrastructure abort.
- Limitation: the specification's shared read-only dependency cache is NOT
  achieved yet. The host's global `sccache` produced zero new Rust hits in
  the round-3 cross-worktree measurement because path-dependent metadata
  changes the Rust cache key. Third-party Rust crates are therefore not warm
  across worktrees today; a private target pays the real cold compile cost.
  Round-5 cold measurement on 2026-08-24 used a release Foreman 0.13.0
  rebuilt from this implementation against a fresh detached task-44
  worktree with an absent private target and sccache enabled. Exec timestamps
  around the actual `foreman verify --profile rust --tier 0` run measured:
  `cargo fmt --check` 2.38s, `cargo clippy --all-targets -- -D warnings`
  116.39s, and `cargo test` including immediate preflight plus both provenance
  snapshots 492.38s; 611.32s total. The test step comprised about 229.97s for
  the pre-run no-run build, 245.24s for the real tests, and 16.87s for the
  post-run listing and hashes. It retained 107.62s of its 600s deadline; the
  tier retained 1788.68s of the 2400s fleet cap. `sccache --show-stats`
  before→after was: compile requests 24099→26414, executed 7628→9163, hits
  2410→2831, misses 5186→6296, and non-cacheable calls 16429→17203. Rust hits
  stayed 280→280 while Rust misses rose 4984→6045: all 421 new hits were
  assembler/C/C++, confirming no cross-worktree Rust warming. The run was
  traced for step boundaries, so the small tracing overhead is included.
  The temporary 12GB target, detached worktree and sibling links were removed
  afterwards. An empty sccache would pay each dependency's real compile once;
  that worst case is not measured.
- `VerifyReport` gains `target_dir`: the RESOLVED, verified-private directory
  the preflight established, as cargo reported it rather than from the
  ambient variable. It is `#[serde(default)]`, so older persisted reports
  still deserialize. Version 0.13.0 is required because both the build
  location and this report surface are observable changes.
- Each `VerifyStep` gains explicit `executed_binaries` evidence:
  `complete { binaries }`, `not_applicable`, or `unavailable { reason }`,
  with legacy reports deserialising to unavailable rather than a dishonest
  empty list. Before a `cargo test` or `cargo bench` step, Foreman makes a
  verifier-chosen invocation of the same transparent wrapper and
  Cargo selectors with `--no-run --message-format=json`, the ambient target
  replaced by the immediate-preflight private target.
  This is Cargo control data from an invocation the verifier owns, not paths
  parsed from the code-under-test's captured stdout. Foreman hashes exactly
  non-null `executable` paths in Cargo's `compiler-artifact` records. Every
  path remains untrusted: symlinks and non-regular/non-executable files are
  rejected before read-open; paths are canonically contained in the private
  target, deduplicated by `(dev, ino)`, pinned with `O_PATH`, and streamed
  through `sha2`. The bounds remain 1GiB per file, 64GiB aggregate and 16384
  control/artifact entries, plus a 64MiB control-output cap, all within the
  original step deadline. Listing failure, timeout or an escaping path makes
  provenance unavailable. Foreman runs the real step, then repeats the
  listing and hashes. Any path-set or digest change records unavailable with
  both sets in the reason; an unchanged set completes with the pre-run
  digests. The exact guarantee is: "these bytes existed at these paths when
  the step began and were unchanged when it ended; cargo ran them". A test
  may exec a different file it carries itself; that is outside provenance's
  claim. Non-test/bench steps are not applicable. Collection is diagnostic
  only and cannot alter pass/fail, authority, cache, replay or routing; warm
  reused binaries remain in Cargo's artifact list. `foreman verify` prints
  each relative path and hash, or the explicit not-applicable/unavailable
  state.
- `lowering::verifier_section` now tells the agent not to set its own
  `CARGO_TARGET_DIR`, naming why: pointing one at `/tmp` has previously
  filled a shared `/tmp` to capacity and broken every tool on the host
  (2026-08-22, four agents independently did exactly that to escape the
  shared directory's interference — this change removes the reason to).
- New regression tests `tests/target_dir_isolation.rs`: pin the underlying
  cargo behaviour the bug depends on (so the suite fails loudly if cargo's
  freshness semantics change underneath the premise), prove
  `verify::run_commands` isolates a worktree's build even against a writer
  ACTIVELY compiling into the shared directory for the full duration of the
  run, reject the real command's explicit outside target before it runs,
  reject target symlinks which escape the workspace, distinguish `targetX`
  from `target`, prove transparent wrappers build privately, and prove opaque
  shell cargo is refused before execution. Provenance tests cover symlink,
  FIFO/device, per-file and aggregate caps, inode deduplication, mid-hash
  deadline exhaustion without verdict changes, explicit evidence-state JSON,
  self-replacement, input-triggered post-listing rebuilds, unchanged
  pre/post sets, and human-readable relative SHA-256 output. A foreman-level
  sibling-worktree regression drives the actual Rust tier 0 in tree A after
  building tree B into the old shared slot, proves the pinned run passes, and
  proves the same tier command vector fails when run unfenced against B's
  bytes. A separate two-step fixture rewrites `.cargo/config.toml` before
  `cargo test` and proves the pin keeps all artifacts private.
- The operator contract, unit cleanup, warm-probe exception, per-worktree
  disk cost, and revised `gc-cache --dir` procedure are now in
  `docs/_man/foreman.md` rather than only in this changelog.
- Agent dry runs and verification now share the one measured roughly 13 GB
  `<worktree>/src/target` tree. This removes the rejected round-5 split that
  wasted a second cold build and left a second roughly 13 GB target outside
  the documented GC enumeration.

## 0.12.4

- Tell task agents that linked worktrees use a `.git` file and how to find shared
  Git metadata safely.

## 0.12.3

- Deny agent pushes to `origin` or any destination other than the shared fleet
  repo, and deny pull/fetch/reset/rebase flows through `origin/task/*`.
- Prune a landed task branch from the shared repo and canonical `origin`, when
  present, without letting post-CAS cleanup failures misreport the landing.
- Tell implementation agents never to push or pull task refs through `origin`.


## 0.12.2

- `foreman task set --verifier <profile>`: operator-owned correction of a task's
  verifier profile, validated against the built-in profile table, refused while
  the task is running or landing, canonicalised before storage, and changed
  atomically with its info finding on the task.
- `foreman task add --verifier` help now lists every built-in profile
  directly from the same table used for lookup (`compositor` was accepted but
  undocumented — the whole compositor chain was authored under `rust` and
  verified in `src/` as a result; fleet finding 454).
- Dispatch decisions, successful tier-0 gates and refinery landing outcomes now
  name the verifier profile which ran.

## 0.12.0

- Mark every verifier report and CLI verdict as HEADLESS, and carry the
  compositor's uncovered physical ground in the report itself: `kms-live`
  and the explicit-sync live tests are compiled but not executed.
- Add a separate `physical-acceptance` command for compositor KMS acceptance.
  It requires explicit device, connector, deadline and
  `--take-vt-and-display` arguments, forces the compositor's typed
  `--kms-confirm` interlock, and is not a verifier tier.
- Mark every unattended verifier child, including operator-defined nightly
  tier-2 commands, as headless. Physical acceptance refuses that inherited
  marker, preventing a nightly command from accidentally taking a VT or
  display.

## 0.11.4

- Keep every refinery verifier, landing gate, merge-authority review and Git
  subprocess outside a ledger transaction. The refinery now fences the
  verifier boundary with an explicit autocommit check, and stale-reservation
  sweeping performs `/proc` liveness probes before opening its short guarded
  delete transaction.
- Give live run-event appends a dedicated bounded 60-second SQLite contention
  budget. Ordinary writes retain their smaller attempt budget; a wedged event
  append still fails, but a transition or burst that clears within a minute no
  longer kills the active agent run.
- Log the operation and elapsed milliseconds whenever any SQLite busy budget
  is exhausted, while stating that SQLite cannot identify the lock holder.
- Add contention regressions proving a fake verifier can write through an
  independent connection with a one-second timeout and a live run survives a
  foreign `BEGIN IMMEDIATE` held for ten seconds.

## 0.11.3

- Tell the Claude and GLM lanes plainly that `claude -p` is single-turn and
  headless: background Bash cannot deliver a later completion turn, gates must
  run in the foreground with an explicit timeout, and work must be committed
  before the final response.
- Track Claude Code background-Bash task bookends. A session that returns with
  a live task, or reports that teardown killed it, is now recorded as delivery
  `harness_error` with quality `agent_abandoned_background`, returned to the
  queue once without charging the escalation ladder, and given one deduplicated
  finding explaining the mechanism to the next attempt. A consecutive repeat
  parks the task and promotes that same finding to blocker, still without
  charging the model ladder. Completed, committed runs remain ordinary
  delivered runs even if Claude Code tears down a harmless background helper;
  the dirty branch contract is the abandonment arbiter. Detection covers
  Claude Code's automatic transition
  from foreground Bash to background after its tool timeout (`task_started`
  false, then `task_updated.patch.is_backgrounded` true), the independent
  `background_tasks_changed` snapshot, and the teardown kill event seen in the
  two task-44 incidents. The shared driver applies the same handling to GLM.

## 0.11.1

- Retain the fresh/cache-read/cache-creation input-token breakdown that
  `accumulate_usage` previously folded and discarded. `usage` events and
  `runs` rows now carry `fresh_input_tokens`, `cache_read_input_tokens`, and
  `cache_creation_input_tokens` alongside the existing folded `tokens_in`.
  Cache reads price at roughly a tenth of fresh input, so a run reporting 6M
  input tokens could be a cheap re-read of a cached context or 6M genuinely
  new tokens — and until now the ledger could not tell the two apart, which
  confounded every cost-per-task and efficiency comparison in it.
- `tokens_in` is unchanged in meaning, value and every lane's arithmetic. Cap
  enforcement and the governor read it, and a silent redefinition would move
  every historical number, so the breakdown is strictly additional: it records
  the components the fold consumed rather than recomputing the fold.
- Unknown is recorded as `NULL`, never as zero — "this lane does not tell us"
  is a different claim from "no cache reads happened". Per lane:
  - **Claude** (and **GLM**, which runs through the same driver and parser):
    reports all three. `tokens_in == fresh + cache_read + cache_creation`
    exactly, asserted in `tests/parsers.rs`. A component omitted from any
    contributing usage block makes that component's total unknown, since a
    total is only knowable when every block reported it; an explicit `0` on
    the wire stays a known `Some(0)`.
  - **Codex**: reports `cached_input_tokens` (cache read) and
    `cache_write_input_tokens` (cache creation); both are recorded as the
    direct vendor readings they are. `fresh_input_tokens` is recorded as
    **unknown**. Deriving it means first settling what Codex's `input_tokens`
    means, and rollouts from codex-cli 0.145.0 say it is the *complete* input
    count (`input_tokens + output_tokens == total_tokens`, with
    `cached_input_tokens` a subset of `input_tokens`) — under which reading
    this lane's `tokens_in = input_tokens + cached_input_tokens` fold
    double-counts the cached subset and no partition of it exists to record.
    Correcting that fold moves every historical Codex figure, so it is left
    alone here and flagged for a separately authorised migration; until then
    unknown is the only honest answer, and the cache-read count — the number
    the efficiency question actually turns on — is captured regardless.
  - **ACP**: reports only an undifferentiated input total, so all three
    components stay unknown.
- Historical rows are untouched and still readable: the three columns are
  added by the existing additive migration and arrive `NULL` for every
  pre-existing run. Nothing is backfilled — the data was never captured, and
  inventing it would be worse than the gap.
- Operator surfaces show the breakdown: `foreman status` appends
  `[fresh=… read=… write=…]` to a run line when any component is known, and
  the streamed `[usage …]` echo carries the three components with `?` for a
  component its lane does not report.

## 0.11.0

- Add the persistent per-task `operator_driven` flag, authoring and update
  CLI controls, and task list/show visibility.
- Keep operator-driven work out of unattended dispatch and MCP claims while
  retaining explicit `foreman run --task` as the operator claim path.
- Report otherwise-ready operator-driven tasks separately in dispatch queue
  summaries and preserve the flag across requeues.

## 0.10.6

- Allow a task to bump only the `[package]` version line of a crate its
  committed branch history already changes, plus the matching source-less
  workspace entry in `Cargo.lock`; uncommitted work cannot widen scope and
  other manifest edits remain operator-only.
- Add repeatable `task add --crate` for operator-owned scope in bump-only
  tasks or crates the branch will not otherwise touch; free-form task prose
  does not grant authority.
- Add `--integration-base` to the policy hook so branch-touched crate scope
  is resolved once from the caller's configured integration ref.
- Fail closed on Cargo manifest/lock-writing commands and unrecognised shell
  references to `Cargo.toml` or `Cargo.lock`; only documented read-only
  command shapes and exact quoted `cat` heredocs pass the shell classifier.
  Write-target globs, expansions, and escapes fail closed before literal
  filename matching, while read-only globs remain allowed.
- Add the public foreman operator runbook covering manifest scope and the
  policy hook's integration-base and shell-command contracts.

## 0.10.1

- GLM lane: cap per-response output at 32768 (`CLAUDE_CODE_MAX_OUTPUT_TOKENS`)
  and compact at 120k (`CLAUDE_CODE_AUTO_COMPACT_WINDOW`, was 200k). Every
  GLM session that died with "API Error: The model has reached its context
  window limit" (12 transcripts across tasks 8-43) peaked at 137-139k of
  context with the CLI error `max_output_tokens`, while glm-5.3 accepts a
  270k-token request directly: the wall is the remapped model's
  `input + max_tokens` ceiling, not its window, and a compactor planned
  against 200k never ran. Harness test covers both variables on the GLM
  lane and their absence on the native claude lane.

## 0.10.0

- Add strict-data `foreman.conf.mix` fleet policy beside the ledger, with
  `FOREMAN_CONF` as an explicit path override. Policy is snapshotted per CLI
  sweep and reloaded per long-lived MCP tool call.
- Resolve `ladder`, `ladder_patience`, `daily_budget_usd`, optional
  `daily_output_tokens`, Claude `review_model`, `codex_review_model`,
  `review_override`, `two_arm_review`, `tier_timeout_secs`, and `tier2_commands`
  with `env > conf > compiled default` precedence. Invalid or unknown config is
  a hard error naming the key.
- Add `foreman config show [--json]`, including each effective value and its
  source plus whether the conf file was found.
- Route dispatch, run completion verification, standalone verification,
  governor status/reservations, refinery review reservations/model selection,
  mayor startup, and MCP routing/claims through the shared resolver.
- Route merge authority from the recorded implementation family: Claude work
  is reviewed by Codex, Codex work by Claude, and GLM/unknown work by Claude.
  `FOREMAN_REVIEW_OVERRIDE` may fix the reviewer to `claude` or `codex`, but
  GLM is never accepted as merge authority. High-risk tasks may opt into both
  review arms with `FOREMAN_TWO_ARM_REVIEW=true`; either arm can reject.

Strict-data example (nested tier keys are quoted because Mix data-map keys are
strings):

```mix
ladder: ["glm", "codex", "claude:sonnet", "claude:opus"]
ladder_patience: 2
daily_budget_usd: 300
review_model: "opus"
codex_review_model: "gpt-5.6-sol"
# Optional: review_override: "claude" # or "codex"
two_arm_review: false
tier_timeout_secs: {"0": 2400, "1": 3600, "2": 7200}
tier2_commands: ["cargo test --workspace --release"]
```

## 0.9.0

- Scope interrupted-landing recovery to verification rows from the task's
  current attempt. With no current-attempt tip-bearing verdict, recovery
  returns the task to `done` without adding a ladder failure.
- Retry transient SQLite busy/locked failures on refinery ledger writes so a
  contended landing transition does not abort the refinery tick.
- Migrate the ledger from schema version 3 to 4 by additively appending nullable
  `verifications.attempt`. Existing rows remain `NULL` because their attempt
  cannot be inferred safely; all newly recorded rows store the task's current
  attempt.
