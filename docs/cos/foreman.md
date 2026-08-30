# cosmix-foreman — operator runbook

> **Development halted 2026-08-30 — until further notice.** The crate stays in
> the workspace and builds, but nothing here is being developed, deployed, or
> run; the fleet units are uninstalled and the ledger is archived. Treat this
> page as a historical runbook, not a live procedure.

`cosmix-foreman` runs task worktrees, applies the optional policy hook, verifies
completed work, and feeds successful branches to the refinery. This page covers
the completion-flow controls and the operator actions needed when a task does
not land.

## Operator procedure

### Completion charges and routing

A status name does not consume the escalation ladder. Foreman attaches at most
one explicit charge to each implementation run, and only for a runnable
`verifier_red` gate or a genuine `review_rejected` verdict. `task show` lists
every implementation attempt under `attempt_charges`; the run event stream
also ends with a `disposition` event carrying `ladder_charge: 0` or `1`.

Routing follows the typed finding reason:

- `verifier_red` and `review_rejected` each add the attempt's single ladder
  charge; the combined `ladder_failures` count advances after that rung's
  patience is exhausted;
- `branch_contract` and MCP self-bounces do not charge quality, but their
  recurrence counter parks the task at `branch_contract_limit` (default 3).
  Normal agent completion does not reset it because the refinery has not yet
  judged the branch; successful landing or an explicit operator requeue does.
  The count is categorical rather than error-text fingerprinted: three
  different branch-contract failures still require a human hand-off;
- `rebase_conflict` is recorded before claim and the agent still launches on
  the cleanly-aborted branch with a mandatory rebase-first instruction;
- `policy_denied` files an operator blocker and does not charge. Refinery lane
  or credential denials receive the same 30-second delay as infrastructure
  refusals and park after the operational-refusal threshold (default 3), with
  the exact misconfiguration in the blocker. The recurrence is read from
  durable policy findings rather than the infrastructure counter, so an agent
  completing another attempt cannot erase it; an operator requeue starts a
  fresh sequence;
- a same-rung retry resumes its recorded Claude/Codex session when possible;
- vendor and harness failures increment `infra_refusals` without charging the
  ladder. Retry delay grows by 30 seconds per consecutive refusal, capped at 30
  minutes. The first refused task is skipped for the rest of that sweep, so it
  does not consume a `--max-tasks` slot ahead of ready work. At
  `FOREMAN_INFRA_REFUSALS_FINDING` (default 3) Foreman files a major finding;
  at `FOREMAN_INFRA_REFUSALS_PARK` (default 10) it parks the task and promotes
  that finding to blocker with the last refusal verbatim. A later
  non-infrastructure disposition resets the consecutive count. After fixing a
  parked task, explicitly requeue it.
- a budget-enforcement refusal is recorded against that exact rung, then the
  planner advances to the next usable rung or parks if none can meter it.

Every task enters at `start_rung` (default `0`, the first configured rung),
regardless of its risk. Risk controls landing gates such as two-arm review; it
does not select the implementation model. The ladder advances only through
charged failures or a pre-claim refusal of an exact rung. `start_rung` is read
with the rest of `foreman.conf.mix` for each invocation, and must name an
existing rung in the resolved ladder; an out-of-range value is a configuration
error, never silently clamped. A task's rung is derived from its charge count
at every dispatch, so raising `start_rung` while tasks already carry charges
moves them further up than the new entry — the charges are counted from the
new entry, not re-taken there. A `--budget` task is admitted only if a
dollar-metering rung is reachable from `start_rung`.

Only byte-exact, case-sensitive final `VERDICT: APPROVE` and `VERDICT: REJECT`
lines are delivered quality verdicts. Leading/trailing whitespace, missing,
hedged, lowercase, or otherwise malformed verdict lines remain
fail-closed but route as harness delivery failures, so they never advance the
quality ladder.

The refinery deliberately defaults an unannotated landing error to a
`branch_contract` task bounce. Only failures explicitly marked at their source
as host infrastructure—filesystem I/O, process/Git plumbing invocation, or
ledger I/O—use `infra_refusal` and its backoff. This direction is a safety
boundary: newly added fallible parsing or validation cannot turn agent-derived
content into an invisible queue-stopping infrastructure error. Task-fault
wrappers preserve an infrastructure classification already attached by an
inner call. Cargo has no typed cause channel: Foreman treats recognisable local
manifest, lockfile and dependency-resolution diagnostics as branch faults and
routes every ambiguous non-zero exit as infrastructure. The trade-off is a
retry for some obscure bad branches rather than parking an innocent task for
host I/O or a corrupt Cargo cache. The refinery's offline `cargo metadata` and
`cargo update` children have a 120-second wall deadline and a 4 MiB cap on each
captured output stream. A deadline kills and reaps the whole child process
group and routes through infrastructure backoff, so package-cache lock
contention cannot stall the sole landing lane indefinitely.

`ladder_patience` remains a positive integer for one fleet-wide value, or may
be a map with an optional `default` and agent/full-rung overrides:

```mix
start_rung: 0
ladder_patience: {"default": 2, "glm": 1, "claude:opus": 3}
branch_contract_limit: 3
```

Use the one-shot `FOREMAN_START_RUNG` override when needed. It has the same
environment-over-config precedence as the other fleet policy values.

### Landing-owned versioning

