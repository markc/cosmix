# Codex Role

Since the 2026-07-17 mandate rewrite (`CLAUDE.md` § "Working method — ultracode
workflows"; the retired dual-reviewer mandate is archived at
`_doc/2026-07-17-CLAUDE.md`), Codex — always the best available model per
`~/.codex/config.toml` (2026-07-17: `gpt-5.6-sol`, high reasoning) — is the
**bulk worker and cold-eyes stage inside ultracode workflows**, plus an inline
consultant in the main session:

- **Workflow stages** — implementation, transforms, sweeps, and cold
  diff-review phases run as `codex exec` wrapper stages authored into the
  workflow script by Claude. Stage prompts are self-contained (paths, what
  changed, what's already verified); results return through typed schemas.
  Mechanics: `_doc/2026-07-17-ultracode-startup.md`.
- **Inline consultant** — design consults and follow-up rounds via the codex
  MCP in the main session, one thread per feature arc.
- **MCP vs `codex exec` — same model; pick on context handling, not quality.**
  The MCP thread stays **warm**: `codex-reply` continues the same context, so
  review/fix rounds don't re-ingest the code — the efficient route for
  iterative work on **one arc**. `codex exec` is a **cold one-shot** that
  re-pays context each call, but is the **only** route inside a workflow/agent
  and the more robust choice for **fan-out** (`isolation: 'worktree'`), bulk,
  and any run past ~30 min (background + poll dodges the MCP idle-abort — which
  aborts *reporting* but not the work, so verify the tree, never discard).
  **Rule:** inline single-arc multi-round → MCP; fan-out / bulk / in-workflow /
  >30-min → `codex exec` backgrounded.
- **ZCode has the same shape of headless route**, for when ZCode is the
  fan-out/verifier seat instead of (or alongside) Codex:
  `zcode --cwd <repo> --prompt "<task>"` (wrapper at `~/.local/bin/zcode`;
  the analogue of `claude -p` / `codex exec`). Default to `--mode plan`
  (read-only) for verifier passes; `--max-turns`, `--no-color`, `--json`,
  `--disallowed-tools` for scripted control. See `CLAUDE.md` § "Working
  method" for the full three-engine table.
- When Codex itself is driven directly in this checkout (Codex CLI), it may
  implement when asked; see `AGENTS.md` for its front-door guidance.

Default behavior when operating here:
- Read `CLAUDE.md` for project frame, then this document for the working map.
- Do not modify `CLAUDE.md` or memory files unless explicitly asked.
- Keep patches narrow; do not overwrite or revert uncommitted changes.
- **GitHub publishing preference:** no pull requests by default. When Mark
  asks to commit and push, commit directly on `main` and push `origin main`;
  a branch/PR only when Mark requests one or protection forces it.
- Surface disagreements as recommendations, not unilateral rewrites.

# Review-stage prompts

When a workflow review phase (or an inline consult) points Codex at a diff,
these standing prompts have proven high-leverage:

> Focus on cross-file consequence tracing, silent regressions, storage-layer
> barriers, and whether previous fixes fully resolve the finding rather than
> merely appearing to. For each suggested change, also surface: **what
> assumption is this change relying on that is not enforced anywhere?**

For a follow-up pass over fixes:

> Did this commit resolve your previous findings, or only partially address
> them? For each prior finding: fully fixed / partially fixed (what residual
> remains) / unaddressed.

The follow-up prompt is load-bearing — it refuses partial fixes. Iterate
review → fix → re-review until "no issues found" or every finding is
dispositioned; nothing substantive ships on a single round.

Route to Codex stages what it catches well: silent no-ops on partial
refactors, security hygiene, cross-crate consequence chains, partial fixes
sold as full. Keep with Claude + Mark what it doesn't: architectural shape,
substrate-mandate alignment, failure domains, trust boundaries,
state-of-truth. Codex stages ship things *correctly*; they don't select
*what* to ship.

# Project Summary

Cosmix is an AI-first, self-hosted computing substrate written in Rust. The
working tree is now a **public source/docs constellation plus a private control
overlay**:

- three public sibling Cargo workspaces (`$COSMIX`, `$COSMIX`, `$COSMIX`) hold the
  Rust source;
- public `$COSMIX` is the sanitized upstream docs/control template new users
  can clone, study, and patch without inheriting Mark's private mesh state;
- private `$CMCTL` (this checkout) is Mark's local control overlay and the
  **hub that manages/drives all four sibling folders** — `$COSMIX`, `$COSMIX`,
  `$COSMIX`, and `$COSMIX`: deploy scripts, journals, operational state, and
  mesh-private material that must not leak upstream. Sessions here have
  authority to `cd` into any sibling to read/build source and publish sanitized
  changes to `$COSMIX`.

One-way dependency order for code remains **bus ← mix ← cos** (and bus ← cos
directly); `cosmix` and `cmctl` depend on none and orchestrate the public source
repos. Its core thesis is that the system should be legible, modifiable, and
reconstructible by agents. The current expression is a Linux-oriented sovereign
stack: local daemons, a message hub, a WireGuard mesh, a Mix shell and scripting
language, mail services, storage services, LLM/agent services, and an ABP-driven
display layer.

The project is not primarily a conventional desktop environment. The desktop,
display renderer, Mix language, mesh, and mail stack are surfaces of the same
substrate objective: make every component observable and scriptable through
structured protocols so an AI agent can inspect state, route commands, modify
configuration, and help rebuild the system.

# Architectural Shape

**Current split (post-2026-05-29 extraction, later public/private control
split).** The Rust source dissolved out of the original private hub (step 7a,
`7ea78ee`) into three public sibling workspaces; each public workspace root is
`~/.<repo>/src/Cargo.toml`. The original private control/docs material was then
split again: `$COSMIX` is now the sanitized public docs/control upstream, while
`$CMCTL` is the private local control checkout. `cmctl` may retain operational
state, private deploy scripts, real mesh identifiers, and journals; `cosmix`
must remain suitable for public users to clone and adapt. One-way dependency
order — **bus ← mix ← cos** (and bus ← cos directly); `cosmix` and `cmctl`
depend on none.

| Repo | Path | Visibility | Role |
|---|---|---|---|
| **bus** | `$COSMIX/` | public · markc/bus | CosMix Agent Bus protocol family — `cosmix-lib-bus` + `cosmix-lib-client` + `cosmix-lib-props-core` + `cosmix-lib-buildinfo` + `cosmix-lib-log` (5 crates). Depends on nothing. |
| **mix** | `$COSMIX/` | public · markc/mix | Mix language — `cosmix-lib-mix` + `cosmix-mix` + `mix-bench` (3 crates). Depends on bus. |
| **cos** | `$COSMIX/` | public · markc/cos | Substrate libraries + daemon family (38 crates in `src/crates` as of 2026-07-25, plus the separate `desktop/` workspace: ctk + apps). Depends on bus + mix. |
| **cosmix** | `$COSMIX/docs/` | public · markc/cosmix | Sanitized docs/control upstream: project docs, specs, public control examples, and reusable flow. No Rust source; no private mesh state. |
| **cmctl** | `$CMCTL/` | private · markc/cmctl | Mark's private control overlay cloned/split from the old private cosmix hub: deploy scripts, journals, private operational state, and local orchestration. No Rust source; **manages/drives all four sibling folders** — `$COSMIX`, `$COSMIX`, `$COSMIX` (source) + `$COSMIX` (docs upstream) — locally. |

The public source repos were sanitized same-day (real WG IPs, mesh node names,
owned domains, live zone file, real LE certs stripped -> RFC 5737 IPs +
`example.*` domains + alpha/beta/gamma/delta/epsilon node names) and **deleted +
re-init'd on GitHub** with a single fresh `init` commit apiece.

⚠️ **That clean slate did not hold.** Mesh-private identity re-entered `cos` on
2026-06-03 (`e329683`) — five days after the re-init — and sat in the published
history for seven weeks, spreading to 19 files (104 matches: six real WG
addresses, real domains, node names, the operator's home path). Every 2026-07-25
tip is sanitized and pushed (cos `7682a83`, mix `55429a6`, amp `afdeb26`), but
**the history still contains the leaked values and must be treated as
disclosed.** The recurrence is why the rule is now an executable gate rather
than prose — see `_decisions/2026-07-25-no-hardwired-mesh-values.md`,
`_bin/check-public-hygiene.mix`, and the `pre-commit`/`pre-push` hooks installed
in all three repos. History rewrite (459 of 478 cos commits + `push --force`)
remains an open operator decision.
The later `cosmix` publicization applies the same rule to docs/control material:
public `$COSMIX` gets reusable docs and sanitized control paths; private
`$CMCTL` keeps the local secrets and per-site operational state. New users are
expected to clone public `cosmix`, then create or copy their own private control
checkout for development; code patches go to `bus` / `mix` / `cos`, doc/control
patches suitable for everyone go upstream to public `cosmix`, and private
site-specific state stays out of upstream. The "Workspace Map" + crate tables
below describe the LOGICAL crate set across the public source repos, not a
single in-tree workspace. Current crate destinations are enumerated in the
Workspace Map below and the live `$COSMIX/src/Cargo.toml`,
`$COSMIX/src/Cargo.toml`, and `$COSMIX/src/Cargo.toml` manifests.

- `cosmix-noded` is the consolidated node daemon: hub, config, monitor, and
  logger. Most local services communicate through it.
- ABP (Agent Bus Protocol) is the main application-facing wire format: markdown
  frontmatter headers plus an optional body. It is designed to be readable by
  machines, humans, and agents.
- Mix is the ARexx-inspired shell and scripting language. It has native
  concepts for ABP messaging and is both a standalone interpreter and a Cosmix
  citizen.
- The mesh layer uses WireGuard plus ABP/WebSocket routing so services can be
  addressed across nodes.
- The `ui.*` ABP display lane (domain-blind renderers painting windows from
  a wire vocabulary) was a 6-month experiment retired by the 2026-07-18
  ABP-control-plane pivot (ABP controls apps, it does not paint pixels) and
  ARCHIVED 2026-07-20 — `cosmix-lib-display`, `cosmix-disp-skia`,
  `cosmix-mixer-bench`, `cosmix-benchd`/`-bench-trace`, and the `cosmix-mail`
  reference app now live only under cos git tag `amp-display-archive`
  (private Mix companions under cmctl tag `amp-display-archive`). The
  `display-crates` workspace metadata is now empty. The forward GUI path is
  native bevy apps under `desktop/apps/` (`studio`, `filemgr`) sharing the
  `desktop/ctk` toolkit (one shared workspace since 2026-07-24; CosMix Desktop).

# Workspace Map

> **Repo legend (post-2026-05-29).** Crates live in
> **`$COSMIX/src/crates/`** *except* the carved-out sibling sets:
> **bus** (`$COSMIX/src/crates/`) holds `cosmix-lib-bus`,
> `cosmix-lib-client`, `cosmix-lib-props-core`, `cosmix-lib-buildinfo`,
> `cosmix-lib-log`; **mix**
> (`$COSMIX/src/crates/`) holds `cosmix-lib-mix`, `cosmix-mix`,
> `mix-bench`. The `(bus)` / `(mix)` tags below mark these; everything
> untagged is cos — new daemons/libs land there by default, so the rule
> stays true without a count to maintain.

Core protocol and substrate libraries:
- `cosmix-lib-bus` (bus) — ABP parsing, serialization, and message types.
- `cosmix-lib-client` (bus) — WebSocket hub client.
- `cosmix-lib-props-core` (bus) — SPEC 07 read-side property types
  (`PropPath`, `PropValue`, `PropTree`, `PropDescribe`, `redact`, `diff`)
  plus the ABP wire dispatcher. Split out of the former `cosmix-lib-props`
  on 2026-05-29 (lib-props split).
- `cosmix-lib-props-store` — SPEC 12 storage/mutation/audit surface: sqlite +
  memory backends, audit HMAC, `NamespaceSpec` / lifecycle machinery. The
  substrate half of the same 2026-05-29 split (paired with the SPEC 07 read
  crate `cosmix-lib-props-core`).
- `cosmix-lib-config` — typed settings and path conventions.
- `cosmix-lib-daemon` — daemon startup, logging, and shutdown scaffolding.
- `cosmix-lib-log` (bus) — unified logging surface (CLI flags, sinks,
  EnvFilter, hot-reload handle) for every Cosmix binary; the pure-core
  half, lives in bus. Its cos-side SPEC-12 `<svc>.log` namespace +
  live-reload watcher is the separate `cosmix-lib-log-props` (below).
- `cosmix-lib-buildinfo` (bus) — compile-time build provenance
  (version / git_sha / git_dirty / build_time) baked into every cosmix
  daemon for the `--version` / ABP INFO triple.
- `cosmix-lib-log-props` — cos-side SPEC-12 `<svc>.log` namespace plus the
  live-reload watcher over `cosmix-lib-log` (the log analogue of the
  props-core/props-store core+extension split above).
- `cosmix-lib-mesh` — WireGuard mesh configuration and peer sync.
- `cosmix-lib-wg` — pure-logic WireGuard primitives shared with the
  `cosmix-wgd` daemon: key material, interface/peer value types, config
  rendering, IPAM, `wg show` parsing.
- `cosmix-lib-node-id` — stable 5-hex node identifier derived from
  `/etc/machine-id` (`_plan/2026-05-20-cosmix-wgd.md` §12).
- `cosmix-lib-mesh-trust` — cross-mesh trust + grant verification (per
  `_plan/2026-05-20-cosmix-cross-mesh-authz.md`). Core (`--no-default-features`)
  carries envelope canonicalisation, Ed25519 signature verify, freshness,
  and capability-bag math against passed-in fixtures. The `cosmix` feature
  adds the `with_cross_mesh_grants` `AuthPolicy` combinator, the
  `TrustGrantsCache` substrate subscriber, and `NamespaceSpec` drafts for
  the five wgd namespaces. P0 (C1..C6) shipped 2026-05-21; P1+ is gated on
  wgd-side prerequisites (plan §12).
- `cosmix-lib-dns` — authoritative WG-mesh DNS core: `zones.mix` strict-data
  loader, two-layer owner-aware→flattened zone model, RFC 1982 serial
  arithmetic, per-owner/per-zone replay floors, resolver, hickory-proto
  codec, and tokio serve loops. Core crate (no ABP/mesh/config; no `cosmix`
  feature — the core/citizen split lives on the `cosmix-dnsd` binary).
  P2-C added the additive `serve_*_observed` siblings for the citizen's
  rcode canary; the standalone serve path stays the committed P1 source
  **verbatim** (purely-additive core diff vs P1, dead-stripped from the
  standalone binary). Binary-level identity was abandoned across two
  reframes — literal byte-identity at P2-C (`.rodata` relocates once the
  shared core gains a symbol), then instruction-multiset-identity at P2-D
  (`lto = true` redistributes inlined serve code into `main`'s future on
  benign core growth even with the standalone source held constant). The P2
  source-verbatim acceptance harness was retired at P2 close in `40298aa3`;
  no live gate now compares this source against P1. The surviving executable
  check is `$COSMIX/src/crates/cosmix-dnsd/tests/standalone_prober.rs`; see also
  `feedback_lto_inlining_redistribution`.
- `cosmix-lib-files` — files-as-truth markdown corpus core (backs
  `cosmix-filesd`): surgical frontmatter writer, atomic write, BLAKE3
  hashing, UUIDv7 identity, link extraction, index schema + reconcile diff.
- `cosmix-mesh-sign` — SPEC 13 1b-b: signs the authored `inventory.mix`
  into the `inventory.signed` trust root via the genesis Ed25519 key
  (operator secrets DB), sharing the canonicaliser with
  `cosmix-lib-mesh-trust` and `cosmix-noded`.
- `cosmix-lib-davproto` — CalDAV/CardDAV codecs shared by the `cosmix-maild`
  DAV server face and the future DAV client: JSCalendar↔iCalendar (RFC 8984/
  5545) + JSContact↔vCard 4.0 (RFC 9553/6350) emit + parse, strong
  content-hash ETags. Pure (no ABP/mesh/config). Plan retired 2026-07-23
  (git history); deploy + client-interop residue tracked in
  `_plan/2026-07-23-consolidated-backlog.md`.

Scripting and agent-facing libraries:
- `cosmix-lib-mix` and `cosmix-mix` (mix) — Mix interpreter, REPL, script
  runner, and ABP-enabled shell. The 0.29.0 "correctness floor"
  (2026-07-11) brought structured builtin contracts
  (`mix builtins --json/--data`), `mix lint` (semantic analyzer, stable
  MIX-E1xxx/W2xxx codes), structured errors + tracebacks
  (`catch $msg, $err`, `raise()`), `run_argv`/`run_argv_must`, the
  `validate` family, HTTP `ca_file`/`ca_pem`, and `--strict-arity`.
  Since then (fleet on **0.33.0**, 2026-07-16): `finally` + `ssh_exec` +
  HTTP response v2 (0.30), the `$s = $s .. rhs` in-place fast path
  (0.31.1), the `time` shell modifier + `realpath()` (0.32.x), and
  nested lvalue assignment + MIX-E1501/E1502 lints (0.33.0).
- `mix-bench` (mix) — autoresearch metric harness for the standalone Mix
  arena.
- `cosmix-lib-llm` — multi-backend LLM client.
- `cosmix-lib-agent` and `cosmix-agentd` — agent session loop and daemon.
- `cosmix-lib-skills` — skill learning, retrieval, and refinement.
- `cosmix-claud` and `cosmix-mcp` — Claude CLI / Claude Code integration
  surfaces.

Services:
- `cosmix-noded` — consolidated node daemon: hub, config, monitor, and
  logger. Most local services communicate through it.
- `cosmix-maild` — JMAP, SMTP, and CalDAV/CardDAV mail daemon (the DAV face
  `src/dav/` rides the same axum server + auth, over `cosmix-lib-davproto`).
- `cosmix-maild-auth`, `cosmix-maild-rules`, `cosmix-maild-bayesian` — inbound
  mail authentication, deterministic rules, and Bayesian filtering.
- `cosmix-indexd` — semantic indexing and vector storage.
- `cosmix-mds` — metadata store and content-addressable blob storage.
- `cosmix-filesd` — files-as-truth markdown corpus daemon over
  `cosmix-lib-files`: watches a corpus tree, maintains a rebuildable SQLite
  index, and serves it over ABP (`filesd.*` verbs + props). Also runs an
  fs-mode live-readdir layer over a "places" allowlist backing the webd
  dual-pane file manager (`/files`).
- `cosmix-webd` — Axum-based web/CMS daemon.
- `cosmix-foreman` — **DECOMMISSIONED 2026-08-30, development halted until
  further notice** (Mark: not enough progress for the burn; method is the
  inline loop). Crate stays a cos workspace member at 0.24.10 (tag
  `foreman-halted-2026-08-30`) so the tree builds; nothing runs it. Fleet
  units uninstalled, launcher removed, worktrees + build scratch deleted;
  ledger + unlanded-branch bundle kept at `$CMCTL/.foreman/keep/`. All fleet
  scripts and unit files moved to `_archive/foreman-2026-08/{_bin,_etc}/`
  (index: `_archive/foreman-2026-08/README.md`). What it was: build-orchestration
  harness (binary `foreman`) — agent drivers (claude/codex/GLM), SQLite ledger,
  escalation ladder + dispatch (per-task sibling worktrees), refinery merge
  queue with Claude merge-authority review, governor, policy gate, verifier
  tiers 0–2, MCP pull-work server, mayor.
- `cosmix-wgd` — WireGuard config daemon over `cosmix-lib-wg`: derives
  interface/peer state from the signed mesh inventory (never authors
  props); P2 derive + dry-run landed 2026-07-06 (`_plan/2026-05-20-cosmix-wgd.md`).
- `cosmix-dnsd` — authoritative WG-mesh DNS daemon over `cosmix-lib-dns`.
  P1 is the standalone server only (the spine's goal-(c) isolated-node
  self-resolution); P2 (A/B/C/D, all landed) fills the empty-bodied
  `cosmix` feature to make the default build the mesh citizen, with
  zero feature-polarity change. P2-C added the read-only ABP surface
  (`dnsd` service: `dnsd.zone.snapshot` / `dnsd.stats`); P2-D added
  maillog-style logging (`cosmix_daemon::init_tracing`, crate-target
  `cosmix_dnsd`), the `_etc/systemd/cosmix-dnsd.service` unit
  (User/Group `cosmix-dnsd` = SPEC-10 v1.3.0 UID/GID 506; noded a
  SOFT `Wants=`/`After=`, never `Requires=`/`BindsTo=` — goal-(c)),
  and a bounded WG-up bind-retry in the live `cosmix-dnsd` source. During P2,
  the twice-reframed `--no-default-features` acceptance invariant was
  **"P1 serve path source verbatim + functional prober"**. Its one-shot harness
  checked an additions-only core diff plus four P1-verbatim `serve.rs`
  functions, wrapper-free standalone `main.rs` arms, and the §8 functional
  prober. That harness was retired at P2 close in `40298aa3`; no live gate now
  enforces the P1 source-shape clauses. The remaining executable behaviour
  check is `$COSMIX/src/crates/cosmix-dnsd/tests/standalone_prober.rs`, run with
  `cargo test -p cosmix-dnsd --no-default-features --test standalone_prober`.
  The former cgu=1 instruction-multiset comparison was advisory only and was
  retired with the harness because LTO makes benign deltas expected.

Display — ARCHIVED (ui.* lane, cos tag `amp-display-archive`, 2026-07-20):
- `cosmix-lib-display`, `cosmix-disp-skia`, `cosmix-mixer-bench`,
  `cosmix-benchd`, `cosmix-bench-trace`, `cosmix-mail` — the ui.* ABP display
  experiment (protocol types, Skia CPU renderer, mixer broker, bake-off
  harness, JMAP reference app). Retired by the 2026-07-18 control-plane pivot;
  retrieve with `git checkout amp-display-archive`. The native renderer
  bake-off arms (mixer-egui/-iced/-html/-bevy + mixer-fused) are separately
  archived under cos tag `mixer-bakeoff-arms-archive`.

Desktop spine (LIVE, in `src/crates` — mesh/engine-free cores consumed by the
separate `$COSMIX/src/desktop/` workspace; see `$COSMIX/src/desktop/APPS.md` for ctk + the apps):
- `cosmix-actions` — engine-independent action registry + data-driven keyboard
  resolution; the action spine behind CTK menus. No Bevy, ABP or mesh dep.
- `cosmix-app-identity` — the single identity authority for desktop apps
  (stable slug + branded display name; backs the `dev.cosmix.*` slug spine).

Interaction / ctkd lane (LIVE — the `interact.*` ephemeral-surfaces stack):
- `cosmix-interaction-schema` — the `interact.*` wire contract; headless serde
  DTOs and the single source of truth for the interaction vocabulary, so Mix
  scripts and daemons depend on it without pulling Bevy.
- `cosmix-interaction-broker` — the headless decision core: origin labelling,
  per-origin + aggregate rate limiting, dedupe/coalesce, remote urgency clamp,
  dispatch-registration enforcement.
- `cosmix-interactd` — the headless daemon: registers the ABP `interact`
  service, serves `notify.v1` with queued freedesktop delivery, owned
  update/dismiss, action dispatch, and watchable `interact.props.*` state.
  **Renderless** — it draws nothing.

Mixer schema (LIVE — the one bake-off crate that survives):
- `cosmix-mixer-schema` — the mixer.v1 domain contract, still a live dep of
  `desktop/apps/studio`. Stays in `src/crates`.

MIDI / audio:
- `cosmix-midicomp` — SMF (Standard MIDI File) ⇄ plain-text converter; an
  MIT Rust port of `midicomp` (tolerant SMF reader, `midly`-backed writer).
- `cosmix-musicd` — music/audio daemon: MIDI-file → SoundFont
  (`rustysynth`) render/play, native SFZ→SF2 converter (bit-parity),
  live MIDI-keyboard synth, and the mixer engine behind the bake-off GUI
  (shipped 2026-07-15); ABP citizen.
- `cosmix-song` — the `song.v1` editable document model: tracks/notes on tick
  timing, SMF import/export, JSON + binary persistence, snapshot undo. Pure
  domain crate (no audio, no UI, no cosmix deps), sibling of
  `cosmix-mixer-schema`, feeding the studio sequencer line. Lifted from
  miditui (MIT); design `_doc/2026-07-18-midi-sequencer-design.md`.

# Important Docs

- `CLAUDE.md` is the canonical project mandate and should be read before doing
  substantive work.
- `_spec/README.md` is the entry point for the specification suite.
- The crate map in THIS file plus `_doc/README.md` (doc-tree orientation)
  are the current-architecture truth (the old 00-overview/01-status
  snapshots were retired 2026-07-23 — git history; the
  old architecture-overview and headless-classification decisions were
  retired 2026-07-23 — git history). One surviving discipline from the
  latter: **no mesh-substrate crate may depend on display/GUI code** — GUI
  lives in the separate `$COSMIX/src/desktop` workspace, and any new mesh-side
  feature must build and run headless.
- `_journal/` contains chronological session notes and implementation
  history.

# Working Guidance for Codex

When contributing, optimize for agent legibility and narrow, reviewable
changes. Prefer read-only investigation, reviews, tests, and architecture
pressure-testing unless the user explicitly asks for edits. When edits are
requested, keep them scoped to the relevant crate or document, avoid touching
memory/journal files unless asked, and do not overwrite uncommitted work.
