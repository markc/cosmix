# Agent Notes

Read `CLAUDE.md` as complementary guidance before substantive changes. This
repository is **public**: never reproduce private mesh values (real host
names, addresses, domains, keys, operator home paths) anywhere in it.

## Repository role

`markc/cosmix` is the whole project in one tree, rooted at `$COSMIX`
(default `~/Projects/cosmix`):

- `src/` — one Cargo workspace, every crate flat under `src/crates/`:
  the Bus family (`cosmix-lib-bus`, `-client`, `-buildinfo`, `-log`,
  `-props-core`), Mix (`cosmix-lib-mix`, `cosmix-mix`, `mix-bench`), and the
  substrate libraries + daemons. Dependency direction is bus ← mix ← cos and
  cargo enforces it (no cycles).
- `src/desktop/` — the desktop, a separate workspace with its own toolchain
  and a `[patch.crates-io]` section; build with `--manifest-path` or
  `setup.mix --desktop`.
- `docs/` — the cosmix.dev Pages site. `docs/mix/*.md` and `docs/cos/*.md`
  are the manuals' *source*; `mix man` reads `$COSMIX/docs/mix` locally.
  `docs/dev/{bus,mix,cos}/` holds each former repository's README /
  CLAUDE.md / AGENTS.md, rewritten to monorepo paths — read the one for the
  area you are changing.
- `bootstrap` (sh) + `setup.mix` (Mix) — the install. `bootstrap` is the only
  non-Mix script and exists solely because `mix` does not yet.

## Build and verify

```sh
cd $COSMIX/src && cargo build --workspace --release     # or: mix $COSMIX/setup.mix
cd $COSMIX/src/desktop && cargo build --workspace --release
#   the gate also covers feature-gated binaries when present: build them with
#   the extra commands in src/tools/version_flag_gate.mix's Usage header
cd $COSMIX/src && mix tools/version_flag_gate.mix   # after both release builds: every binary answers --version
cd $COSMIX/src && mix tools/version_flag_gate_selftest.mix   # no build needed: proves each gate FAIL branch still fires
cd $COSMIX/src && cargo test --workspace                # core workspace; desktop is EXCLUDED (src/Cargo.toml)
cd $COSMIX/src/desktop && cargo test --workspace --no-fail-fast   # ctk, quoin, comp, term: its own workspace
cd $COSMIX/src/desktop && cargo test -p ctk --lib --features bus,theme app_control::
#   must report "N passed" with N > 0; "0 passed" means the bus feature was
#   compiled out and ctk's authorization tests did not run
cd $COSMIX/src && cargo clippy --workspace --all-targets -- -D warnings
cd $COSMIX/src/desktop && cargo clippy --workspace --all-targets -- -D warnings
cargo fmt -p <crate>          # never a repo-wide fmt from a task
```

On a machine without a real GPU the desktop run fails exactly two llvmpipe
pixel tests in `cosmix-comp` (`client_surface_material::…_on_a_real_gpu` and
`capture::…kms_overlay_equivalence_…`); any other failure blocks. The
`app_control::` filter exists because a bare `ctk` run still reports ~267
passed without the `bus` feature — `app_control` is `#[cfg(feature = "bus")]`,
so only the filtered count drops to 0 when the feature is compiled out.

`src/rust-toolchain.toml` pins the compiler; rustup honours it.

## Paths

Everything derives from `$COSMIX`. The rule lives in
`src/crates/cosmix-lib-config/src/paths.rs` (daemons) and, verbatim, in
`src/crates/cosmix-mix/src/cosmix_paths.rs` (mix) — keep them in step. Root
from the `COSMIX` env var, else self-located from the running binary (an
ancestor holding `bootstrap` + `src/Cargo.toml`), else `~/Projects/cosmix`;
`COSMIX_SRC/ETC/VAR/BIN/RUN/LOG/TMP` override single directories; a system
install at `/opt/cosmix/bin` with no checkout above it keeps FHS/XDG
defaults. Never hardcode an install path.

## Conventions

- Scripts are Mix. No Python; sh only for `bootstrap`.
- Docs for a behaviour change go in the same commit, in `docs/`.
- Desktop furniture belongs to compositor-hosted Quoin. Apps use conventional
  CTK menus and purpose-specific controls with compositor-managed window chrome;
  do not add Quoin-like panel furniture to individual apps. See
  `src/desktop/APPS.md` for the layout policy and legacy migration scope.
- Version-bump a crate when a consumer would observe the change.
- Every binary answers `--version` and `-V` and exits 0 before any other side
  effect: no async runtime, config read, logging, display check, Bus connect,
  fd quarantine or window, with or without a display or broker. The FIRST
  stdout line is `<crate> <semver> (<sha12>[-dirty][, features: …], built
  <rfc3339>)`; any further lines are `key: value` (cosmix-comp adds `commit`,
  `features`, `profile`). `--version --json` prints one JSON object with at
  least `component`, `version`, `git_sha`, `build_time` and `git_dirty`,
  agreeing with the line. The one naming exception is mix, which answers as
  `mix`, its binary name, not its crate `cosmix-mix`. `main`'s first statement
  is `cosmix_buildinfo::exit_on_version!();` and it precedes runtime
  construction, so no `#[tokio::main]` on a binary: build the runtime
  explicitly after the macro. The default scope is the whole argv up to `--`,
  so a value literally spelled `-V` or `--version` must follow `--` (clap's
  `--flag=-V` form also works); binaries whose options take free strings
  with no `--` escape use `exit_on_version!(leading)`, which reads argv[1]
  for the flag and argv[2] for `--json`. The crate's `build.rs` calls `cosmix_buildinfo::emit()` so the sha
  is real. A new binary is covered by `src/tools/version_flag_gate.mix`
  automatically; it fails until the binary conforms.
- Public-safe architecture specifications belong in `docs/spec/`. Read their
  status and evidence labels: draft publication is not normative acceptance.
  Chapter ordering does not reassign legacy runtime specification IDs.
- Operational docs (`_doc/`, `_plan/`, journals, private specs and decisions)
  remain in the maintainer's private control repo. Never copy them wholesale
  into the public suite. Update accepted public contracts with affected code.

## History

Merged 2026-08-30 from `markc/bus`, `markc/mix`, `markc/cos` (git subtree —
full history preserved) and the former docs-only `markc/cosmix`. Those three
repositories are frozen at the merge commit.