Agent-authored version edits are tolerated but discarded: the refinery resets
the rebased value to the integration-base value, files an informational finding,
then applies its own bump. `task add --bump patch|minor` records the operator's
explicit SemVer intent, and `task set ID --bump patch|minor` can correct it
while it is unclaimed and not running or landing. Explicit intent takes
precedence. When the field is absent, the refinery preserves the historical
derivation exactly: PATCH by default, MINOR when `risk=high` or `kind` is
`feature`, `breaking` or `schema`. `task show` reports `effective_bump` and
whether its `bump_source` is `explicit` or `derived`. After a
branch rebases cleanly in the detached landing worktree, the refinery discovers
the package manifests owning changed files from the pinned integration-base
tree, applies the effective bump, drops any pre-release and build metadata
belonging to the old version, refreshes the nearest Cargo lockfile offline, and creates a
`refinery: version packages for task N` landing commit. Workspace-inherited
versions are resolved and updated at `[workspace.package].version`. The
rebased package name and effective workspace root must still match the
integration base. Cargo's membership-affecting workspace fields are
`[workspace].members` (adds explicit members) and `[workspace].exclude`
(removes paths from membership); either changing is a typed task bounce.
`[workspace].default-members` only changes the default package selection when
`--workspace` is absent, so it does not alter membership and the refinery's
workspace verifier does not treat it as a membership field. Changing
`[package].workspace` is a typed task bounce for concrete-version and
workspace-inherited packages alike. Lockfiles are discovered from the
integration base, so a branch cannot evade relocking by deleting one. Package
manifests and both base-owned and newly added workspace lockfiles must remain
regular contained files and are opened without following symlinks. A changed virtual-workspace manifest is checked even when no package
owns the changed path: `[workspace].members` must match the integration base,
an edited `[workspace.package].version` is reset with the same informational
finding, and the base-owned workspace lockfile must remain a regular contained
file. An unusable orphan manifest already present in the integration base is
named in the bounce finding; it cannot stop later queue entries, and a task
which removes that manifest may land without validating the bytes it deletes.
The healing exception covers malformed TOML and valid TOML carrying a package
or workspace version the landing path cannot bump (including non-three-part
SemVer or component overflow). It still requires no Cargo-manifest ancestor
and applies only when that exact manifest is removed. Manifests the task keeps,
and deletion of usable package or workspace manifests, follow the normal
authority checks and bounce.
Verification and merge authority judge the resulting landing commit.

This keeps two open branches for the same crate off the shared version line;
they can land back-to-back and receive successive versions. A bump-only
`task add --crate` branch remains supported: its authored bump is reset, then
re-applied under refinery authority (including an empty refinery ownership
commit when the resulting bytes already match). Ordinary implementation specs
should not contain a version-bump step.

### Crate scope

The policy hook normally derives a task's crate scope from committed branch
history. A crate is in scope after a non-manifest file below
`crates/<crate>/` appears in `git diff <integration-base>..HEAD`. An
uncommitted worktree or index change does not grant authority: commit the code
change first.

Ordinary task branches no longer bump versions — the refinery versions the
packages owning changed files at landing (see "Landing-owned versioning") —
but `crate_is_task_scoped` also authorises ordinary package-manifest work,
including dependency additions, removals, and re-pins. Use repeatable
`task add --crate` when a manifest-only maintenance task has no committed
non-manifest change from which to derive scope:

```sh
foreman task add "bump cosmix-foreman" \
  --spec-file task.md \
  --crate cosmix-foreman
```

`--crate` is policy authority stored in the protected ledger. A crate name in
free-form task title or spec prose deliberately grants nothing.

### Integration base

`foreman run --policy` and `foreman dispatch --policy` accept `--integration`
(default `main`). Before dispatch, the runner resolves that ref to the current
`merge-base` with the task branch and bakes the immutable commit into the hook
as `--integration-base`. Every tool call in the session therefore compares
committed history against the same base even if the integration branch moves.

`policy-check --integration-base` is hook plumbing, not an agent authority
input. If the runner cannot resolve the configured integration ref, it refuses
to launch the policy-gated session.

### Version-bump bounds

An in-scope package manifest is open by default. Its proposed bytes must be
valid TOML and must retain the `[package].name` found in `HEAD`; within that
identity it may add, remove, or re-pin dependencies and change features,
build configuration, or other manifest tables. The refinery discards agent
version edits and owns both versioning and lockfile refresh at landing.

Projects which deliberately need the former fence may set
`restrict_manifest_edits: true` in their operator-owned project manifest.
Only in that opt-in mode may proposed `Cargo.toml` content replace exactly one
line: the `[package]` `version = "…"` assignment. `validate_version_step`
then requires a strict increase within the same major version. The matching
one-line version replacement in that source-less workspace package's
`Cargo.lock` block remains the only directly accepted lockfile edit.

Out-of-scope package manifests are refused regardless of that flag. The hook
always reconstructs and judges the resulting whole-file content, so `Write`,
`Edit`, and an exact shell heredoc receive the same decision.

### Shell policy

The only shell write shape whose proposed bytes the hook accepts is a lone
quoted-delimiter heredoc:

```sh
cat <<'MANIFEST' > crates/example/Cargo.toml
# complete proposed file
MANIFEST
```

For a task-scoped package, that shape can write any valid manifest content
which retains the package identity, including dependency add/remove/re-pin.
With `restrict_manifest_edits: true`, it is limited to the version-only bounds
above.

All other shell segments which mention `Cargo.toml` or `Cargo.lock` escalate
unless their command is one of these recognised read-only shapes:

- `cat`, `ls`, `head`, `tail`, `wc`, `grep`, `rg`, `diff`, or `stat`
- `git diff`, `git show`, `git log`, or `git blame`
- `cargo build`, `check`, `test`, `clippy`, `fmt`, `metadata`, `tree`, or
  `doc` with the Cargo manifest supplied through `--manifest-path`

The hook also escalates `cargo add`, `remove`, `set-version`, `update`,
`generate-lockfile`, and `upgrade` even when the command names no manifest.
Editors and replacement commands such as `sed`, `perl`, `awk`, `python`,
`ruby`, `cp`, `mv`, `git restore`, and `git checkout` fail closed because the
hook cannot reconstruct their proposed content.

## Durable state

Ledger schema 14 installs the attempt-charge fields and reason-specific routing
counters/backoff timestamp as one SQLite transaction with the version stamp.
Schema 15 adds nullable operator-owned task bump intent; `NULL` retains the
historical derivation. Schema 16 adds nullable finding resolution text and
timestamp fields. Landing or retiring a task resolves its open findings with a
reason naming that terminal outcome; bounced and parked tasks retain theirs.
The schema-16 migration first reclassifies historical `policy-gate` findings
titled `policy escalation` from blocker to info, then reconciles findings on
tasks already landed or retired. It does not close findings on active tasks.
An older binary refuses an upgraded ledger instead of silently applying
semantics it does not understand.

From 0.14.0, Foreman never creates a default ledger below the process working
directory. It resolves the database in this order:

1. `--db <path>`
2. `FOREMAN_DB`
3. `STATE_DIRECTORY/ledger.db`
4. an existing legacy `./.foreman/ledger.db`
5. `COSMIX_VAR/foreman/ledger.db`, or the normal XDG/FHS Cosmix variable-data
   directory when `COSMIX_VAR` is unset

The legacy check is read-compatible only. When it selects an existing ledger,
Foreman prints one deprecation note naming the resolved path. It never creates
that cwd-relative ledger. Move callers to `--db` or `FOREMAN_DB` before moving
the file.

