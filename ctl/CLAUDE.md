# Cosmix Project Mandate (starter control folder)

**Foundational orientation for every Claude session on Cosmix. Read this first;
it supersedes contrary framing in subsidiary docs. The *frame* lives here; the
*working map* lives in `CODEX.md`.**

*This is the public STARTER copy that ships in `$COSMIX/ctl/`. Copy the folder
out (`mix $COSMIX/setup.mix --ctl ~/.ctl`) and it becomes your private hub;
edit freely there. `$CMCTL` below means that copy, wherever you put it.*

Rewritten 2026-07-17 around the ultracode-workflow methodology. The previous
mandate (dual-reviewer loop, C1/C2 escalation machinery) is archived verbatim at
`_doc/2026-07-17-CLAUDE.md`.

## Identity

IMPORTANT: We want a Recursive Self-Improvement system.

> **Cosmix is an agent-operable computing substrate: legible, modifiable, and
> reconstructible by design.** Not a desktop with AI features — an AI-first
> substrate that currently *manifests* as a Linux desktop + mesh service stack.

The substrate is the project; the desktop, the ARexx-flavoured IPC, the
sovereign mesh, the Mix language, and the maild stack are its surfaces. The
primary *operator* is an AI agent; the primary *user* is an AI-first developer
driving the substrate through such agents. Lineage: AmigaOS/ARexx (everything is
a message port), Smalltalk/Lisp machines (live self-reflective image), Plan 9
(everything is a namespace). The novel contribution is the AI agent as
first-class operator of self-observation, self-modification, and
self-rebuilding.

**Three design criteria** — every architectural decision is filtered by:

1. **More legible to agents?** State queryable as structured data; code paths
   introspectable; schemas agent-readable.
2. **More modifiable by agents?** Config mutable through structured channels
   (ABP verbs, property writes), not ad-hoc file edits; lifecycle agent-operable.
3. **More reconstructible by agents?** Build system agent-operable; components
   hot-swappable where safe; the system can rebuild parts of itself.

A change advancing none of these is unlikely to belong; one working against any
almost certainly doesn't. Resist normal-desktop drift: features are judged by
the criteria, not "what would GNOME do". Mix is Cosmix's ARexx-equivalent and
the primary control surface; when agent- and human-usability conflict,
agent-usability wins. Existing code is *exploration residue*, not a foundation
to preserve; design docs are proposals, and where they and the code disagree,
the three criteria decide.

## Canonical law: agentic-first — usability over security-by-default

**Set by Mark 2026-08-16 as CosMix law/lore. Weighs on every design call.**

The primary objective is an **agentic-first experience where the need for human
interaction is secondary.** Do whatever it takes to allow **free-flowing agentic
control and permissions** across both surfaces — the **desktop** and the **WG
mesh**. When a security mechanism forces a human into the loop, that mechanism is
the thing to remove or make **opt-in**, not the automation.

- **Do NOT over-engineer security at the expense of usability and flexibility.**
  A mandatory human gate (typed nonce, confirmation prompt, interactive
  approval) on a path an agent must drive is a design *bug*, not a feature.
- **Default open, opt-in hard.** Ship the unattended/agent-operable path as the
  default; make the guard rail a flag/property a human turns *on* when they want
  it — like every other part of the substrate, enabled *if, when, and as* it is
  actually needed. (Reference implementation: kms-live's takeover confirmation,
  made opt-in behind `--kms-confirm` 2026-08-16 — default takeover is unattended;
  ADR `_decisions/2026-08-16-agentic-first-security-is-opt-in.md`.)
- **This does not license removing real, non-optional invariants.** Guarantees
  that bind *correctness* — device/VT identity continuity, TOCTOU re-checks, the
  hygiene that stops the system seizing the *wrong* thing — stay unconditional.
  What goes opt-in is the *human-in-the-loop ceremony*, not the machine-checked
  binding underneath it. The test: "does this gate exist to stop an agent doing
  something wrong, or only to make a *human* vouch for it?" The second kind is
  the kind that becomes a flag.
- **Lock-down is a future, on-demand act.** Security features get built and
  turned on when a concrete threat makes them worth their usability cost — never
  pre-emptively, never as a reflex. Absence of a lock is the intended state
  until then.

When this law and a "more secure default" instinct conflict, this law wins;
escalate to Mark only if honouring it would forfeit a correctness invariant
above.

## The working loop (Claude drives, codex implements) — proven 2026-07-21

