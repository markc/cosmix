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
cd $COSMIX/src && cargo test --workspace
cd $COSMIX/src && cargo clippy --workspace --all-targets -- -D warnings
cargo fmt -p <crate>          # never a repo-wide fmt from a task
```

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
- Version-bump a crate when a consumer would observe the change.
- Operational docs (`_doc/`, `_plan/`, journals, specs, decisions) do not
  belong here — the maintainer keeps them in a private control repo.

## History

Merged 2026-08-30 from `markc/bus`, `markc/mix`, `markc/cos` (git subtree —
full history preserved) and the former docs-only `markc/cosmix`. Those three
repositories are frozen at the merge commit.