The final user path is `$XDG_DATA_HOME/cosmix/foreman/ledger.db`, falling back
to `$HOME/.local/share/cosmix/foreman/ledger.db`. Root's system path is
`/var/lib/cosmix/foreman/ledger.db`. Derived directory environment values must
be absolute, so a malformed unit cannot silently recreate state below its
working directory. The `STOP` kill switch remains a sibling of the ledger.
Only `--db` and `FOREMAN_DB` may create missing parent directories. The
directories selected through `STATE_DIRECTORY` and XDG/FHS state resolution
must already exist, so an operator typo cannot materialise a second fleet.
The resolved creation mode is inherited unchanged by policy-hook and
mayor-spawned MCP children.

### Optional filesystem sandbox

`FOREMAN_SANDBOX=bwrap` enables Foreman's bubblewrap filesystem allow-list for
dispatched Codex, Claude and GLM lanes. Unset, empty, and
`FOREMAN_SANDBOX=off` leave it disabled. The default remains **OFF** while the
view is soaked; fleet task 26, not task 25, decides whether to change that
default. Any other value is an operator error and refuses the lane rather than
silently running it unsandboxed.

The view replaces `$HOME` with a tmpfs and binds back the task worktree, its
Git common directory, the pinned Cargo target and Cargo cache read-write. The
Rust toolchain, selected sibling dependency clones, `~/.local/bin` and
`/opt/cosmix/bin` are read-only. Claude and GLM also receive the native Claude
install root read-only. A policy-gated Claude/GLM run composes its hook into
that same view with these exact-path mounts:

- the Foreman hook executable and per-run Claude settings file read-only;
- the host ledger read-write, with live SQLite WAL/SHM sidecars;
- in project mode, the project manifest, canonical project repository and its
  Git common directory read-only. Manifest loading needs all three before it
  can verify repository identity and open the project-bound ledger.

The project repository bind is only for manifest startup and remains
read-only unless it is already the run's writable worktree. The writable
ledger mount lets the hook record denials; it does not replace the hook's
gate-path rule, which still denies agent attempts to modify the ledger or its
settings.

Credential reachability stays lane-specific:

- Codex sees `~/.codex` read-only and cannot see Claude or Zcode state.
- Claude sees its own `~/.claude` directory and `~/.claude.json` read-write so
  OAuth/session refresh works, but cannot see Codex or Zcode state.
- GLM receives its Z.ai token only in the scrubbed child environment. It sees
  the Claude installation binaries but not Claude's stored OAuth/session
  state, Codex state, Zcode state, or the Foreman environment file.

Enabling the sandbox adds hard refusal modes. A missing `bwrap`, unknown mode,
non-absolute required path, missing or wrong-type hook input, unreadable
executable/settings/manifest/repository, or non-writable ledger refuses the
lane before the agent starts. Required binds are not existence-filtered after
validation, so a source that disappears during launch makes bubblewrap refuse
the whole child. Once the hook executable starts, every failure before policy
evaluation — including manifest parsing/identity checks and ledger open —
prints a clear `foreman policy: ... denying` line and exits 2. Claude Code
therefore treats an indeterminate startup as a denial, never as permission to
proceed.

### Project manifests

Use `foreman --project <manifest.mix> …` to bind an invocation to a repository
without relying on the caller's working directory or the ambient state ladder.
The manifest is strict-data Mix. `name`, `repo`, `db`, `cache_dir`, and a
non-empty `instruction_pack` are required; one ledger per manifest is the
isolation boundary. Relative paths resolve beside the manifest. Keep the
manifest, ledger and cache outside the managed repository: Foreman refuses
operator control or state below an agent-writable repo. The manifest's
canonical parent determines a derived `.foreman-<manifest-stem>-<name>` root:
manifest-mode task worktrees and that project's `clone.lock` and `verify.lock`
live there, not under the repository's possibly shared parent or the host-wide
verifier namespace.

```mix
name: "example"
repo: "repo"
db: "state/ledger.db"
cache_dir: "cache"
integration: "trunk"
branch_template: "change/{id}"
worktree_template: "work-{id}"
package_manifest_template: "crates/{crate}/Cargo.toml"
verifier: "project"
profiles: {
  project: {
    cwd: ".",
    tier0: [["cargo", "fmt", "--check"], ["cargo", "test"]],
    tier1: [["cargo", "test", "--workspace"]],
    tier2: []
  }
}
landing_tier: 1
landing_review: true
landing_gate: ["cargo", "test", "--locked"]
push_remote: "publish"
instruction_pack: "Use trunk as integration. Follow this repository's release policy."
lanes: { codex: { credentials: ["PROJECT_TOKEN"] } }
```