The default loop for any non-trivial change (companion to the ultracode-workflow
section below — that's orchestration, this is the inline convergence loop):

**scout → codex design consult → codex implements → verify independently, NEVER
trust the report → fresh-thread cold review → converge (no partial fixes) → bump +
commit + push.**

- **Scout inline** (grep/read) to ground the design in real file:line before asking.
- **Codex (MCP) designs then implements** on one thread per feature-arc (context
  stays sharp); the **cold review runs on a FRESH codex thread** for real
  adversarial distance. Give reviews a concrete hunt list (file:line + failure
  scenario); verify each finding against source before accepting; reject wrong ones
  with evidence.
- **Verify every codex claim yourself** — re-run build/test/clippy from the crate
  dir and read the load-bearing diff. Green reports have hidden a flaky test, wrong
  provenance, and a silent no-op run. Trust the tree, not the summary.
- **Converge — no partial fixes.** Fix by severity, re-review the fix, iterate until
  "no issues / all dispositioned". MINORs: fix or explicitly defer-with-reason.

Two hard-won disciplines (each caught a real miss this session):

1. **Pressure-test "is this earned / is this done" with codex BEFORE building or
   declaring done.** It refuted a wrong "this arc is finished" (a live double-action
   bug would have shipped) and a wrong "this app is a consumer" (a hollow crate would
   have been built). Resist over-engineering *and* premature "done".
2. **On a codex-MCP timeout, verify the TREE yourself.** Tasks can abort (~30 min) on
   *reporting* while the edits + tests actually completed — `git status`, build,
   test. Don't discard finished work; don't trust an absent report.

## This repo: $CMCTL, the private hub that drives the public monorepo

**Since 2026-08-30 the whole public project is ONE repository, `markc/cosmix`,
checked out at `$COSMIX` (default `~/Projects/cosmix`).** `$CMCTL` is the
private control hub that drives it: deploy/provision scripts, journals, plans,
specs, decisions, and mesh-private operational state. Sessions here have
authority to `cd` into `$COSMIX` to read source, run cargo, orchestrate
releases and push; cmctl itself holds no Rust source.

| Repo | Path | Visibility | Role |
|---|---|---|---|
| **cosmix** | `$COSMIX` = `~/Projects/cosmix/` | public | The monorepo: `src/` one Cargo workspace (Bus family + Mix + substrate libs + daemons, 48 crates flat under `src/crates/`), `src/desktop/` its own workspace, `docs/` the cosmix.dev site incl. the manuals' sources, `bootstrap` + `setup.mix` the install |
| **cmctl** | `$CMCTL/` | **private** | This repo: Mark's control overlay — drives `$COSMIX`, never enters it |

Dependency direction inside the workspace is still **bus ← mix ← cos**; cargo
enforces it. **Everything is keyed off `$COSMIX`**: `COSMIX_SRC=$COSMIX/src`,
`COSMIX_BIN=$COSMIX/bin`, `etc/var/run/log/tmp` likewise, resolved by one rule
in `src/crates/cosmix-lib-config/src/paths.rs` (mirrored in
`cosmix-mix/src/cosmix_paths.rs`): env `COSMIX`, else self-located from the
running binary, else the default. A fresh machine is `git clone … && ./bootstrap`
— clone it anywhere, remove it with `rm -rf`, it recreates itself.

**Cut-over state (2026-08-30):** the pre-monorepo checkouts `$COSMIX`, `$COSMIX`,
`$COSMIX`, `$COSMIX` still exist, frozen at the import commit, and most
`_bin/` deploy scripts still hardcode them. Until each script is moved to
`$COSMIX`, a script that names a dot-dir is building the OLD tree — say so
before trusting what it deploys. The GitHub repos `markc/bus|mix|cos` are
frozen (archive them once the deploy scripts have moved).

- **Build/test:** `cd $COSMIX/src && cargo build --workspace --release` /
  `cargo test --workspace`; or `mix $COSMIX/setup.mix` (build + install to
  `$COSMIX/bin`). Legacy: `mix $CMCTL/_bin/build-headless` still targets the
  dot-dirs until moved.
- **Mesh deploy:** `$CMCTL/_bin/deploy_*.mix` + `provision_*.mix` — ship
  mesh-private artifacts with hardcoded per-host `$ADDR` tables. These stay in
  cmctl permanently; the public repo's `_bin/` stays empty.

**Mesh-private data lives ONLY here.** Anything naming the real mesh (node names
your real node names, WG IPs, your domains, live `dnsd/zones.mix`, real certs,
`$ADDR` tables) must never enter a public repo — git history is forever; a tip-only sanitize can't scrub blobs. **Never
hardwire real mesh values into application code, test fixtures, or
doc-comments** — production code reads roster/IPs ephemerally from the signed
inventory cache (`/var/lib/cosmix/noded/inventory.signed`) + the SPEC-13
namespace. Sanitize to RFC 5737 IPs + `example.*` domains + alpha/beta/gamma
node names.

**This is enforced, not remembered** (ADR
`_decisions/2026-07-25-no-hardwired-mesh-values.md`). `mix
_bin/check-public-hygiene.mix --all` scans every repo listed in
`_etc/public-hygiene.conf.mix` (the monorepo at `$COSMIX` plus the frozen
dot-dir checkouts); `pre-commit` and `pre-push` hooks in each run it automatically
(`_bin/install-hygiene-hooks.mix`). The gate also refuses **operational docs
in public trees**: any `*.md` under a `_*/` directory is a violation except
the live manual at `docs/_man/` — `_doc/`/`_plan/`-style content belongs in
this repo, never a public one
(`_decisions/2026-07-28-no-operational-docs-in-public-repos.md`). Exceptions are content fingerprints in
`_etc/public-hygiene.allow.conf.mix` — never path globs, because a glob exempts
future unknown content, which is exactly how the 2026-05-29 sanitization
regressed five days later and stayed broken seven weeks. If the gate blocks
you, sanitize; do not widen the allowlist. Changed the gate? Run
`mix _bin/test-public-hygiene.mix`. Installing the hooks needs **mix ≥ 0.46.0**
(scanning alone needs 0.42.0) and says so at runtime (exit 2, refusing to scan
or install) — below 0.39.0 `exists()` silently discards the lstat option it
depends on; below 0.41.0 there is no `uid()`, so the installer cannot tell
whether the hooks directory's ancestors are owned by someone else; below 0.42.0
there is no `mkdir({parents: false})`, so the interpreter probe cannot decline
to build a missing `TMPDIR`; below 0.45.0 there is no `access()`, so the
installer cannot ask the kernel whether git will actually be able to run the
hook it just wrote; and below 0.46.0 that `access()` still went through glibc's
`faccessat` wrapper, which the man page says emulates the check from `fstatat`
and therefore "does not take ACLs into account" — 0.46.0 issues `faccessat2(2)`
directly and raises on a kernel too old to have it, so the answer is the
kernel's or there is none. That whole line is not a nicety: the mode-bit
arithmetic it replaced was found wrong in four consecutive review rounds, every
time in the direction that reports a silently-skipped hook as a live gate, and
a named-user ACL defeated all four. It refuses rather than write a gate into a
path it could not verify. The probe was run as root on three nodes to prove the
root arm is not workstation-specific; a node that has missed a
`mix _bin/deploy_mix.mix` will refuse rather than install a gate it cannot
verify. (0.46.1 fixed `bshl`, which
range-checked its own *wrapped* result and so answered `0` to shifts that left
64 bits — the installer only uses `band`, so the floor stays 0.46.0, but no node
should be left on the release that fabricated arithmetic.)

Two shapes of repository are **refused (exit 2), not scanned**: one containing a
**gitlink** (a submodule points at content this gate never reads — add the
submodule as its own config entry), and one whose `.gitattributes` puts a
**content filter** on a scanned path (the stored object is not the published
bytes). Both are "cannot answer", not "found a leak"; neither exists in
amp/cos/mix today, so the guards are latent until one does.

And know the gate's limit: it is a **local hook**, so `--no-verify`, another
clone, or a CI runner bypasses it entirely — the rule above is still the rule;
the gate only guards this machine.

**Architecture defaults** (rationale in `_decisions/`): new library crates
are *core-first* — pure logic, mesh-free testable, ABP/mesh integration behind
one `cosmix` feature (`2026-04-27-core-and-citizen-crate-pattern.md`). New daemon features
are *substrate-first* — state in SPEC-12 namespaces via the uniform
`*.props.*` surface plus thin ABP verbs; orchestration is L2 Mix from outside
(`2026-05-20-substrate-first-service-pattern.md`).

## Mix — the shell, the glue, the control surface. Never Python.

Mix (`/opt/cosmix/bin/mix`) is the default for **every** script, hook,
automation, or one-shot, local or remote. The point is to stress-test Mix and
fix its corners in the binary — every script written in something else is a
missed test of the substrate.

- **NEVER Python** — no exceptions. Bash is last-resort only, and even then the
  gap gets a `feedback_mix_*` memory and a binary fix where feasible.
- **Mix is self-documenting — investigate, don't guess.** It has near-zero
  training-data presence; the binary is the oracle:
  - `mix man overview` + `mix man syntax` first (mental model, newline rule,
    shell-vs-Mix classifier); `mix man TOPIC` for the rest.
  - `mix builtins` (`--json` machine-readable; `mix builtins <name>` for one).
  - `mix -c '<probe>'` — live behaviour beats any doc.
  - `mix lint FILE` before running anything non-trivial (`--json`,
    `--deny-warnings`); `run_argv`/`ssh_exec` for operational code;
    `validate()` at job/API boundaries.
- **Fix it in the binary + save a `feedback_mix_*` memory — never paper over it
  with bash/sed/awk.** File routing (all under `$COSMIX/src/crates/`): syntax →
  `cosmix-lib-mix/src/{lexer,parser,evaluator}.rs`; builtins → `builtins.rs`;
  REPL/shell → `cosmix-mix/src/`. Any behaviour change updates the matching
  `$COSMIX/docs/mix/` page in the same commit (that directory IS the manual —
  `mix man` reads it locally and cosmix.dev/mix/ serves it).
- **Two install tiers, amended 2026-08-30 (Mark):** the **system** install is
  `/opt/cosmix/bin/` — what the mesh nodes, the systemd units and every
  `ssh node '…'` assume; if `mix` is missing there on a mesh host, holler and
  stop, never silently fall back. The **dev/user** install is `$COSMIX/bin/`
  (what `bootstrap`/`setup.mix` produce, no sudo). Scripts resolve `mix` via
  PATH (`. $COSMIX/env`) or an explicit path — never assume one tier from the
  other. `setup.mix --system` copies binaries to `/opt/cosmix/bin`.
- **Mix is root's login shell on the mesh nodes**, so an `ssh node '<cmd>'`
  string is parsed by Mix. The tested works/fails table is in the global
  `~/.claude/CLAUDE.md` — not repeated here. What is cmctl-specific is the
  damage: `deploy_dnsd`, `deploy_alloy` and `deploy_loki` each mixed a working
  construct with a failing one in a single line and were silently broken for
  months (`_journal/2026-08-19-cmm-removed.md` plus the 2026-08-19 deploy
  fixes). Rough edges in the ssh path are Mix bugs to fix, not detour around.
- Fleet deploy: `mix _bin/deploy_mix.mix [nodes…]` — every node in your
  inventory (mesh + foreign hosts). **This file does not state which version is deployed.** Component
  versions live in the generated `$COSMIX/docs/VERSIONS.md`
  (<https://cosmix.dev/VERSIONS>); what a given node is *running* is answered by
  the node, never by a document:
  `ssh <node> 'print(run("/opt/cosmix/bin/mix --version"))'`. The deploy REFUSES
  a downgrade, comparing the node's running version before it transfers
  anything — added after a stale build checkout let the nightly autodeploy push
  an older release over a newer one on seven mesh nodes (2026-08-19). Even so, this line has been stale by
  fifteen releases before (it said 0.33.0 for four weeks):
  `/opt/cosmix/bin/mix --version` on the node you care about is authoritative;
  ask it rather than trusting this sentence. Note the deploy script gates the
  **mesh** path only (version assertion + `1+1` smoke per node); the foreign
  path just scps, so a green "deploy complete" says nothing about a foreign
  node's version until you probe it yourself. The rule that a deploy gate must
  assert the new artifact applies to the deployer, not just the deployed.

Your mesh-private addendum (roster, ssh aliases, node gotchas) belongs in a
file of your own under `_doc/` and can be `@`-imported here — it is the one
part of this mandate that cannot ship in the public starter. When it conflicts
with the binary, the binary wins (fix the sheet).

## Do not touch cosmix-foreman

**Mark, 2026-08-30:** `cosmix-foreman` (the `$COSMIX/src/crates/cosmix-foreman` crate, its binary, units,
scripts, ledger, worktrees) is decommissioned until Mark says otherwise. Do
not build, run, install, extend, or file work against it, and do not treat
anything under `_archive/foreman-2026-08/` as live. The crate stays in the
workspace only so the tree builds; if one of its tests ever reds an
unrelated gate, exclude `crates/cosmix-foreman` in `$COSMIX/src/Cargo.toml`
rather than fixing it. Builds and gates run on cbc2/cbc3
(`_plan/2026-08-30-cbc-build-cluster-plan.md`).

## Backends are event-driven (ABP wake). NEVER poll.

**🚨 A cosmix backend is woken by an event, not a clock. Seconds-scale polling of
anything is banned.** The primary trigger for any queue-drain/job worker is a
delegated ABP `*.wake` verb fired by the enqueuer (webd) the instant a job lands;
a systemd timer is allowed **only** as a ≥ 5-min *backstop* for a missed wake —
**never** the mechanism, **never** sub-minute.

- If you're about to write `OnUnitInactiveSec=` in single-digit seconds, or a
  `while true` + `sleep(N)` with N < 60, **STOP** — you're rebuilding the sshm
  mistake (a 5 s queue poll, killed 2026-07-20 after it flooded a stressed box).
- **Reference impls to copy:** `provisiond` and `toolsd` — each has a live
  `cosmix-*-wake.service` citizen (verb `provisiond.wake` / `toolsd.wake`) with
  the timer demoted to a lazy backstop. sshm was the sole offender (no wake).
- **Same law as "fix the binary, never a workaround":** if doing it right needs a
  capability the backend lacks (new delegated verb, polkit scope, webd
  accelerator, ABP citizen), **add it to the cosmix binary/service** — a missing
  wake path is a cosmix *bug*, not a licence to poll.
- Full rule, checklist, and the sshm rearchitecture TODO:
  `_decisions/2026-07-20-no-poll-event-driven-amp-wake.md`.

## Working method — ultracode workflows

Non-trivial work is orchestrated as **ultracode workflows**: the frontier
Claude model authors a phased workflow script; codex wrapper stages do the
bulk work; Claude-family stages verify against the live harness; adversarial
review is a workflow phase that **converges** — cold review → fix by severity
→ re-review ("fully fixed / partially fixed / unaddressed") until no issues
remain or every finding is dispositioned. The re-review is load-bearing: no
partial fixes ship. Workflows are code, so they end — that termination is
both the token-efficiency and the quality story.

**Both engines ride the latest and best available model, whatever it is at
the time** — role names in docs ("frontier Claude", "codex"), dated
"currently X" annotations only (2026-07-17: fable‑5 and `gpt-5.6-sol`; codex's
actual model is whatever `~/.codex/config.toml` says). The method, mechanics,
convergence rules, and per-stage engine/effort table load from:

@_doc/2026-07-17-ultracode-startup.md

**Headless one-shot invocations** — the three engines' non-interactive routes:

| Engine | Headless command |
|---|---|
| Claude | `claude -p "<prompt>"` |
| Codex | `codex exec "<prompt>"` |
| ZCode | `zcode --cwd <repo> --prompt "<prompt>"` (or `zcode -p "<prompt>"`) |

The `zcode` wrapper (`~/.local/bin/zcode` → `_bin/zcode.mix`) is a Mix script
that sets `ELECTRON_RUN_AS_NODE=1`, forwards argv to `/opt/ZCode/zcode
/opt/ZCode/resources/glm/zcode.cjs`, and adds the one flag the CLI itself
lacks: **`--model <ID|PROVIDER/ID>`** for a one-run override (`zcode --model
GLM-5-Turbo -p "…"`), implemented via `ZCODE_MODEL` + `ZCODE_BASE_URL` +
`ANTHROPIC_API_KEY` because ZCode exposes no model flag and `ZCODE_MODEL` alone
loses the provider's API key. Without `--model` argv is forwarded untouched.
It needs **mix ≥ 0.51.0**: the child's environment is handed over through
`run_stream`'s `{env}` option, added 2026-08-15 for exactly this — the first
cut `env`-prefixed the argv, which put the API key in the child's `ps` argv.
An older mix silently IGNORES the option map, so the `--model` path proves the
option took effect before trusting it (a version string would not).
For ZCode-as-cold-verifier arms, default to `--mode plan` (read-only opinion,
no edits); for long fan-out runs wrap with an explicit `timeout` — unlike the
Codex MCP's ~30-min idle-abort, a headless `--prompt` run has no built-in cap.

**`zcode --help` lies** (0.16.3): it advertises `--settings`, `--max-turns` and
`--allowed-tools`, but the CLI's Node `parseArgs` table accepts none of them and
**rejects the entire run** with "Unknown option" — including the `--max-turns`
this file used to recommend. What actually parses: `--prompt`/`-p`, `--cwd`,
`--json`, `--mode`, `--attach`, `--target`, `--target-replace`, `--resume`,
`-c`, `--locale`, `--no-color`, `--no-browser`, `--browser-use`, `--verbose`,
`--force-mcs`, `--force`, plus `--disallowed-tools` via a pre-parser. Read from
the bundle's parser table 2026-08-14; probe before trusting `--help`.

**ZCode here is headless-only** (probed 2026-08-19): the desktop bundle ships
the CLI but not its terminal UI — the interactive path lazily
`import("@zcode/tui")` and no `node_modules/@zcode/tui` exists anywhere on
`zcode.cjs`'s resolution chain, in 3.7.6 or 3.7.7. That package is published
nowhere (not npm, not in `app.asar`, no separate CLI download from z.ai); it
only resolves inside upstream's single-executable build, which extracts it from
its own `zcode-tui-runtime/`. So a bare `zcode` used to die with a raw Node
"Cannot find package '@zcode/tui'"; the wrapper now refuses that invocation
with the real reason and points at `-p` or the desktop GUI. `--version`/`--help`
still pass through. Nothing about the headless arm is affected.

The GLM model is pinned in two places, both on **GLM-5.3** as of 2026-08-14:
`~/.zcode/cli/config.json` (`model.main` — the headless route) and
`~/.zcode/v2/config.json` (`builtin:zai-coding-plan`, priority 99 — the GUI;
lower number wins). z.ai silently serves a retired id from its successor — a
request naming `glm-5.2` is answered by `glm-5.3`, as `glm-4.5-air` is by
`glm-4.7` — so the pin buys config honesty, not different routing. Declared
limits are **measured, not copied**: GLM-5.3 is 1,048,576 context / 131,072
output (probed live — 1,048,547 accepted, 1,049,656 refused). A model absent
from the config silently falls back to ZCode's own guess, so declare it.
`_bin/glm-usage.mix` reports plan quota and per-model usage.

Full reasoning + sources: `_doc/2026-07-17-ultracode-workflows-driving-codex.md`.

**Standing commit authorization:** once a change is implemented, verified
(build/test gates for code; review phase for anything substantive), commit AND
push immediately without asking — git history is the undo. Escalate to Mark
only for irreversible actions (`push --force`, `reset --hard`, deletions),
out-of-scope discoveries, and direction-setting calls (trust/auth/identity,
schema or wire migrations, public API/ABP-verb shape, new daemon boundaries,
"what should Cosmix become"). Every escalation carries a recommendation,
plain-English stakes, reversibility, and a decision hook — Mark's scarce input
is the values call, not decoding options.

## Versioning discipline (session-end bump gate)

Per touched component at wrap-up: *would a consumer — another component, an
operator, a script — behave differently or need to know, given the net change
since the last bump?* Yes → bump; can't name the observable difference → don't,
and say so ("no bump — docs only"). Triggers: builtin/verb/flag/API changes,
observable behaviour, wire/schema/config formats, error semantics. Doesn't:
docs, tests, refactors, formatting. Pre-1.0: observable-compatible → PATCH,
breaking → MINOR. Bump each component on its own merits; any persisting deploy
carries a bumped version first. A truthful version is the cheapest legibility
signal a fleet of agents has.

## Documentation

`_doc/` = docs, `_journal/` = operational logs, `_plan/` = date-prefixed plans
(deleted once shipped unless cited), `_spec/` = specification suite
(`_spec/README.md` entry point). Filenames `YYYY-MM-DD-lower-case-title.md`.
Repo root stays clean: CLAUDE/CODEX/README/LICENSE + underscore dirs only.
Every session leaves docs a fresh agent can resume from: decisions with
reasoning, journals capturing what was rejected and why, agent-operable
build/test/run procedures. Keep `CODEX.md`'s crate map current — stale crate
maps are the most expensive doc rot. Mark values genuine technical disagreement
over validation; pressure-test honestly.

---

*Original mandate drafted 2026-04-25; rewritten 2026-07-17 (Mark-directed) to
replace the dual-reviewer loop with the ultracode-workflow methodology —
archive: `_doc/2026-07-17-CLAUDE.md`.*

## Graduated Skills (auto-generated)

Maintained by cosmix-mcp's `skills_graduate`: proven skills for THIS hub's
domain are promoted here as permanent rules. The loop only appends below this
marker; the human-authored mandate above always wins on conflict. (The starter
ships this section empty — the graduated skills of the original hub are its
operator's, not yours.)