Each verifier tier is an ordered list of argv lists. A step may instead be
`{ argv: [...], opaque: true }`; opaque exceptions exist only in the
compile-time built-in profile table or an operator-owned manifest, never in
task-supplied data. The step's exit code alone decides pass/fail. Opaque only
records Cargo target and executable provenance as `unavailable`; under task
44's landed invariant provenance can never change the verdict. Transparent
steps keep the normal immediate Cargo metadata preflight, private target pin
and executable provenance. `run`, `dispatch`, `verify`, `refine`, `gc-cache`,
task authoring, MCP completion and mayor sessions all consume the same
manifest snapshot.
Each manifest gets a state root derived from its canonical filename and
validated project name. Relative `db` and `cache_dir` paths resolve beneath
that root, as do `clone.lock`, `verify.lock` and manifest-mode worktrees; two
manifests in one directory therefore cannot share those namespaces. Existing symlinks are
resolved before containment checks, and state that escapes the per-manifest
root is refused. The manifest `(name, repository identity)` pair is stamped
into its ledger on first project-mode open. Repository identity is the sorted
Git root-commit set, so moving the same checkout retains identity while an
unrelated repository with the same manifest name is refused. A populated legacy ledger without an
identity stamp is not adopted implicitly; migrate it explicitly or select a
fresh per-project ledger. Empty schema-only ledgers may be stamped on their
first project-mode open. Explicit repository/workdir and integration flags are
assertions in project mode: an identical value is accepted, but they cannot
redirect the invocation. `--db` is likewise fixed. These flags retain their
normal override behaviour when no manifest is active.
Manifest lane eligibility and non-empty credential requirements apply to task
implementation, MCP routing/claiming, merge-review arms, and `push_remote`.
Remote delivery requires at least one lane with a non-empty credential list
whose every named variable has a non-empty value. Only that selected set is
forwarded into the otherwise cleared, non-interactive Git child environment.
A route outside that policy fails closed before verifier spend or reviewer
launch, files an operator blocker, and leaves the quality ladder unchanged.
The operator must correct the manifest/credentials or re-route the task;
policy denial does not automatically advance to another rung.
Policy-hook children inherit `--project`, so they reload the same immutable
manifest identity. Their integration diagnostics and task-ref exclusions use
`integration` and `branch_template`. `package_manifest_template` tells the
hook how to map a package manifest to existing task crate scope; omit it when
the project wants no automatically scoped package-manifest edits. A literal
`Cargo.toml` supports a root package, while `{crate}` is one complete path
component for multi-package repositories. `restrict_manifest_edits` defaults
to false and opts the project into the former version-only fence when true.
The required `instruction_pack` is limited to 8192 UTF-8 bytes at manifest
load. An oversized pack is refused; mandatory project policy is never accepted
and then silently truncated. Project-mode refinement also ignores ambient
`FOREMAN_SIBLING_REPOS` refreshes, so it cannot fetch or fast-forward another
fleet project's supporting clones. An omitted `landing_gate` means no project
gate; project mode never inherits `FOREMAN_LANDING_GATE` or the fleet config.
When `push_remote` is configured, a successful local landing is followed by a
bounded update push whose refspec is the immutable journalled
`<verified-sha>:refs/heads/<integration>`. A racing local branch move can
therefore cause a recorded rejection but can never substitute a different
commit. Success, a machine-readable single-ref rejection, and ambiguous exits
are persisted as `succeeded`, `failed`, and `unknown` respectively. Omitting
`push_remote` keeps delivery local and prints an explicit remote-update no-op;
it is not an error or a silent skip. After a proven successful integration
update, Foreman prunes only the branch in the landed task's own record. The
delete uses its separate journal row and the same bounded outcome taxonomy;
an ambiguous delete remains `unknown` and is not blindly retried. A supplied
branch name or refspec is refused before Git can contact the remote.
MCP `task_complete` treats its optional branch field as an assertion against
the task row; it cannot set or replace the recorded branch used by cleanup. A
project MCP claim provisions and records the manifest-named worktree and
branch. Completion canonicalises the caller's `workdir` and refuses unless it
is that recorded linked worktree on that branch. MCP `task_bounce` requires
the `attempt` generation returned by claim; a delayed bounce from an older
same-name claimant cannot disposition or increment counters on a newer claim.
Every dispatch claim also returns a 24-hour `lease_until`. Local runner claims
renew it every five minutes. MCP and remote workers have no controller-local
pid and renew the same generation-fenced lease with `task_heartbeat`; they
should call it periodically during long work and before a long quiet operation.
Completion, bounce, requeue and every other normal release clear the lease in
the same SQLite write that clears `claimed_by`.

### Verifier lane

Legacy invocations serialise Cargo verification through
`/tmp/.foreman-verify-<uid>.lock`. A project-manifest invocation instead uses
its derived `<manifest-root>/verify.lock`, so an unrelated project cannot
contend with the fleet lane by construction. Set
`FOREMAN_VERIFY_LANE=<absolute-path>` to select a private lane explicitly; the
environment value wins over both defaults. Tests which spawn a real child
Foreman must set this to a path below their own temporary root with
`Command::env`, never by mutating the test process environment.

Lane acquisition uses non-blocking polling and
`FOREMAN_VERIFY_LANE_WAIT_SECS` bounds the wait (900 seconds by default; use a
short value such as 30 seconds in tests). The lock file is stamped with the
holder's pid, `/proc` start time and acquisition time. A timeout reports that
holder. If a nested Foreman has no private lane and its ancestor already holds
the host lane, it refuses immediately with `would deadlock on the host verify
lane held by pid …; set FOREMAN_VERIFY_LANE`; this condition cannot resolve by
waiting.

Policy values resolve as environment override, config file, then compiled
default. The config file resolves from an existing `foreman.conf.mix` beside
the ledger, then
`CONFIGURATION_DIRECTORY/foreman.conf.mix`. Keeping the beside-ledger check
ahead of `CONFIGURATION_DIRECTORY` preserves existing fleet roots. If no file
exists, Foreman names the missing path on stderr instead of presenting the
compiled defaults as configured policy.

## Merge-authority review

The reviewer receives a complete changed-file index rather than the first
64 KiB of a patch. Each entry gives the repository-relative path,
additions, deletions, and hunk count. The index is complete-or-error under its
64 KiB prompt cap; Foreman never silently drops its tail. The reviewer must
inspect every indexed path from repository objects with `git show
<tip>:<path>` and, where it exists, `git show <base>:<path>`. It must not
dereference worktree paths. Foreman rejects changed symlinks and gitlinks
before the session starts, so a branch cannot turn mandatory inspection into
a read outside the repository. The existing per-review token reserve is
enforced against cumulative input (including tool/file results) as well as
output. Task text keeps both its beginning and acceptance tail when its 8 KiB
prompt cap applies. A completed session that does not report affirmative input
usage is rejected because Foreman cannot prove that cap was enforced.

Every review has a shell-owned rubric. A diff touching `cosmix-foreman` keeps
the existing eight-point harness checklist. Every other diff is judged for
correctness and edge cases, tests which genuinely prove the change, correct
versioning of observable behaviour, and matching documentation.

### Session continuation and fallback

Every implementation retry at the same ladder rung (the same agent and model)
resumes the immediately preceding recorded session. The resumed turn contains
the new findings and the safety framing needed to treat them as evidence, not
instructions; it does not resend the cold task prompt. A rung change, missing
session id, or first attempt starts cold with the full task prompt.

Merge-review conversations persist independently per `(task, reviewer arm,
model)`. A later review of the same stable task worktree resumes that arm with
a re-review turn naming the new tip, the current complete changed-file index,
the prior-finding disposition request, and the JSON response contract. When
the current diff touches `cosmix-foreman`, the turn also repeats the harness
checklist even if the conversation originally opened on a non-Foreman diff.

Resume identity is fail-closed. Any session id reported by init or the terminal
result must equal the requested id; a mismatch fails the run and clears its
resumable reference. A resumed stream that reports neither id is not proven to
be the requested conversation and can never approve. For a cold Claude stream,
a terminal result id backfills an id omitted by init so the next sweep can
resume it.

An exact vendor session-not-found response, with no other stdout or stderr, is
treated as a pruned conversation before model work. Foreman journals the
fallback, retires the dead id by clearing `runs.session_ref`, and starts fresh
with the full prompt only when it can prove residual capacity. Output tokens,
dollar spend, and elapsed wall time are subtracted from the original caps; an
exhausted cap or unknown capped spend refuses the fallback. Any rendered or
additional output likewise fails closed instead of authorising another
session.

The reviewer may put prose first, but its final non-whitespace content must be
one raw JSON object or one fenced `json` block with this exact shape:

```json
{
  "verdict": "REJECT",
  "findings": [
    {
      "severity": "MAJOR",
      "file": "src/example.rs",
      "line": 42,
      "title": "Short title",
      "body": "Actionable explanation"
    }
  ],
  "files_inspected": ["src/example.rs"]
}
```

`verdict` is exactly `APPROVE` or `REJECT`; severity is exactly `BLOCKER`,
`MAJOR`, `MINOR`, or `NIT`; paths are normalised repository-relative strings;
and line numbers are positive. Unknown/missing fields, malformed JSON, and
prose-only verdicts reject fail closed. `BLOCKER` or `MAJOR` findings reject
even if the supplied verdict says `APPROVE`. Each indexed path absent from
`files_inspected` creates a synthetic MAJOR at `path:1`, so incomplete review
coverage rejects by construction.

Validated findings go straight into the `findings` table as individual typed
rows with `severity`, `file`, `line`, and the owning review `run_id`. The
complete finding batch and tier-3 verdict commit in one SQLite transaction, so
recovery cannot leave partial or duplicate review rows. The parsed verdict,
full prose, JSON response, and files-inspected list remain in tier-3
verification evidence. MCP task detail exposes the structured location and run
without reconstructing either from prose. If Foreman is interrupted after this
batch commits but before the Git compare-and-swap, a retry reuses only the
approved batch for the exact task attempt, base, and tip; any changed SHA gets
a fresh review. Detached rebases use the original author date as the committer
date so replaying an unchanged branch onto the same base produces that same
reviewed tip rather than a timestamp-only SHA change.

### Reviewer defaults and evidence

The 2026-08-25 operator comparison on the same branch tips recorded Opus at
40 approvals / 21 rejections and Codex at 4 approvals / 26 rejections. On task
30 specifically, Opus approved four times while Codex rejected five times; the
Codex rejections were substantive. This supports Codex as the default single
arm for non-high tasks and Opus as the independent second arm for high-risk
tasks. It is evidence for fleet defaults, not a hard-coded trust decision.

The compiled defaults are equivalent to:

```mix
review_primary: "codex"
review_secondary: "claude"
codex_review_model: "gpt-5.6-sol"
codex_review_stall_secs: 900
review_model: "opus"
review_stall_secs: 300
two_arm_review: true
```

The stall clocks are per review family because their event streams behave
differently. Claude normally streams progress and keeps its 300-second silence
budget. Codex may reason without output, so its default is 900 seconds. Set
`codex_review_stall_secs` (or `review_stall_secs` for Claude) in the resolved
`foreman.conf.mix` to change the next invocation without rebuilding Foreman.
The one-shot overrides are `FOREMAN_CODEX_REVIEW_STALL_SECS` and
`FOREMAN_REVIEW_STALL_SECS`. Both values must be positive. The independent
1200-second review wall remains unchanged and still wins if it expires first.

`FOREMAN_QUIET` suppresses the informational stderr message emitted when an
exactly identified dead review session is retired and replaced with a fresh
review. It does not change fallback classification, journalling, budgets, or
the resulting verdict; it exists for test fixtures and callers that capture
Foreman's stderr.

Codex CLI 0.145.0 exposes `exec --json`, but no heartbeat interval or option to
stream in-progress reasoning. Foreman already treats its `turn.started`,
`item.started`, `item.updated`, and completed reasoning events as progress;
there is no reliable event to consume while a reasoning item itself is silent.
A harness stall, wall, or token-budget kill is recorded as `harness_error` and
the review finding names the Foreman budget that expired. A vendor-side failure
remains `vendor_error` (or `resource_exhausted` for a vendor/driver resource
ceiling), so the two causes do not collapse into the same evidence.

Set `two_arm_review: false` to use only the primary on high-risk tasks, swap
the two distinct families with `review_primary` / `review_secondary`, or set
`review_override: "claude"` (or `"codex"`) to force one arm for every risk.
The fixed override wins over two-arm routing. One-shot equivalents are
`FOREMAN_REVIEW_PRIMARY`, `FOREMAN_REVIEW_SECONDARY`,
`FOREMAN_TWO_ARM_REVIEW`, and `FOREMAN_REVIEW_OVERRIDE`; model overrides remain
`FOREMAN_REVIEW_MODEL` and `FOREMAN_CODEX_REVIEW_MODEL`.

A system unit can provide the state and configuration roots directly:

```ini
[Service]
StateDirectory=cosmix/foreman
ConfigurationDirectory=cosmix
```

Cargo targets remain per-worktree under the task-44 isolation contract. They
do not belong below any cache directory. Existing cmctl units pass `--db`
explicitly, so their `ExecStart` paths continue to win and need no coordinated
edit for this change.

## Rust tier-1 feature coverage

For tasks whose `crates` column is non-empty, Foreman asks `cargo metadata
--all-features` for the resolved workspace graph and expands those named
packages to include every transitive workspace reverse dependency. Tier 0
runs fmt, clippy and tests only for that closure. An empty `crates` column
keeps the historical whole-workspace command shape.

The rust profile's tier 1 keeps exactly one default `cargo test --workspace`
suite, then discovers non-default Cargo features only in the same task/reverse-
dependency closure. A task with `crates=[]` retains full-workspace feature
discovery. Each selected feature gets its own
`cargo test -p <crate> --features <feature>` step.
The unit is deliberately one crate and one feature, not workspace-wide
`--all-features`: enabling every feature across this workspace also enables
mesh/citizen integrations which can require a live broker or host library and
fail for reasons unrelated to the task being landed.

Two established repository conventions are not auto-run: `_...` features are
private harnesses, and a feature named `cosmix` is a live Bus-citizen build.
Every such omission is named in the verification report. Cover selected cases
on a suitably provisioned host with an invocation-scoped override:

```text
FOREMAN_FEATURE_SETS="some-crate:feature-a,feature-b"
```

The override replaces discovery and uses the strict
`crate:feature[,feature]` format with `;;` between crates. An empty or malformed
value is a red verification gap, not an empty green lane. Cargo also fails
loudly if a configured crate or feature no longer exists. For an
environment-bound feature discovered on a host that cannot support it, record
the exception explicitly:

```text
FOREMAN_FEATURE_EXCLUDE="cosmix-musicd:jack"
```

Exclusions apply only to auto-discovery and are listed in the report. Failure
to run or parse metadata is also red. A workspace with genuinely no runnable
optional features records a passing informational step rather than silently
omitting the feature dimension.

## Cargo target-directory isolation

From 0.13.0, Foreman does not share Cargo's `target` directory between
worktrees. Cargo can give same-named local crates from sibling worktrees the
same shared output slot and then accept the wrong tree's binary as fresh. The
safe boundary is one target directory per Cargo workspace checkout.

### Paths and child environment

Foreman pins `CARGO_TARGET_DIR` in every verifier child and every Claude
or Codex agent child. One function derives the pin from the Cargo
workspace root the verifier uses, so the agent's dry run and the verifier
build into the same `<workspace-root>/target`. In the normal fleet layout
that single path means:

- a task checkout uses
  `~/.cmctl/.foreman/task-<id>/src/target`;
- the refinery rebases and verifies that same registered, now-unclaimed task
  checkout, so landing consumes its warm target; legacy/manual branches with
  no dedicated task worktree use a private detached fallback; and
- an installer verifier-PROBE clone uses that clone's own `src/target`; the
  deliberate WARM exception is described below.

Before a verifier executes any Cargo step, Foreman runs `cargo metadata`
directly using Cargo resolved from the verifier process's own PATH, excluding
relative entries and executables inside the worktree. It never executes or
trusts stdout from the command's `env`, memguard, timeout, flock, shell, or
other wrapper during preflight. Recognised `env K=V` assignments are parsed
and applied as environment data, then the verifier pin is applied last.
Target-affecting `+toolchain`, `--manifest-path`, `--config`, and
`--target-dir` arguments are preserved. The probe's job is only to confirm
that the manifest resolves and Cargo reports exactly the pinned target.
Thus `env CARGO_TARGET_DIR=/shared cargo test` still reports and uses the pin.
A conflicting argv-level target or an escaping workspace/target symlink is
refused before the real command runs.

An opaque shell string such as `sh -c 'cargo test'` has no separately
addressable cargo argv to transform, so it is refused with a clear error. It
may run only when that exact `(profile, tier, step)` is declared opaque in the
built-in profile table or operator-owned manifest. The command's exit code is
still the complete pass/fail verdict. Such a declaration only records target
and binary provenance as `unavailable`; provenance is diagnostic and, by task
44's invariant, can never change pass/fail. Task data cannot declare an opaque
exception. No built-in profile currently declares one.

### Verification provenance

Tier 0 records only the resolved private `target_dir`; it does not compile or
hash an additional provenance snapshot. Tier 1 selects one principal test
step per report (the rust profile's single `cargo test --workspace`) to carry
`executed_binaries`, and records `provenance_tier: 1`. Every other step omits
the field. Around that one step Foreman runs the same transparent wrapper and
Cargo selectors with execution disabled by `--no-run --message-format=json`,
the ambient target removed, and the private target already proved by preflight
fixed explicitly. It hashes exactly the non-null `executable` paths in Cargo's
`compiler-artifact` records. A warm, reused binary is still listed and hashed;
no mtime heuristic is involved.

Those JSON streams are Cargo control data from separate invocations chosen by
the verifier immediately before and after the tested process. They are not the
captured stdout of a test or benchmark, and code under test does not choose the
control argv.
Paths are still untrusted: Foreman rejects symlinks and non-regular or
non-executable files, canonicalises each path inside the verified private
target, deduplicates hardlinks, streams SHA-256 under per-file and aggregate
byte caps, caps control bytes and records, and shares the original step
deadline. A failed or timed-out listing, malformed record, escaping path or
cap breach is `unavailable`; a step other than Cargo test/bench is
not selected. Provenance remains diagnostic and cannot change the verdict.
The exact guarantee remains: "these bytes existed at these paths when the step
began and were unchanged when it ended; cargo ran them". A test may exec a
different file it carries itself; that remains outside provenance's claim.

### Unit change and warm probes

Delete this line from `foreman-dispatch.service`, `foreman-refine.service`,
and `foreman-tier2.service`:

```ini
Environment=CARGO_TARGET_DIR=%h/.cmctl/.foreman/target
```

The installer's WARM action is raw Cargo in the fleet workdir in
`install-foreman-units.mix`. Its explicit shared-cache target is the
operator's choice for this shared sccache-backed warm. It is not a
verification, and it runs main's own code rather than a task branch, so keep
that target. The installer's tier-0 PROBE runs `foreman verify`, which pins its
own workspace target. This is the exact unit hand-off; do not remove the WARM
target or invent tier-0/tier-1 unit edits.

### Disk and garbage collection

The isolation trades disk for correctness. Measured on the current host, the
one pinned `src/target` is about 13 GB for each live task worktree after a full
`--workspace --all-targets` build. The rejected split layout would have made
the agent warm `<worktree>/target` and the verifier rebuild independently in
`<worktree>/src/target`, wasting a cold build and roughly doubling that disk.
Both now reuse the single `src/target`. `sccache` does not reduce these
per-worktree target trees. Persistent registered `task-<id>` worktrees retain
their target across attempts. A legacy/manual landing instead recreates a
detached checkout at the deterministic sibling path
`.foreman-review-<repo>-task-<id>`. Reusing the path lets each reviewer arm
resume with the same cwd, but the checkout itself is short-lived: Foreman runs
`git worktree remove --force` followed by `git worktree prune` when that
landing returns, on success or failure, so its private target is removed too.

`foreman gc-scratch` is the fleet backstop. The refinery normally reclaims a
landed task immediately after its terminal ledger transition, but a crash or
an older terminal row can leave scratch behind. A process death can likewise
bypass the legacy review checkout guard; the sweep recognises the exact
`.foreman-review-<repo>-task-<id>` registration and removes it once that task
is `landed` or `retired`. Dry-run reports those checkout candidates without
removing them. A daily user timer and service ship as
`src/_etc/systemd-user/foreman-gc-scratch.{timer,service}`. Install both, copy
`src/_etc/cosmix/foreman-gc-scratch.env.example` to
`~/.config/cosmix/foreman-gc-scratch.env`, set `FOREMAN_PROJECT`, then enable
the timer:

```text
systemctl --user daemon-reload
systemctl --user enable --now foreman-gc-scratch.timer
```

Project mode takes the fleet root, repository and ledger from that manifest.
A non-project invocation remains available with explicit roots:

```text
foreman --db /srv/foreman/ledger.db gc-scratch \
  --fleet-dir /srv/foreman --repo /srv/foreman/workdir \
  --pool tank --terminal-age-hours 24 --pressure-percent 80 --confirm
```

A real (non-`--dry-run`) sweep also requires `--confirm`, or it refuses without
deleting anything. This is not an interactive prompt — the installed timer's
`ExecStart` always passes it, so the unattended backstop never blocks on a
person — it exists so that the bare command name, the thing a caller
unfamiliar with `gc-scratch` would type first against a live fleet, previews
instead of deleting. `--dry-run` still previews without it.

The ordinary pass selects only `landed` and `retired` tasks older than the
configured age. `running`, `landing`, every other non-terminal state, and
operator-driven tasks are always skipped. When `zpool list` reports the named
pool at or above the configured capacity, the same pass widens to younger
terminal tasks, newest first. The report records the real pool capacity before
and after and records scratch size using allocated blocks (`du -sB1`
semantics), not apparent file length. A failed pool probe is reported and
makes the command red, but the ordinary age pass still runs.

The effective sweep policy resolves from flags, then `foreman.conf.mix`, then
compiled defaults. Its keys are `scratch_terminal_age_hours` (24),
`scratch_pool` (unset until the operator names the real ZFS pool),
`scratch_pressure_percent` (80), and `scratch_shared_max_gb` (160 per shared
cache). Equivalent `FOREMAN_SCRATCH_*` environment overrides are available.
Every report records its RFC 3339 selection time. Pass that value back with
`--as-of` to replay age selection against the same ledger snapshot.

Only two task directory roots are eligible: a registered task worktree's
`src/target/` and the exact sibling `task-N-target/`. Worktree targets must be
untracked, gitignored, real directories in a worktree sharing
the named repository's Git common directory. Symlinked roots and containment
failures are reported and refused. The worktree, branch and tracked files are
never removed. Use `--dry-run` to print task-directory and shared-cache entry
candidates, their allocated before/after sizes, and the total bytes the same
stalest-first pass would reclaim, without deleting.

The shared `target/` and `target-refine/` caches are each bounded to a generous
160 GiB by default. The existing stalest-first cache GC removes entries only
under `{debug,release}/{deps,build,.fingerprint}` until the cap is met; it does
not cold-delete either cache. Before a shared cache is planned or gc'd it must
pass the same Git proof a task directory does: no tracked file anywhere beneath
it, and the cache root itself gitignored. A fleet root routinely sits *inside* a
checkout, so "it is called `target`" is not evidence that nothing in it is
tracked, and `{debug,release}/{deps,build,.fingerprint}` is a real place for a
tracked file to be. A cache that fails the proof — or whose Git ownership probe
is merely inconclusive — is refused whole and reported, in `--dry-run` as well
as for real, so a preview never advertises candidates the sweep would refuse.
Override the per-cache cap with
`--shared-max-gb N` or fleet policy. The generous default retains the 55 GiB
and 97 GiB hot caches deliberately kept after the 2026-08-28 incident while
preventing unbounded growth.

Worktree removal is a separate lifecycle decision and is not part of either
cleanup path. If it is added later it must be gated on the task branch being
merged or pushed; scratch reclamation is intentionally independent of that
irreversible choice.

### Recovering a stuck scratch-cleanup lease

Both cleanup paths — the refinery's post-landing reclaim and the `gc-scratch`
sweep — hold a durable ledger lease on a task for the whole time they are
deleting its scratch, so a concurrent requeue can never hand the worktree
back to a live run mid-delete. The lease is not a bare sentinel: it is
stamped with the reclaiming process's identity, as
`claimed_by = foreman-scratch-gc:pid=<pid>:start=<starttime>`.

That stamp is what makes the interlock enforceable rather than advisory. **No
requeue clears a scratch-cleanup lease while the process that took it is
still running — `--force` included.** `--force` overrides every other stuck
claim; this is the one exception, because what it would release is not a
stalled agent but an in-flight `remove_dir_all` on the very worktree the
requeue is about to make dispatchable again. Revalidating the lease between
candidate directories (which every leased reclaim does) is not sufficient on
its own: a check and a deletion are two operations, so a `--force` landing
just after a successful check could still dispatch into a deletion already
running. The refusal is therefore enforced where the requeue commits, and it
is decided by the host — is that pid actually alive? — not by the flag.

So the state is self-diagnosing and has exactly two outcomes:

- **The sweep is alive.** Every requeue is refused, naming the pid. Wait: the
  lease clears by itself the moment the sweep finishes. If it is genuinely
  wedged, `kill <pid>`, then `foreman task requeue --force <id>` — which now
  succeeds, because the pid is gone.
- **The sweep died** (OOM, host reboot, `kill -9`) before releasing. Nothing
  is deleting, so `foreman task requeue --force <id>` clears the inert lease
  immediately. The non-force refusal says so explicitly, and says the process
  is no longer running rather than making the operator judge that.

A lease is invisible to every later sweep while held (`begin_scratch_cleanup`
only takes an unclaimed row), and a landed/retired row is outside the dead-
claim reaper's `claimed`/`running` candidate set, so a stuck lease is never
silently resurrected into `queued` either.

## Operator-driven tasks

Some tasks must never be picked up by the unattended ladder — typically a
task that edits foreman's own gates (policy hook, verifier, refinery), which
an agent is forbidden to touch and an operator drives by hand. Before 0.11.0
the only way to keep such a task out of dispatch was to silence the wake
citizen and the dispatch timer for the duration of every operator run;
requeue → wake → dispatch otherwise reclaimed it within a second. That
procedure is retired: reservation is now a persistent, operator-owned ledger
flag (`tasks.operator_driven`, schema v6).

### Controls

```sh
foreman task add "…" --spec-file spec.md --operator-driven --reason "await trust decision"
foreman task set <id> --operator-driven --reason "debug with operator present"
foreman task set <id> --operator-driven=false --reason "Mark approved unattended work"
foreman task set <id> --verifier <profile>                   # correct its tier-0 profile
foreman task set <id> --bump minor                           # correct its SemVer intent
foreman task list                                            # reserved rows carry [operator-driven]
foreman task show <id>                                       # controls and effective bump
```

`task set` is operator CLI only; the MCP surface cannot change these controls.
Reservation and release reasons are mandatory and are filed as `info` findings
with `operator_reserved` / `operator_released` reason codes in the same
transaction as the flag change. Both directions are decisions that the next
operator needs to understand; making either reason optional would preserve the
exact hurried, unexplained path this audit trail closes. Repeating the current
state is a no-op and does not file another finding.
`--verifier` accepts the legal names from foreman's built-in verifier table (the
same table rendered by `foreman task add --help`). It refuses to change a task
which is running or landing. An accepted change stores the canonical profile
name and atomically files an info finding recording the canonical before/after
values.

### Effect

- Unattended `foreman dispatch` and MCP `claim` skip a reserved task even
  when it is otherwise ready; the refusal is explicit (`task <id> not ready:
  operator-driven`), consumes no attempt, and is not journalled as a run.
- Dispatch queue summaries list ready-but-reserved tasks separately
  (`dispatch: queue summary — operator-driven: …`) so a reserved task is
  visible, not silently idle. A legacy reservation with no reservation finding
  is labelled `[UNEXPLAINED]`; `foreman status` and `status --json` expose the
  same per-task explanation state for boards and other consumers.
- `foreman run --task <id>` — the explicit operator claim — still runs it.
- `task requeue` preserves the flag: a bounced or parked operator-driven
  task stays reserved until an operator clears it.
- Landing automatically releases the flag and records that release. Schema 17
  reconciles already-landed rows whose historical flag was never cleared.
- Dispatch decisions, successful tier-0 output and refinery landing outcomes
  name the verifier profile which ran, so the green path is auditable from the
  operator log without opening the verification JSON.

An operator run no longer needs the wake citizen or the dispatch timer
stopped; the flag is the mechanism. Once the operator run (or a hand-verified
branch) reaches `done`, the refinery lands it through the same gate as any
other task and removes the now-meaningless terminal reservation.

## Whole-file attachment harm

`foreman attachment-harm` is a read-only investigation of Claude's project
transcripts. It answers which existing files agents repeatedly receive whole
after a context compaction. This is the observed failure mode: a file fills
the context, the summary cannot carry it forward, and the same file is
attached again after the boundary.

```sh
foreman --db /srv/foreman/ledger.db attachment-harm
foreman --db /srv/foreman/ledger.db attachment-harm \
  --claude-projects /srv/claude/projects --limit 20 --json
```

The scanner streams one JSONL record at a time and drains any corrupt record
above 64 MiB without retaining it; malformed and oversized gaps make the
session incomplete and are reported. It accepts an attachment as a
whole file only when the vendor record says `type: file`, `startLine: 1`, and
`numLines == totalLines`; slices and records without complete extent metadata
are counted separately and excluded. A paired `compact_boundary` and
`isCompactSummary` is one compaction, not two. The exact
`runs.session_ref = <JSONL filename UUID>` join supplies task/run outcomes;
operator and Foreman sessions remain separate populations.

Ranking is by the number of whole-file reattachments after compaction, then by
the number of affected sessions and repeat attachments. The same logical file
must have appeared in an earlier compaction epoch: a file first opened after a
compaction is not evidence of reattachment. JSONL record character count (the
historical attachment-size measure), actual UTF-8 record bytes and decoded
file-content bytes are printed only as context and never break a tie.
Reports retain paths, sizes, session/task IDs and record positions, but never
attachment content, tool output or summary text. The command opens the ledger
read-only and never writes transcripts.

Two ignored tests provide opt-in acceptance against private data without
putting results or transcript contents in the repository. Set
`FOREMAN_ATTACHMENT_HARM_KNOWN_SESSION` to the known transcript to verify its
exact record positions and sizes. Set `FOREMAN_ATTACHMENT_HARM_CORPUS` and,
optionally, `FOREMAN_ATTACHMENT_HARM_LEDGER` to print the current top ten for
each population and task 111/112/113 coverage. Run the corresponding ignored
`attachment_harm` tests with `--nocapture`; the tests only emit report fields.

Absence is not evidence that a file is safe: it may be untouched in the
observation window or always read in slices. Likewise, this worklist is not a
line-count policy. Keep new files around 600 lines where that is cheap at
creation time; split existing files only when measured agent harm and code
cohesion justify it. A large cohesive table is not made better by cutting it
to meet a number.

## Per-task dollar budgets

`task add --budget <USD>` stores an operator-owned total dollar budget for a
task. The value must be finite and positive. It is separate from the fleet's
daily governor ceiling: authoring refuses a task budget above a non-zero
`daily_budget_usd`, while `daily_budget_usd: 0` means the daily dollar ceiling
is disabled. The configured ladder must contain at least one dollar-metering
lane.

For each attempt, Foreman holds the task's unspent remainder and passes that
same amount to the lane as its dollar cap. An explicit, narrower
`foreman run --max-budget-usd <USD>` wins; the hold and run cap are then that
narrower amount. A hold never exceeds the task remainder. The fleet governor
remains the final admission gate, so a large task budget does not widen the
daily ceiling.

Only dollar-metering lanes can run a budgeted task. A Codex or GLM rung is
refused before claim as a normal rung refusal, with no run row or reservation;
dispatch can continue to a later Claude rung. A task ladder with no metering
rung is refused at authoring time.

Known attempt costs charge their reported amount. If an attempt dies before
its first usage checkpoint or otherwise finishes without a price, it is not
free: Foreman charges the dollar amount reserved for that run. Legacy unpriced
runs which predate recorded per-run reservations conservatively charge the
then-remaining task budget. `foreman task show <id>` exposes `budget_usd`,
`budget_charged_usd` and `budget_remaining_usd`; `foreman status` and
`foreman status --json` report the same totals for every budgeted task.

When no remainder is available for another attempt, Foreman does not claim the
task or report a harness fault. It atomically parks the task and files a blocker
finding whose title names the remaining and required amounts. Dispatch counts
that as a parked task outcome, stays green, and can use the same `--max-tasks`
slot for another ready task.

The recovery procedure is explicit:

```sh
foreman task set <id> --budget <USD>    # replace/top up the total budget
foreman task requeue <id>               # resolve the blocker and make it ready
foreman task set <id> --budget clear    # remove the task budget instead
```

The replacement value is a new total, not an increment; amounts already
charged remain visible and continue to count. Budget changes are refused while
a task is claimed, running or landing. `task set` is operator-only and the MCP
surface cannot widen or clear this authority.
